# Distribución de binarios

Requisitos del consumidor: Node >=24 o Python >=3.10. No se requiere Rust ni Bun cuando se instala un artefacto precompilado compatible. Los paquetes todavía son privados/no publicados; los nombres y la versión coordinada son `@johpaz/hive-sdk-rust` y `johpaz-hive-sdk-rust`, versión `0.1.0`.

## npm

El paquete principal contiene API JavaScript, declaraciones TypeScript y selector de plataforma. No incluye `native/hive.node` de desarrollo, compiladores, scripts de descarga ni un paso de compilación al instalar. Usa dependencias opcionales de la misma versión:

| Sufijo del paquete nativo | Rust target |
|---|---|
| `linux-x64-gnu` | `x86_64-unknown-linux-gnu` |
| `linux-x64-musl` | `x86_64-unknown-linux-musl` |
| `linux-arm64-gnu` | `aarch64-unknown-linux-gnu` |
| `darwin-x64` | `x86_64-apple-darwin` |
| `darwin-arm64` | `aarch64-apple-darwin` |
| `win32-x64-msvc` | `x86_64-pc-windows-msvc` |

Los nombres completos añaden el sufijo a `@johpaz/hive-sdk-rust-`. Linux distingue glibc/musl mediante el reporte de Node. Una plataforma no soportada produce un error explícito; no descarga ni compila código como fallback. En el checkout, el binding local sigue permitiendo desarrollo.

`node scripts/package-native.mjs --target linux-x64-gnu` empaqueta un binario previamente construido en `target/<triple>/release/`. `--binary` permite elegirlo explícitamente; el script comprueba formato/arquitectura y guarda target, perfil y SHA-256. Esta comprobación no certifica por sí sola la compatibilidad de libc. No usar un build de la distribución anfitriona como sustituto de manylinux: el primer build local falló en Debian por requerir glibc 2.38.

`node scripts/test-npm-install.mjs <principal.tgz> <nativo.tgz>` crea un consumidor temporal e instala offline sin scripts. Prueba factories asíncronas de store/agente/objetivo, tools, reapertura HiveDB y contabilidad de un objetivo durable; además conserva una comprobación de objetivo local. El PATH del consumidor contiene solo la instalación Node. Antes de publicar se suministra explícitamente el tarball nativo porque aún no existe en el registro; la resolución desde npm requiere una prueba adicional después de publicar.

## Python

Maturin >=1.16 construye wheels `cp310-abi3`, con API Python y biblioteca PyO3 dentro del mismo archivo. El mínimo del builder se alinea con la versión validada y los metadatos license-files; no es un requisito runtime. No hay dependencia runtime de paquetes Python para el motor. `maturin build --release --locked` se ejecuta desde `packages/python`. Linux de distribución requiere construir y auditar con manylinux/musllinux; un wheel `linux_x86_64` construido localmente no certifica compatibilidad con otras distribuciones.

`tests/installed-smoke.py` se ejecuta con el intérprete del entorno donde se instaló el wheel, usando `-I`, sin PYTHONPATH. Comprueba factories asíncronas, tools, reapertura HiveDB y contadores de objetivo durable/local. El mismo wheel abi3 se prueba en Python 3.10 y 3.15.

`python scripts/test-python-install.py <wheel>` crea ese entorno temporal y limita su PATH al directorio del intérprete. El 10 de noviembre se reconstruyeron ambos bindings desde el código actual en manylinux 2.28, con Rust 1.99.1 y Maturin 1.16.0. Pasaron siete consumidores: Node 24.15/26.13 y Python 3.10.22/3.15.0 locales, Node 24.22.2 en Debian bookworm, Python 3.10.22/3.15.2 en un contenedor manylinux limpio. Los símbolos GLIBC de ambos bindings no exceden 2.28. Esto no certifica las otras cinco plataformas.

`dist/build-info.json` y [el reporte GNU x64](../evals/distribution-linux-x64-gnu.json) registran hashes/tamaños de los tres artefactos, imagen por digest, compiladores, avisos y fixtures de consumo. El [manifiesto de fuentes](../evals/distribution-linux-x64-gnu-sources.json) fija 387 archivos verificados dentro del builder, sin fuentes Rust sobrantes. Se reutilizó la caché del contenedor de build; los consumidores de contenedor fueron nuevos. Las pruebas son de instalación/flujo funcional, no la regresión completa ejecutada contra esos binarios portables.

Linux x64 musl también está construido desde esa instantánea, en musllinux 1.2 con Rust 1.99.1 nativo musl, Maturin 1.16.0 y RUSTFLAGS=-C target-feature=-crt-static. Pasaron cuatro instalaciones en contenedores nuevos: Node 24.22.2/26.13.0 en Alpine 3.24.4 y Python 3.10.22/3.15.2 en musllinux sobre Alpine 3.22.8. Se verificaron ELF x64, dependencia libc.musl-x86_64.so.1 y ausencia de versiones de símbolos GLIBC. Evidencia en [el reporte musl](../evals/distribution-linux-x64-musl.json), su [manifiesto](../evals/distribution-linux-x64-musl-sources.json) y dist/build-info-musl.json. Los artefactos GNU se conservan y el facade es compartido. ARM64 GNU, macOS x64/ARM64 y Windows x64 siguen pendientes.

## CI y publicación pendiente

Rust 1.95.0 se ha comprobado con `cargo +1.95.0 check --workspace --all-targets --jobs 2 --locked --offline` y 14 pruebas de handles durables/schema en Linux x64. El toolchain predeterminado se conserva y el target de esta comprobación es separado. CI tiene un job MSRV con Node 24/Python 3.10; no equivale a certificar el MSRV en las otras cinco plataformas.

Los previews incluyen THIRD_PARTY_NOTICES.txt y THIRD_PARTY_INVENTORY.json. El generador offline conserva expresiones y procedencia desde Cargo.lock/archivos .crate verificados, más suplementos a commits exactos; `--check` detecta drift y `--require-complete` rechaza pendientes. Hay 332 dependencias, 225 documentos y cinco revisiones abiertas: [detalle](../third_party/README.md). El estado incomplete se incluye explícitamente. El generador de paquetes nativos verifica hashes y bloquea `private:false` con pendientes. No hay publicación PyPI/crates configurada que esa evidencia autorice automáticamente.

El wheel musl incluye además johpaz_hive_sdk_rust.libs/libgcc_s-0cd532bd.so.1, copiado y reparado por Maturin desde libgcc-14.2.0-r6 del builder. El inventario Cargo no cubre esa biblioteca. Su [revisión adicional](../third_party/runtime-review.json) fija paquete APK, imagen, hash original y hash dentro del wheel. Se conserva literalmente la expresión de licencia del paquete Alpine; no se infiere de ella una licencia por archivo ni una excepción aplicable. Faltan resolver esa atribución y sus avisos antes de publicar el wheel. npm enlaza libgcc_s del sistema sin incluir esa biblioteca separada en su tarball.

Se verificó el contenido de los tarballs npm y del wheel portable actual, incluyendo los tres archivos bajo dist-info/licenses y todos los módulos Python esperados, sin módulos sobrantes ni más de un binding del paquete. Los bytes de avisos/inventario coinciden con el workspace y los manifests npm conservan private:true. Los consumidores instalan sin Rust/Bun en PATH. Los previews locales debug anteriores no se usan como evidencia de portabilidad. Los artefactos actuales siguen sin publicar.

`ci.yml` verifica regresión, paridad de checkpoints, Rust/Clippy y contrato TypeScript en Node 24/26 y Python 3.10/3.15. `native.yml` prepara la matriz anterior, construye GNU Linux en manylinux 2.28, musl en musllinux 1.2 y las demás plataformas en sus runners nativos. Prueba instalaciones antes de subir artefactos. Se ejecuta manualmente o como workflow reutilizable; no publica en registros.

Los workflows asumen que esta carpeta es la raíz del repositorio. Si se aloja dentro de un monorepo, adaptar sus rutas y checkout antes de activarlos. La CLI de GitHub no tiene sesión iniciada en el entorno de desarrollo; todavía no hay evidencia de ejecuciones remotas de esta matriz.

Queda pendiente completar licencias/avisos de dependencias enlazadas, construir y probar toda la matriz, verificar instalaciones desde registros y coordinar la publicación de crates. La dependencia Git fijada de HiveDB también debe resolverse antes de publicar el crate del motor en crates.io. `private: true` se conserva hasta tener los artefactos y validaciones necesarios. La preparación de workflows no equivale a haber ejecutado sus jobs.

Referencias de implementación: [Maturin Action](https://github.com/PyO3/maturin-action), [runners disponibles](https://github.com/actions/runner-images) y [actionlint](https://github.com/rhysd/actionlint).
