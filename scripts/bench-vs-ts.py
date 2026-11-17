#!/usr/bin/env python3
"""Measure the Rust gateway against the TypeScript Hive gateway on the same (isolated, neutral) profile.

Both are started one after the other on the same port, from their own fresh copy of the profile, with nothing
that can reach a real service (see `--prepare-test-home ... --neutral`). What is measured is what the gateway
itself controls: start-up time, memory at rest and under load, and the speed of local HTTP routes. NOT measured
(and not claimed): the latency of a model reply, its quality, or its cost: those belong to the provider.

A number is only a comparison when both sides do the same work, so:
  * both serve the SAME data: the TypeScript Hive re-seeds tools, skills and models when it starts, so it is run
    once first and the profile it leaves behind is what every measured run starts from (for both sides);
  * "ready" means a real data route answers (`/api/agents` with its agents), not just `/health` (the TypeScript
    side answers `/health` while it is still initializing);
  * every route is classified before any ratio is drawn:
      comparable       same number of items and a reply size within +-33 %  -> a ratio is meaningful;
      payload_differs  same items but a reply more than 33 % bigger or smaller -> reported with MB/s, no ratio;
      liveness         `/health`: a small fixed JSON on both sides, a probe of the HTTP layer itself;
      not_comparable   the two do different work (NOT_COMPARABLE) -> each side alone, never a ratio.
    `/api/version` is not measured (SKIPPED): the TypeScript one calls the public npm registry on every request.

    python3 scripts/bench-vs-ts.py --profile COPY --rust target/release/hive-gateway-rust --ts ../hive \\
        --runs 10 --load-seconds 10 --out evals/bench-vs-ts.json

Linux only (reads /proc). Needs `bun` for the TypeScript side.
"""
import argparse, http.client, json, os, platform, shutil, signal, socket, statistics, subprocess, sys, tempfile, threading, time

PORT = 18790
LOAD_PATHS = ["/health", "/api/agents", "/api/providers", "/api/tools", "/api/skills", "/api/setup/status"]
# Not loaded at all: every call of the TypeScript `/api/version` is a real HTTPS request to the public npm registry
# (no cache), so load-testing it would hammer a service that is not ours. The Rust one answers a fixed local status.
SKIPPED = {"/api/version": "the TypeScript side asks the npm registry on every call; the Rust one answers a fixed local status"}
# Routes whose two implementations do different work: reported separately, never as a ratio.
NOT_COMPARABLE = {
    "/api/setup/status": "the TypeScript side reports configured/setupMode without checking the provider connection; the Rust one validates "
                         "coordinator, provider, model, address and that the key can be read, and says why when it cannot",
}


def free(port):
    with socket.socket() as s:
        return s.connect_ex(("127.0.0.1", port)) != 0


def wait_free(port, seconds=15):
    end = time.time() + seconds
    while time.time() < end:
        if free(port):
            return
        time.sleep(0.1)
    sys.exit(f"port {port} is busy: stop whatever is listening before measuring")


def tree(pid):
    """pid and all its descendants."""
    pids, todo = [], [pid]
    while todo:
        p = todo.pop()
        pids.append(p)
        try:
            with open(f"/proc/{p}/task/{p}/children") as f:
                todo += [int(x) for x in f.read().split()]
        except OSError:
            pass
    return pids


def memory_mb(pid):
    """Proportional set size of the process tree (shared pages are split, so a tree is not double counted)."""
    total = 0
    for p in tree(pid):
        try:
            with open(f"/proc/{p}/smaps_rollup") as f:
                total += sum(int(l.split()[1]) for l in f if l.startswith("Pss:"))
        except OSError:
            pass
    return total / 1024


def start(kind, args, profile_src):
    home = tempfile.mkdtemp(prefix=f"bench-{kind}-")
    shutil.rmtree(home)
    shutil.copytree(profile_src, home)
    env = dict(os.environ, HIVE_HOME=home, HIVE_PORT=str(PORT), HIVE_GATEWAY_CHILD="1")
    if kind == "rust":
        cmd, cwd = [args.rust, "start", "--skip-check"], None
    else:
        cmd, cwd = ["bun", "packages/cli/src/index.ts", "start", "--skip-check"], args.ts
    wait_free(PORT)
    t0 = time.perf_counter()
    proc = subprocess.Popen(cmd, cwd=cwd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
    ready = None
    while time.perf_counter() - t0 < 60:
        if proc.poll() is not None:
            sys.exit(f"{kind} exited during start-up")
        try:
            c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=2)
            c.request("GET", "/api/agents")
            r = c.getresponse()
            body = r.read()
            if r.status == 200 and json.loads(body).get("agents"):
                ready = time.perf_counter() - t0
                break
        except (OSError, ValueError):
            pass
        time.sleep(0.005)
    if ready is None:
        stop(proc, home)
        sys.exit(f"{kind} was not ready in 60 s")
    return proc, home, ready


def stop(proc, home, keep=False):
    try:
        os.killpg(proc.pid, signal.SIGTERM)
        proc.wait(timeout=10)
    except Exception:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except OSError:
            pass
    if not keep:
        shutil.rmtree(home, ignore_errors=True)


def seed_with_ts(args):
    """Run the TypeScript Hive once on a copy and keep what it leaves (it re-seeds tools, skills and models), so both
    sides are measured on the same data. Its master key is put back as a file, which the TypeScript side moves into
    the OS keyring on every start."""
    proc, home, _ = start("ts", args, args.profile)
    stop(proc, home, keep=True)
    key = os.path.join(args.profile, ".master.key")
    if os.path.exists(key):
        shutil.copy(key, os.path.join(home, ".master.key"))
    return home


def shape_of(path):
    """What a route answers right now: status, bytes and, for a list, how many items."""
    c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=10)
    c.request("GET", path)
    r = c.getresponse()
    body = r.read()
    items = None
    try:
        value = json.loads(body)
        lists = [v for v in value.values() if isinstance(v, list)] if isinstance(value, dict) else []
        items = len(lists[0]) if lists else None
    except ValueError:
        pass
    return {"status": r.status, "bytes": len(body), "items": items}


def ab(path, seconds, clients):
    """One route with several ApacheBench processes at once (keep-alive). A single `ab` tops out near 15 000 req/s
    by itself, which would make fast routes look equal; spreading the clients over processes keeps the client from
    being the limit. Totals are summed; the slowest p99 is reported."""
    procs = min(6, clients)
    each = max(1, clients // procs)
    cmd = ["ab", "-k", "-q", "-c", str(each), "-t", str(seconds), "-n", "10000000", f"http://127.0.0.1:{PORT}{path}"]
    running = [subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True) for _ in range(procs)]
    outputs = [p.communicate(timeout=seconds + 60)[0] for p in running]

    def num(out, label):
        for line in out.splitlines():
            if line.startswith(label):
                return float(line.split(":")[1].split()[0])
        return None

    def pct(out, q):
        for line in out.splitlines():
            if line.strip().startswith(f"{q}%"):
                return float(line.split()[1])
        return None

    rates = [num(o, "Requests per second") for o in outputs]
    if any(r is None for r in rates):  # ab died or was cut off: keep the evidence instead of a made-up number
        return {"rps": None, "mean_ms": None, "p50_ms": None, "p99_ms": None, "requests": 0, "failed": 1,
                "ab_output": "\n".join(o.strip()[-200:] for o in outputs)[:400]}
    rps = sum(rates)
    return {"rps": round(rps, 1), "mean_ms": round(procs * each * 1000 / rps, 3), "p50_ms": max(pct(o, 50) or 0 for o in outputs),
            "p99_ms": max(pct(o, 99) or 0 for o in outputs), "requests": int(sum(num(o, "Complete requests") or 0 for o in outputs)),
            "failed": int(sum((num(o, "Failed requests") or 0) + (num(o, "Non-2xx responses") or 0) for o in outputs))}


def load(seconds, clients):
    per_path = {p: ab(p, seconds, clients) for p in LOAD_PATHS}
    return {"per_path": per_path, "failed": sum(v["failed"] for v in per_path.values())}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--profile", required=True, help="neutral copy made with --prepare-test-home ... --neutral")
    ap.add_argument("--rust", required=True)
    ap.add_argument("--ts", required=True, help="directory of the TypeScript Hive")
    ap.add_argument("--runs", type=int, default=10)
    ap.add_argument("--clients", type=int, default=36, help="total concurrent clients, spread over up to 6 ab processes")
    ap.add_argument("--load-seconds", type=int, default=3, help="seconds of load per route")
    ap.add_argument("--settle", type=float, default=5.0)
    ap.add_argument("--out", default="evals/bench-vs-ts.json")
    args = ap.parse_args()
    result = {"clients": args.clients, "load_paths": LOAD_PATHS, "runs": args.runs, "impl": {}, "not_comparable": NOT_COMPARABLE, "skipped": SKIPPED}
    seeded = seed_with_ts(args)
    print(f"profile both sides start from (left by one TypeScript run): {seeded}", flush=True)
    args.profile = seeded
    for kind in ("rust", "ts"):
        result["impl"][kind] = {"startup_s": [], "idle_mb": [], "load": [], "load_peak_mb": [], "shapes": {}}
    # Alternate the order so neither side always benefits from a warm disk cache.
    for i in range(args.runs):
        for kind in (("rust", "ts") if i % 2 == 0 else ("ts", "rust")):
            proc, home, ready = start(kind, args, args.profile)
            time.sleep(args.settle)
            entry = result["impl"][kind]
            entry["startup_s"].append(round(ready, 3))
            entry["idle_mb"].append(round(memory_mb(proc.pid), 1))
            if not entry["shapes"]:
                entry["shapes"] = {p: shape_of(p) for p in LOAD_PATHS}
            if i < 3:  # load is slower; at most three repetitions per side
                entry["load"].append(load(args.load_seconds, args.clients))
                entry["load_peak_mb"].append(round(memory_mb(proc.pid), 1))
            stop(proc, home)
            print(f"{kind} run {i + 1}/{args.runs}: ready {ready * 1000:.0f} ms, idle {entry['idle_mb'][-1]} MB", flush=True)
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    json.dump(result, open(args.out + ".raw", "w"), indent=2)  # raw data first: a summary bug must not lose a long run
    med = lambda values: statistics.median([v for v in values if v is not None]) if any(v is not None for v in values) else None
    unmeasured = {}
    for kind, e in result["impl"].items():
        def stat(p, key, digits=None):
            value = med([l["per_path"][p][key] for l in e["load"]])
            return None if value is None else (round(value, digits) if digits is not None else round(value))
        paths = {p: {"rps": stat(p, "rps"), "mean_ms": stat(p, "mean_ms", 2), "p99_ms": stat(p, "p99_ms", 1),
                     "unmeasured_repetitions": sum(1 for l in e["load"] if l["per_path"][p]["rps"] is None)} for p in LOAD_PATHS}
        e["summary"] = {"startup_ms_median": round(med(e["startup_s"]) * 1000), "idle_mb_median": round(med(e["idle_mb"]), 1),
                        "load_peak_mb_median": round(med(e["load_peak_mb"]), 1), "load_failed": sum(l["failed"] for l in e["load"]),
                        "per_path": paths}
    shapes = {k: v["shapes"] for k, v in result["impl"].items()}
    def classify(path):
        r, t = shapes["rust"][path], shapes["ts"][path]
        if path in NOT_COMPARABLE:
            return "not_comparable"
        if r["status"] != 200 or t["status"] != 200:
            return "not_comparable"
        if path == "/health":
            return "liveness"
        if r["items"] != t["items"]:
            return "not_comparable"
        return "comparable" if 0.75 <= r["bytes"] / t["bytes"] <= 1.33 else "payload_differs"
    result["class"] = {p: classify(p) for p in LOAD_PATHS}
    shutil.rmtree(seeded, ignore_errors=True)
    result["footprint_bytes"] = {
        "rust_binary": os.path.getsize(args.rust),
        "ts_bundle_dist_hive_js": os.path.getsize(os.path.join(args.ts, "dist/hive.js")) if os.path.exists(os.path.join(args.ts, "dist/hive.js")) else None,
        "bun_runtime": os.path.getsize(shutil.which("bun")),
    }
    result["machine"] = {"os": platform.platform(), "cpu_count": os.cpu_count(), "python": platform.python_version(),
                         "bun": subprocess.run(["bun", "--version"], capture_output=True, text=True).stdout.strip(),
                         "ts_hive_version": json.load(open(os.path.join(args.ts, "package.json")))["version"]}
    os.makedirs(os.path.dirname(args.out) or ".", exist_ok=True)
    json.dump(result, open(args.out, "w"), indent=2)
    print(json.dumps({k: v["summary"] for k, v in result["impl"].items()}, indent=2))


if __name__ == "__main__":
    main()
