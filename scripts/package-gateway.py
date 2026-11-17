#!/usr/bin/env python3
"""Package an already-built `hive-gateway-rust` for one platform: archive + SHA-256 + manifest.

Building and smoke-testing each target happens in CI (.github/workflows/gateway.yml); this only checks that
the binary is really for the target it is labelled with, refuses stale or incomplete third-party notices when
the package is meant to be public, and writes:

    dist/gateway/hive-gateway-rust-<version>-<key>.tar.gz  (or .zip on Windows)
    dist/gateway/hive-gateway-rust-<version>-<key>.sha256

The archive holds the binary, LICENSE, THIRD_PARTY_NOTICES.txt and a short README for people who run the
gateway without npm or Python.
"""
import argparse, hashlib, io, json, pathlib, struct, sys, tarfile, tomllib, zipfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGETS = {
    "linux-x64-gnu": ("elf", 62, "hive-gateway-rust"),
    "linux-arm64-gnu": ("elf", 183, "hive-gateway-rust"),
    "darwin-x64": ("macho", 0x01000007, "hive-gateway-rust"),
    "darwin-arm64": ("macho", 0x0100000C, "hive-gateway-rust"),
    "win32-x64-msvc": ("pe", 0x8664, "hive-gateway-rust.exe"),
}
README = """hive-gateway-rust {version} ({key})

The local gateway of Hive SDK Rust: HTTP and WebSocket on 127.0.0.1, with the engine, tools, scheduler and
integrations. It needs no npm or Python.

It is a companion of an EXISTING Hive profile: it does not create one. Point it at the profile directory
(the one that holds data/hivedb):

    HIVE_HOME=/path/to/profile HIVE_PORT=18790 ./{binary} start
    HIVE_HOME=/path/to/profile ./{binary} --check-config      # validates it without calling any model
    ./{binary} --prepare-test-home SOURCE DEST                # an isolated copy for testing (Hive stopped)

Notes
- Linux needs libdbus-1 (and a Secret Service for provider keys kept in the OS keyring).
- Only builds listed in the release exist; there is no musl/Alpine build of the gateway.
- Licenses of every dependency: THIRD_PARTY_NOTICES.txt.
- Verify the download against the .sha256 next to the archive.
"""


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def check_binary(kind: str, machine: int, data: bytes) -> None:
    # Reject architecture/container mismatches rather than labelling a host binary as another target.
    if kind == "elf":
        ok = data[:4] == b"\x7fELF" and data[4] == 2 and data[5] == 1 and struct.unpack_from("<H", data, 18)[0] == machine
    elif kind == "macho":
        ok = struct.unpack_from("<I", data, 0)[0] == 0xFEEDFACF and struct.unpack_from("<I", data, 4)[0] == machine
    else:
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        ok = data[:2] == b"MZ" and struct.unpack_from("<I", data, pe)[0] == 0x4550 and struct.unpack_from("<H", data, pe + 4)[0] == machine
    if not ok:
        sys.exit(f"binary is not a {kind} executable for this target")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True, choices=sorted(TARGETS))
    parser.add_argument("--binary", required=True, help="path to the built hive-gateway-rust")
    parser.add_argument("--output", default="dist/gateway")
    parser.add_argument("--public", action="store_true", help="release build: notices must be complete and current")
    args = parser.parse_args()
    kind, machine, binary_name = TARGETS[args.target]
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    data = pathlib.Path(args.binary).read_bytes()
    check_binary(kind, machine, data)
    notices = (ROOT / "THIRD_PARTY_NOTICES.txt").read_bytes()
    inventory = json.loads((ROOT / "third_party/inventory.json").read_text())
    if inventory["cargo_lock_sha256"] != sha256((ROOT / "Cargo.lock").read_bytes()) or inventory["notices_sha256"] != sha256(notices):
        sys.exit("THIRD_PARTY_NOTICES_STALE: regenerate notices from Cargo.lock")
    if args.public and inventory["pending"]:
        sys.exit("THIRD_PARTY_REVIEW_REQUIRED: resolve inventory gaps before publishing binaries")
    files = {
        binary_name: (data, 0o755),
        "LICENSE": ((ROOT / "LICENSE").read_bytes(), 0o644),
        "THIRD_PARTY_NOTICES.txt": (notices, 0o644),
        "README.txt": (README.format(version=version, key=args.target, binary=binary_name).encode(), 0o644),
    }
    out = ROOT / args.output
    out.mkdir(parents=True, exist_ok=True)
    stem = f"hive-gateway-rust-{version}-{args.target}"
    top = stem + "/"
    if kind == "pe":
        archive = out / f"{stem}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            for name, (content, _) in files.items():
                z.writestr(top + name, content)
    else:
        archive = out / f"{stem}.tar.gz"
        with tarfile.open(archive, "w:gz") as t:
            for name, (content, mode) in files.items():
                info = tarfile.TarInfo(top + name)
                info.size, info.mode, info.mtime = len(content), mode, 0
                t.addfile(info, io.BytesIO(content))
    digest = sha256(archive.read_bytes())
    (out / f"{stem}.sha256").write_text(f"{digest}  {archive.name}\n")
    print(f"{archive.name} {digest}")


if __name__ == "__main__":
    main()
