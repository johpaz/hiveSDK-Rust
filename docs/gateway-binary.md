# El gateway como binario independiente (sin npm ni Python)

`hive-gateway-rust` es el servidor local (HTTP y WebSocket en `127.0.0.1`) que une el motor Rust, las herramientas, el planificador y las integraciones. Se puede usar **sin npm ni Python**: es un ejecutable. Así lo lanza el escritorio de Rune.

## Cómo se produce
- `.github/workflows/gateway.yml` (también reutilizable por `release.yml`) compila el binario en **cinco plataformas**: `linux-x64-gnu`, `linux-arm64-gnu` (dentro de manylinux_2_28, para un glibc antiguo), `darwin-x64`, `darwin-arm64` y `win32-x64-msvc`.
- Cada build pasa una **prueba de humo** del binario compilado (`--help`; rechazo limpio de un perfil vacío) y se empaqueta con `scripts/package-gateway.py`: `tar.gz` (o `zip` en Windows) con el binario, `LICENSE`, `THIRD_PARTY_NOTICES.txt` y un `README.txt`, más un `.sha256`. El script rechaza un binario que no sea de la plataforma indicada y, en builds públicas, avisos de terceros desactualizados o incompletos.
- En un tag `v*`, `release.yml` adjunta los archivos y un `SHA256SUMS` al release de GitHub, junto a los paquetes de npm y PyPI. Un ensayo (`workflow_dispatch`) construye y empaqueta sin publicar.

## Cómo se usa
```bash
HIVE_HOME=/ruta/al/perfil HIVE_PORT=18790 ./hive-gateway-rust start
HIVE_HOME=/ruta/al/perfil ./hive-gateway-rust --check-config      # valida sin llamar a ningún modelo
./hive-gateway-rust --prepare-test-home ORIGEN DESTINO            # copia aislada para pruebas
```
Es el **compañero de un perfil Hive existente** (una carpeta con `data/hivedb`): no crea uno desde cero y se niega con un mensaje claro si no lo encuentra. Quien lo despliegue debe aportar ese perfil.

## Límites (dichos de frente)
- **Sin build musl/Alpine.** El gateway enlaza `libdbus` (llavero del sistema); en Linux necesita `libdbus-1` en tiempo de ejecución.
- **Linux es el único sistema ejecutado de verdad** por el equipo; los builds de macOS y Windows los verifica por primera vez el CI (los backends de escritorio de esas plataformas solo se habían comprobado de tipos).
- El workflow **no se ha ejecutado todavía**: no hay remoto de GitHub. Lo verificado en local es el script de empaquetado (archivo, suma, rechazo de plataforma errónea) y la prueba de humo, con el binario de Linux.

## crates.io
No se publica: `hive-agent-core` y el gateway dependen de `hivedb-core`, hoy una dependencia **Git** (`rev` fijo), y `cargo publish` la rechaza. Para publicar hace falta primero publicar `hivedb-core` (o aceptar otra vía de empaquetado) y fijar `repository`/`readme` en los `Cargo.toml`. Mientras tanto, desde Rust se usa como dependencia por Git o por ruta.
