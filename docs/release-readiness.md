# Pendientes para publicación — 2026-11-10

**Actualización del 15 de noviembre:** esta fotografía corresponde al alcance anterior. La ampliación vigente está en [paridad](parity.md). Los binarios históricos no validan el código nuevo. Cargo.lock incorpora los features de servicios/navegador; el inventario de avisos debe regenerarse antes de empaquetar o publicar. En este entorno no se pudo completar porque faltan los archives de core-foundation 0.9.4 y security-framework 2.11.1 y la red/caché global está restringida. No se relajaron los gates de publicación.

El SDK se construye desde cero, con APIs propias y bases nuevas, y npm/Python comparten el motor Rust. La compatibilidad o importación del SDK anterior no forma parte del alcance. La publicación todavía requiere cerrar distribución, atribución, CI/release y validaciones con modelos reales. Este documento es una fotografía de estado; PLAN.md conserva los criterios de aceptación.

Para iniciar la prueba funcional sin publicar, seguir [el procedimiento del proyecto local](local-project-validation.md). La [fotografía de avance](progress-2026-11-10.md) distingue consumidor offline comprobado, proveedor real y conexión pendiente con el backend.

## Estado al cierre de la implementación local (15 de noviembre)

Las Fases 0–4 del plan quedaron implementadas y probadas **en Linux x64** (detalle y límites de cada una en [paridad](parity.md), [goals](goals.md), [política](policy.md), [multiusuario](gateway-multiuser.md), [canales](channels.md), [MCP](mcp.md), [Obscura](obscura.md), [Office](office.md) y [resiliencia](resilience.md)). Regresión local: **Rust 525** pruebas, **npm 205** (Node 26), **Python 200** (3.10), `cargo clippy --all-features --all-targets -D warnings`, `cargo fmt --check`, `tsc --strict` y **MSRV 1.95.0** (`cargo check --workspace --all-targets --locked`) limpios. Se comprobó que las skills (`crates/hive-core/skills`) viajan en el paquete del crate y `goals.mjs` en el tarball de npm. El CI ahora ejecuta además `cargo test -p hive-agent-core --all-features` (antes solo tres suites con features) y instala poppler para los cruces de PDF.

**Lo que NO está cerrado, y por qué no lo puedo cerrar yo:**

| Pendiente | Por qué |
| --- | --- |
| Cinco revisiones de avisos de terceros y `libgcc_s` del wheel musl | Son decisiones de licencias/atribución (textos de aviso ausentes en el crate publicado, excepción de runtime GPL). Un script no puede dar por «limpio» un componente; requiere tu revisión. `scripts/third-party-notices.py --check` sigue diciendo `incomplete` |
| `hivedb-core` fijado por Git (bloquea `cargo publish`) | Hay que publicarlo en un registro o decidir el empaquetado permitido; es una decisión de publicación |
| Binarios de las seis plataformas y `evals/distribution-*.json` | Los binarios históricos no validan el código nuevo; hay que ejecutar `native.yml` en CI (primera ejecución remota aún no hecha) |
| Repositorio, commits y credenciales npm/PyPI | Los commits son tuyos; `release.yml` solo debe correr sobre artefactos verificados |
| Evaluación con modelos y cuentas reales | Proveedores, goal runner con modelos reales, Telegram/Discord/Slack/WhatsApp reales y Obscura fuera de Linux x64 solo se probaron con servidores falsos (Obscura sí con el binario real 0.2.4/0.2.7 en Linux x64) |
| Funcionalidad pendiente | WhatsApp QR sin probar con un teléfono real y sin revisar su `wacore-libsignal` para distribuir (ver [whatsapp-qr.md](whatsapp-qr.md)), voz y realtime embebibles (Gemini Live real), `computer_use` (necesita decisión explícita), OCR (necesita elegir un modelo) y PDF con contraseña de propietario o clave pública, protocolo nativo de Ollama, tope de salida derivado de la ventana de contexto, medios y modo webhook en Slack, otros transportes MCP, Office (fechas y estilos de Excel, encabezados de Word, notas del orador) y la prueba conjunta de las aprobaciones (escritorio y canales) con un gateway, modelos y cuentas reales |

## Ya disponible

- Tools/callbacks propios del lenguaje, proveedores OpenAI Chat/Anthropic Messages/Gemini GenerateContent, JSON/SSE y salida estructurada validada en Rust.
- Presupuestos por turno y objetivo, reservas durables, deadline, límites de profundidad/tools/modelos y detección configurable de resultados repetidos.
- HiveDB, checkpoints/ledger, CAS y recuperación explícita sin repetir automáticamente efectos inciertos.
- Contexto por bytes/BPE, perfiles de proveedor, resumen explícito, recuperación local y atribución del costo auxiliar.
- MCP, delegación, verificaciones opt-in y triggers desactivados por defecto, con control durable interno en workers.
- Factories y APIs asíncronas, npm Node >=24 y Python >=3.10; MSRV Rust 1.95 comprobado en Linux x64.
- Regresión local actual: 209 Rust, 158 npm en Node 24 y 153 Python en 3.10. Hay pruebas de paridad, tarballs/wheels de preview e inventario de avisos; las plataformas de publicación siguen pendientes según la matriz siguiente.

## 1. Avisos de terceros

El catálogo Cargo conserva 332 dependencias y 225 documentos únicos. Sigue marcado incomplete, con cinco revisiones. Hay además una revisión separada del runtime libgcc_s incluido por Maturin en el wheel musl, registrada en third_party/runtime-review.json. El generador de paquetes nativos públicos rechaza el estado Cargo incompleto y npm conserva private:true. No hay publicación PyPI configurada; resolver los cinco paquetes Cargo no cierra automáticamente la atribución de bibliotecas adicionales del wheel.

| Pendiente | Estado de investigación |
| --- | --- |
| htmlescape 0.3.1 | Se comparó el crate con el commit de upstream que declara esa versión/licencias: coinciden 10 de 12 archivos; src/decode.rs y tests/test.rs difieren. El árbol no contiene textos LICENSE. Falta resolver texto y procedencia sin atribuir al crate un commit que no reproduce todo su contenido. |
| tiktoken-rs 0.12.1 | El catálogo incluye MIT del wrapper y MIT de OpenAI mediante una cadena comprobada de árbol, .gitmodules, gitlink y fuente del crate. Cinco assets coinciden byte a byte con hashes del script upstream. encoder.json cambia de bytes, pero su valor JSON coincide con el archivo de origen. Falta resolver la atribución de estos assets; la incorporación del MIT de código no cierra esa revisión. |
| rustls-platform-verifier-android 0.1.1 | Publicación sin texto de aviso ni commit VCS. No aparece en los grafos filtrados de la matriz soportada; continúa pendiente en el catálogo conservador. |
| winapi-i686-pc-windows-gnu 0.4.0 | Mismo problema de avisos/procedencia. No aparece en los grafos filtrados; Windows previsto usa MSVC. |
| winapi-x86_64-pc-windows-gnu 0.4.0 | Igual; no se elimina del catálogo solo por no aparecer en esos grafos. |

La investigación está en third_party/review-evidence.json, ligada al hash de Cargo.lock. Los seis grafos normales/build de los bindings, excluyendo dev, contienen htmlescape y tiktoken-rs. Es una estimación de dependencias mediante cargo metadata --filter-platform, no una inspección de qué código queda enlazado. No se cambió el gate ni se cerraron avisos con esa evidencia parcial.

## 2. Artefactos actuales para seis plataformas

| Plataforma | Pendiente |
| --- | --- |
| Linux x64 GNU | Reconstrucción actual manylinux 2.28 completada el 10 de noviembre: tres artefactos privados en dist y siete instalaciones aisladas correctas, con factories asíncronas y presupuesto durable. Evidencia en evals/distribution-linux-x64-gnu.json; falta instalación desde registros tras publicación. |
| Linux x64 musl | Build actual musllinux 1.2 y cuatro consumidores limpios correctos: Node 24/26 en Alpine y Python 3.10/3.15 en musllinux. Reporte evals/distribution-linux-x64-musl.json. Falta atribución del libgcc_s incluido en el wheel e instalación desde registros tras publicación. |
| Linux ARM64 GNU | Ejecutar build manylinux ARM64 y consumidores. |
| macOS x64 | Ejecutar runner/build nativo e instalaciones. |
| macOS ARM64 | Igual en ARM64. |
| Windows x64 MSVC | Ejecutar build nativo e instalaciones; verificar empaquetado/cierre del proceso en ese entorno. |

Hay workflows preparados para las seis. GNU y musl x64 cuentan con evidencia local del código actual en versiones mínimas/recientes y contenedores limpios. Falta ejecutar las otras cuatro plataformas y la matriz remota completa. Estas compilaciones/instalaciones no certifican las demás.

## 3. Dependencia Git y crates.io

hivedb-core se fija a un commit remoto. Cargo package no permite publicar ese crate del motor con una dependencia exclusivamente Git sin resolver su distribución. Falta publicar versiones apropiadas de HiveDB en un registro compatible o elegir y validar un empaquetado permitido. Los consumidores de npm/wheel precompilados no necesitan compilar HiveDB; este punto afecta al build y a la publicación de los crates Rust acordados en el plan.

## 4. Repositorio, ejecución remota y publicación

La carpeta todavía no es un repositorio Git y no tiene remoto. gh auth status confirma que la CLI no tiene sesión iniciada. Los YAML pasan actionlint, pero no hay evidencia de jobs remotos de toda la matriz.

Falta establecer repositorio destino, conectar acceso, ejecutar jobs y recoger sus artefactos verificados. Después: confirmar nombres/permisos de npm/PyPI, coordinar versiones del core/bindings/facade y seis paquetes opcionales, conectar el release exclusivamente a artefactos probados, publicar en orden y comprobar instalación desde los registros. Los nombres de desarrollo actuales son @johpaz/hive-sdk-rust y johpaz-hive-sdk-rust, versión 0.1.0; no se verificó titularidad/disponibilidad en registros. private:true se retira al cumplir sus condiciones, no antes.

## 5. Calidad/costo con modelos reales

Los harnesses actuales usan fixtures y contadores sintéticos. Falta definir tareas representativas, modelo/versiones y presupuesto explícito, y ejecutar la comparación de contexto completo, descarte, resumen y recuperación con proveedores reales. Registrar uso reportado/facturado disponible, llamadas auxiliares, latencia, respuestas correctas y detenciones. Los límites/perfiles ya existen; esta evaluación sigue abierta y no se sustituye por un benchmark local de CPU/I/O.

## 6. Alcance de proveedores y guía final

La línea de ampliar capacidades de proveedores sigue abierta sin criterios adicionales concretos en el plan. Ya hay texto/tools/streaming/JSON en tres protocolos. Falta convertir el alcance restante en aceptación verificable o acordar qué se reserva para otra versión; no dar esa línea por cumplida sin ese detalle.

La guía final debe reconciliar los ejemplos, nombres/artefactos publicados e instrucciones para bases nuevas con el resultado real de la matriz. Canales, navegador, office y gateway están expresamente fuera de la primera versión del núcleo; no se cuentan como bloqueos nuevos de esa entrega.

## Orden de cierre

1. Resolver avisos comunes y documentar el alcance real de cada artefacto.
2. Resolver la distribución de HiveDB para los crates Rust.
3. Preparar el repositorio destino y ejecutar/reconstruir la matriz actual.
4. Completar evaluación real de calidad/costo con presupuesto explícito.
5. Cerrar guía/versiones y publicar únicamente los artefactos validados; probar su instalación desde los registros.

No se asigna un porcentaje de avance ni una fecha de publicación a partir del número de checkboxes. La parte funcional y la preparación de release tienen criterios distintos y los bloqueos anteriores siguen visibles en PLAN.md.

## Binarios del gateway
`gateway.yml` y `release.yml` publican `hive-gateway-rust` (cinco plataformas, con SHA256SUMS) para quien no use npm ni Python. Pendiente: ejecutarlos por primera vez en CI; sin build musl; crates.io bloqueado por `hivedb-core` (Git). Ver [gateway-binary.md](gateway-binary.md).
