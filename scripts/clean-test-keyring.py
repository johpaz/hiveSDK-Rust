#!/usr/bin/env python3
"""Borra del llavero las claves maestras `hive/master-key:*` que dejaron las pruebas con copias temporales.

El Hive TypeScript mueve la clave maestra de cada carpeta HIVE_HOME al llavero del sistema, con un nombre derivado de la
ruta. Cada copia temporal de una prueba o benchmark deja así una entrada huérfana. Este script NO borra nada por defecto:
muestra qué borraría. Para borrar de verdad: `--delete`.

Qué toca (y qué nunca):
  * solo entradas `service=hive`, `account=master-key:<hash>` creadas desde `--since` (por defecto, la fecha de hoy);
  * nunca la de una carpeta que exista hoy en tu casa: `~/.hive`, `~/.hive-dev`, `~/.hivecode`, `~/.hive-backup-*`…
    (se calcula el nombre que le correspondería a cada `~/.hive*` y se salta);
  * el secreto no se lee ni se imprime: la salida de `secret-tool` se filtra en memoria.

    python3 scripts/clean-test-keyring.py                       # solo muestra
    python3 scripts/clean-test-keyring.py --since 2026-11-16    # desde otra fecha
    python3 scripts/clean-test-keyring.py --delete              # borra
"""
import argparse, datetime, glob, hashlib, os, re, subprocess, sys


def entries():
    out = subprocess.run(["secret-tool", "search", "--all", "service", "hive"], capture_output=True, text=True).stdout
    for block in re.split(r"\n(?=\[/\d+\]\n)", out):
        block = "\n".join(l for l in block.splitlines() if not l.startswith("secret ="))  # el secreto nunca se usa
        created = re.search(r"^created = (.*)$", block, re.M)
        account = re.search(r"^attribute\.account = (master-key:\S+)$", block, re.M)
        if created and account:
            yield created.group(1), account.group(1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--since", default=datetime.date.today().isoformat(), help="fecha AAAA-MM-DD (por defecto, hoy)")
    ap.add_argument("--delete", action="store_true")
    args = ap.parse_args()
    protected = {
        "master-key:" + hashlib.sha256(os.path.realpath(d).encode()).hexdigest()[:16]: d
        for d in glob.glob(os.path.expanduser("~/.hive*"))
    }
    todo = [(c, a) for c, a in entries() if c >= args.since and a not in protected]
    skipped = [(c, a) for c, a in entries() if c >= args.since and a in protected]
    print(f"desde {args.since}: {len(todo)} entradas huérfanas; {len(skipped)} protegidas (carpetas que existen): {[protected[a] for _, a in skipped]}")
    for created, account in todo[:5]:
        print("  ", created, account)
    if len(todo) > 5:
        print(f"   … y {len(todo) - 5} más")
    if not args.delete:
        print("Modo de solo lectura. Para borrar: --delete")
        return
    for _, account in todo:
        subprocess.run(["secret-tool", "clear", "service", "hive", "account", account], check=False)
    print(f"borradas {len(todo)}")


if __name__ == "__main__":
    sys.exit(main())
