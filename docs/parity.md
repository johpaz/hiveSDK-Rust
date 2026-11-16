# Paridad funcional con Hive TypeScript

La ampliación está **incompleta**. La referencia es el SDK local @johpaz/hive-sdk 0.7.3, fijado por hashes de 295 archivos y 33 puntos de entrada en [ts-reference.json](parity/ts-reference.json). La [matriz de capacidades](parity/capabilities.json) diferencia implementación parcial, pendientes y exclusiones. Ningún recuento de pruebas equivale por sí solo a completar una fila.

## Decisiones de alcance

Se conserva un motor Rust y APIs propias en Rust, Node y Python; no se exige compatibilidad de imports ni migración de datos anteriores. Gestión de proyectos, sus entidades, tools, contexto y generación de proyectos quedan excluidos. Las integraciones y gateway son opcionales. No se usarán procesos auxiliares Node/Bun para implementarlas; los runtimes de los bindings y el Bun utilizado para ejecutar la referencia TS en tests son consumidores/fixtures, no implementaciones auxiliares.

## Implementado en esta ampliación

- [Vault de secretos embebido](embedded-secrets.md): cifrado autenticado, CAS, tombstones y rotación atómica con claves del host; solo referencias descriptivas en el catálogo.
- [Tools nativas](native-tools.md): archivos, comandos permitidos y web GET/búsqueda configurables, con ejecutores Rust y bindings. Los contratos web que necesitan sockets siguen pendientes de ejecución en un entorno que permita loopback.
- [Multimodalidad](multimodal.md): adjuntos de imagen, audio y documento en turnos de usuario para OpenAI chat, Anthropic y Gemini, con rechazos explícitos, asignación fija de tokens por adjunto e historial durable. Probado con proveedores deterministas; sin ejecución contra APIs reales ni comprobación de capacidades por modelo.
- [Proveedores](providers.md): catálogo de presets para los 16 proveedores de la referencia y perfiles de cable (normalización de nombres de tools con mapa inverso, ajustes de esquema, `tool_choice`, rechazo de modelos sin tools, cabecera de sesión). Sin reintento sin herramientas por decisión de diseño; verificado solo con servidores locales.
- [Oráculo opcional](oracle.md): cliente del protocolo de decisiones Jev/Kev con validación estricta, fallo en abierto, cooldown y desconfianza, cargado como llamada auxiliar al Objective raíz. Sin planificador del bucle; verificado solo con servidores locales.
- [Artefactos](artifacts.md): almacén por inquilino con bytes en disco, SHA-256 verificado en cada lectura, retención explícita y herramientas `artifact_*`; dimensiones de imagen leídas de la cabecera (PNG/JPEG/GIF/WebP). Sin transformaciones de imagen ni entrega por HTTP/canales; verificado solo en local.
- [Office](office.md): lectura y escritura de DOCX, XLSX, PPTX y texto PDF en el sandbox de archivos (presentaciones con tema, patrón y diseños; PDF extraído y comparado con poppler sobre PDFs de cuatro productores), con contenedor ZIP acotado, XML estricto y límites de tamaño/filas/hojas/tiempo; contrastado con archivos de python-docx/openpyxl/python-pptx y abierto en LibreOffice. Sin PDF, notas, imágenes ni comparación con la referencia.
- [Administración de la cola, eventos y purga](durable-queue.md) y [cron del gateway sobre el scheduler compartido](native-scheduler.md): listado con filtros, reintento explícito, cancelación masiva, log de eventos atómico con cursor y purga de jobs terminados; `cronJobs` sigue siendo la definición, el kernel el motor y `taskRuns` una vista derivada, con jobs inciertos que nunca se reproducen.
- [Resiliencia del proveedor](resilience.md): reintentos opt-in solo para peticiones que demostrablemente no se procesaron (conexión no abierta, 429/503) con backoff y `Retry-After`, dentro del timeout y deadline, y circuit breaker que falla sin enviar; nada incierto se repite. Probado con servidores locales guionizados, no con proveedores reales.
- [Goal runner](goals.md): bucle durable planificar → delegar → sintetizar → revisar sobre la cola y los grupos, con `tick` idempotente y concurrente, rondas acotadas, planes/revisiones inválidos que fallan en vez de adivinarse, jobs inciertos que bloquean sin reintento y cancelación; probado en Rust, npm y Python con jobs completados a mano, **sin modelos reales**.
- [Navegador Obscura](obscura.md): el navegador del SDK es Obscura (`obscura mcp` por stdio), sin Chromium ni Playwright; 31 herramientas `obscura_*` (evaluate y cookies opt-in), `allowed_hosts` con comprobación del destino final, descarte de la sesión tras timeout/cancelación/caída y skills que prohíben otros navegadores. Probado contra un servidor MCP falso **y contra el binario real Obscura 0.2.4 y 0.2.7** (37 herramientas con esquemas idénticos, capturas y PDF reales); **el SDK descarga la última release** a `~/.hive/bin` si no hay ninguna, verificando su SHA-256 publicado.

- [Cola durable y pool Rust](durable-queue.md): prioridad/FIFO, lanes, idempotencia atómica, leases/fencing, reintentos seguros, cancelación con espera de callbacks y presupuesto raíz compartido. Bindings y prueba de salida abrupta Node → Python → Node, sin repetir el efecto de fixture.

- [Grupos de especialistas y síntesis durable](durable-delegation.md): sesiones estables, fan-out atómico, fan-in único, incertidumbre explícita y pruebas ligadas al resultado exacto. Bindings Node/Python y recuperación entre procesos.
- [Delegación durable del gateway](gateway-durable-delegation.md): consultas individuales y en paralelo con pool Rust, raíz durable por turno, narración y continuación del checkpoint padre. Caída de proceso sin repetir inferencia validada con fixtures.
- [Scheduler embebido](native-scheduler.md): calendario Rust compartido con el gateway, fechas/intervalos/cron de cinco o seis campos, publicación atómica, misfires explícitos y productores optativos Node/Python/Rust.
- Catálogo embebido de diez tipos de entidad con schemas compartidos, ámbitos por tenant, CAS, paginación, tombstones y recreación con revisión creciente. Bindings async sobre el store existente; no requiere HTTP.
- Búsqueda BM25 para memoria y descubrimiento de capacidades, con límites explícitos y sin inferencias.
- Importación nativa SKILL.md/YAML, metadatos de workflow, límites de archivo y rechazo de duplicados/referencias a tools inexistentes.
- Ocho tools con ejecutores Rust para memoria y descubrimiento. El Agent loop conserva el control de presupuesto/ledger.
- Cliente CDP Rust y bindings para navegar, evaluar scripts, click, escritura, extracción y screenshot. Se conecta a una página existente; lanzamiento automático y tools de navegador siguen pendientes.
- Operaciones MCP de resources/prompts, también en servidores sin tools. Se conserva la política de solicitud única, límites y ausencia de replay.
- Exportación HiveAgents en ambos bindings sobre el adaptador nativo existente.
- Primitiva de patch versionado compartida con administración del gateway; el resto de sus servicios todavía requiere extracción.
- Pruebas de comparación directa TS/Rust para escenarios de memoria y parseo de skills; prueba de intercambio Node → Python → Node y checks CI de features opcionales.

Evidencia histórica de la primera entrega, ligada a sus hashes: [parity-foundation.json](../evals/parity-foundation.json).

Evidencia de la ampliación de cola/pool: [queue-extension.json](../evals/queue-extension.json).

Evidencia de scheduler: [scheduler-extension.json](../evals/scheduler-extension.json).

Evidencia de vault y herramientas nativas: [native-tools-secrets-extension.json](../evals/native-tools-secrets-extension.json). Incluye límites de ejecución web y pruebas entre Node/Python; no certifica multimodalidad ni completa la matriz.

Evidencia de gateway/objetivos por turno: [gateway-delegation-extension.json](../evals/gateway-delegation-extension.json).

Evidencia de grupos/pruebas durables: [delegation-extension.json](../evals/delegation-extension.json).

Contratos y ejemplos: [servicios embebidos](embedded-services.md), [navegador](native-browser.md).

## Validación y límites

La comparación directa TS/Rust pasó para CRUD/upsert de memoria, aislamiento, búsqueda de una palabra completa, borrado/recreación y metadatos/cuerpo de una skill YAML válida. No compara timestamps, búsqueda por subcadena, ranking, ejecución de workflows, compatibilidad de APIs ni cierre de procesos. El fixture TS de memoria cierra la base y sale explícitamente porque su import mantiene handles nativos.

Las pruebas nuevas de servicios y MCP stdio en Rust y de los bindings npm/Python pasan con proveedores deterministas. Las declaraciones TypeScript compilan en modo strict. Se ejecutaron build del workspace, formato y Clippy con todos los features/targets, sin advertencias. Las pruebas de protocolo de navegador pasaron en una ejecución inicial; su repetición depende de poder abrir sockets locales.

La regresión completa no está certificada en este sandbox. El intento Rust se detuvo en anthropic_transport con EPERM al abrir sockets. Node ejecutó 164 pruebas con aislamiento desactivado: 132 pasaron y 32 fallaron al escuchar en loopback; seis pruebas nuevas están entre las aprobadas. El barrido Python amplio encontró restricciones de sockets y agotó su timeout; no se contabiliza como aprobado. Las cinco pruebas nuevas sí pasaron en Python 3.10, con un heartbeat externo del event loop para evitar el bloqueo de notificaciones de workers observado también con asyncio.to_thread sin el SDK.

No se han hecho pruebas con cuentas o proveedores remotos ni se han enviado mensajes reales.

En la primera entrega, la regeneración del inventario quedó bloqueada por archives macOS ausentes y restricciones de red. El check actual del inventario pasa para Cargo.lock (429 dependencias y 282 documentos únicos), pero conserva cinco paquetes pendientes de revisión y el estado incomplete. No se han habilitado publicaciones.

## Reproducir

```bash
python3 scripts/build-dev.py
cargo test -p hive-agent-core --all-features --test services --test mcp --test browser --locked
node --test --test-isolation=none packages/npm/test/services.test.mjs packages/npm/test/mcp-catalog.test.mjs
PYTHONPATH=packages/python/python python3.10 -m unittest discover -s packages/python/tests -p test_services.py -v
PYTHONPATH=packages/python/python python3.10 -m unittest discover -s packages/python/tests -p test_mcp_catalog.py -v
PYTHONPATH=packages/python/python python3.10 tests/services-parity.py
python3 scripts/compare-ts-parity.py
python3 scripts/snapshot-ts-reference.py --check
```

La comparación requiere el checkout TS original, Bun para esa referencia y sus dependencias instaladas. --source permite señalar otro checkout; --check exige que coincida con la instantánea acordada. No actualizar la referencia automáticamente ante drift. Los escenarios utilizan bases temporales y no leen credenciales ni el perfil real.

## Trabajo que continúa pendiente

Completar extracción de servicios, resolución autorizada de referencias a secretos y aprovisionamiento de claves, compiler/contexto integrado, bundles de skills adaptados, oráculos (planificador del bucle), multimodalidad restante (capacidades por modelo, salida de medios, ejecución real; ver multimodal.md), catálogo completo de proveedores, manager/hot reload/transportes MCP restantes, bus general y administración/reconciliación de grupos/cola/pool, proof packets generales y goal runner, administración/migración completa del scheduler y triggers cron de Objective, catálogo restante de tools, Office, imágenes/artefactos, canales nativos y gateway multi-tenant completo, CLI y distribución validada en seis plataformas.

Para WhatsApp QR se identificaron implementaciones Rust candidatas, entre ellas [whatsapp-rust](https://docs.rs/whatsapp-rust/0.7.0/whatsapp_rust/). No se añadió una dependencia ni se comprobó vinculación/reconexión; WhatsApp Cloud no se contabilizará como sustituto. Es una capacidad pendiente, no una imposibilidad demostrada.
