# Plan de implementación

## Objetivo acordado

Actualización del 15 de noviembre: completar la **paridad funcional** con la referencia local de `../../hive-sdk`, manteniendo APIs propias y un motor Rust compartido por npm/Python. Se excluyen gestión de proyectos, sus entidades, tools, contexto y scaffolding. No se incluyen compatibilidad de imports ni migración de datos del SDK anterior. No se admiten procesos auxiliares Node/Bun para implementar las capacidades. El gateway y las integraciones serán opcionales.

La [matriz de paridad](docs/parity.md) registra la referencia, implementación y validaciones pendientes. Esta ampliación reemplaza la exclusión anterior de canales, navegador, Office y gateway; las fases históricas de abajo conservan su evidencia original, pero **no certifican paridad completa**. Node >=24 y Python >=3.10 siguen siendo los mínimos de los bindings.

### Etapas de ampliación

1. Inventario fijado por hashes, matriz de capacidades y validación temprana de navegador/WhatsApp QR nativos.
2. Servicios embebidos compartidos, catálogo, credenciales, aislamiento y administración sin servidor.
3. Skills, contexto unificado, oráculos opt-in, proveedores/multimodalidad y MCP completo.
4. Harness durable con cola, leases, recuperación, pool Rust, scheduler y pruebas de aceptación.
5. Catálogo de tools, navegador/computer use, Office, imágenes, artefactos y medios.
6. Canales nativos y gateway opcional, conservando Desktop Rune.
7. Bindings, CLI nativa, CI y distribución en las seis plataformas previstas.

Cada fila incluida debe contar con implementación, contrato documentado y evidencia de prueba. Las filas parciales o pendientes impiden declarar completada la ampliación. Los registros de catálogo no equivalen a ejecutores instalados.

**Cierre de la Fase 1 (15 de noviembre).** Hecho y probado en Linux x64: tools de archivos/comandos/web ([native-tools](docs/native-tools.md)); [multimodalidad](docs/multimodal.md); [presets de proveedor y oráculo opt-in](docs/providers.md), [oráculo](docs/oracle.md); [artefactos](docs/artifacts.md); [Office](docs/office.md) (DOCX, XLSX, PPTX con estilo y PDF con estilos: bloques, tablas, enlaces, cabecera/pie con número de página); [navegador Obscura](docs/obscura.md) (sin Chromium ni Playwright) que el SDK **descarga y verifica solo** (última release a `~/.hive/bin`, SHA-256 publicado), probado contra un servidor MCP falso y contra el binario real (0.2.4 y 0.2.7); skills `obscura_browser`, `office_document_manager` y `pdf_report_writer` con las reglas validadas por prueba. **No se cierra en la Fase 1, y queda decidido así:** control del escritorio (`computer_use`, sin backend de SO y con modelo de autorización por definir); transformaciones de imagen (requieren un códec); OCR y PDF cifrados; y todo lo que exige cuentas, presupuesto o modelos reales (evaluación costo/calidad de proveedores, oráculo contra OpenRouter/Kev, canales en vivo). Ninguna fila de la matriz está completa: las de la fase quedan `partial` con sus pendientes escritos en `docs/parity/capabilities.json`.

**Cierre de la Fase 2 del plan de implementación (goal runner y administración).** Hecho y probado en Linux x64 (Rust, npm y Python): [goal runner](docs/goals.md) con `runGoal`/`run_goal`; [administración de la cola](docs/durable-queue.md) (filtros, reintento explícito, cancelación masiva, log de eventos atómico con cursor, purga); y el cron del gateway sobre el scheduler compartido ([native-scheduler](docs/native-scheduler.md)), con `cronJobs` como definición y `taskRuns` derivado. **Pendiente y declarado:** evaluar el goal runner con modelos reales (solo hay proveedores de guion), la ejecución del cron del gateway contra un escritorio real (solo pruebas con fixtures) y ~~los triggers cron de la API Objective~~ (hechos después: [triggers.md](docs/triggers.md)). Ninguna fila de la matriz está completa.

**Cierre de la Fase 3 del plan de implementación (gateway multiusuario y políticas).** Hecho y probado en Linux x64: [multiusuario del gateway](docs/gateway-multiuser.md) (tokens por usuario, superficie restringida, store/cola/scheduler por usuario, aislamiento entre inquilinos incluido WebSocket); [políticas y hooks](docs/policy.md) (evaluador nativo, hooks y aprobación humana en npm/Python y política con aprobación por WebSocket en el gateway); [resiliencia del proveedor](docs/resilience.md); y administración de tareas sobre el kernel compartido con validación previa. **Políticas por usuario, registro de auditoría de decisiones y [límites de tasa](docs/rate-limits.md) (opt-in)** añadidos después. **Pendiente y declarado:** ~~interfaz de aprobación en el escritorio~~ (hecha después en `hive-desktop-rune`, sin probar con un gateway real), inicio de sesión por usuario que entregue su token, y probarlo con el escritorio y modelos reales. Ninguna fila de la matriz está completa.

Avance de servicios/tools: [vault cifrado embebido](docs/embedded-secrets.md) con claves explícitas, CAS, tombstones, rotación atómica y bindings; [tools Rust de archivos, comandos y web](docs/native-tools.md) bajo políticas optativas. La reapertura entre Node/Python usa bases aisladas. Siguen pendientes la resolución autorizada de credential_ref por integración, aprovisionamiento compartido de claves, HTTP genérico y extracción HTML; estos avances no completan las etapas 2 y 5.

Avance de harness: la [cola durable y el pool Rust](docs/durable-queue.md) ya ofrecen ejecución opcional, límites por lane/cola, fencing, presupuesto raíz y recuperación/reconciliación entre procesos y lenguajes. Los [grupos y pruebas por tarea](docs/durable-delegation.md), la [integración local del gateway](docs/gateway-durable-delegation.md) y el [scheduler embebido](docs/native-scheduler.md) amplían ese contrato. Siguen pendientes goal runner, proof packets generales, bus/administración y la migración completa del cron del gateway; no se marca completada la paridad del harness completo.

Evidencia histórica de la primera entrega: [avance al 10 de noviembre](docs/progress-2026-11-10.md). Su aceptación funcional original: [validación con el proyecto local](docs/local-project-validation.md), con ejemplo offline ejecutable y procedimiento live con presupuesto explícito. La primera prueba lee solo el manifiesto del proyecto; la conexión al backend y la evaluación con modelos reales siguen pendientes.

## Fase 1 — Corte vertical y control de gasto

- [x] Workspace Rust y bindings napi-rs / PyO3.
- [x] Ciclo compartido de conversación, modelo, tools y respuesta.
- [x] Cliente HTTP Rust compatible con OpenAI y proveedores personalizados.
- [x] Tools síncronas/asíncronas en ambos lenguajes.
- [x] Eventos, métricas de uso y resultados de parada explícitos.
- [x] Límites de llamadas y detector configurable de resultados repetidos.
- [x] Pruebas del motor y de ambos bindings, incluido transporte HTTP local.
- [x] Validar JSON Schema mediante un motor Rust compartido: schemas al registrar y argumentos antes del callback; errores estructurados, referencias incluidas, rechazo sin efectos externos y presupuesto de intentos.
- [x] Reservar salida antes de llamadas, reconciliar uso y conservar reservas inciertas. Añadir deadline global persistido que impide nuevos despachos y limita HTTP activo; callbacks cooperativos.

Validación inicial: 13 pruebas Rust, 7 npm y 7 Python; estas últimas ejecutadas en Python 3.10 y 3.15. Build de ambos bindings y Clippy sin advertencias en Linux x64. Ver `docs/validation.md`. Esta fase no equivale al SDK público completo.

## Fase 2 — Streaming y proveedores

- [x] Exponer eventos incrementales mediante iteradores asíncronos en npm y Python: texto, fragmentos de tools, uso y resultado final, con reanudación de checkpoints.
- [x] Mover el transporte a I/O asíncrono Rust, reutilizar clientes y abortar HTTP activo mediante token de cancelación; cola acotada y límites de respuesta.
- [x] Añadir Anthropic Messages nativo para texto y tools del cliente, JSON/SSE, uso de caché y checkpoints, compartiendo el transporte y presupuestos.
- [x] Añadir Gemini GenerateContent nativo para texto y tools, JSON/SSE, firmas durables y uso de thoughts/caché sin duplicación.
- [ ] Ampliar capacidades de proveedores conservando contratos neutrales; no simular compatibilidad con APIs que tienen semánticas distintas.
  - [x] Salida estructurada explícita mediante JSON Schema, validación final Rust, mappings nativos JSON/SSE y persistencia/paridad; sin reescritura del schema ni solicitudes de reparación. Ver `docs/structured-output.md`.
- [x] Probar timeout/cancelación y cierre del socket, respuestas parciales, fragmentos UTF-8, ensamblado de tools y errores de protocolo sin reintentos.
- [x] Limitar salida por llamada/turno mediante el contrato neutral y los parámetros modernos o legacy del adaptador compatible, sin fallback con otra solicitud.
- [x] Acotar contexto en bytes JSON antes de inferir; detener u omitir turnos antiguos completos sin llamadas auxiliares ni borrar historial.
- [x] Añadir presupuesto BPE local con codificación explícita persistida junto al modelo, margen y estimación de entrada antes de inferir, sin consultas auxiliares.
- [ ] Añadir perfiles de framing por proveedor, presupuesto acumulado de entrada y ventana combinada de entrada/salida; seguir midiendo costo/calidad por tarea. El conteo neutral y el total reportado aún no son un techo monetario.
  - [x] Perfiles explícitos con los constructores JSON de los adaptadores, carga acumulada conservadora, reserva persistida y ventana combinada con cap explícito; comprobación antes del despacho, reapertura y paridad npm/Python.
  - [ ] Ampliar evaluación por tarea de costo/calidad del contexto seleccionado; no atribuir ahorro ni precisión de un modelo remoto a fixtures locales.
    - [x] Harness determinista de contexto completo, descarte, resumen y recuperación; 3 tareas, costo acumulado/auxiliar, respuestas esperadas y reporte reproducible `evals/context-contract.json`.

Aceptación: contratos de eventos y cancelación equivalentes en Rust, npm y Python; medición de llamadas/tokens por tarea y calidad de resultado.

## Fase 3 — HiveDB, sesiones y contexto

- [x] Integrar HiveDB directamente por crate Rust, fijado a un commit remoto; sin bindings npm/Python ni rutas locales. Publicación del crate en crates.io pendiente por dependencia Git.
- [x] Persistir historial, configuración del motor, uso y razones de parada por inquilino/agente/sesión. Callbacks y configuración de credenciales del proveedor no se serializan.
- [x] Reclamar sesiones con comparación de versión antes de efectos externos; marcar interrupciones y exigir recuperación explícita, sin repetir tools automáticamente.
- [x] Persistir checkpoints y ledger de tools dentro del turno, con identidad de ejecución y operación.
- [x] Reanudar explícitamente sin reiniciar uso ni detector de repetición; bloquear operaciones inciertas hasta reconciliación.
- [x] Reconstruir la selección de contexto bajo presupuesto de bytes al reanudar, preservando sistema, turno activo y pares tool/resultado.
- [x] Añadir resumen y recuperación de contexto; hacer explícito el costo de cualquier llamada auxiliar.
  - [x] Fuentes con SHA-256, búsqueda local acotada, resumen proporcionado y recuperación de turnos completos; proyección bajo bytes/BPE, instalación CAS entre turnos, reapertura y APIs npm/Python.
  - [x] Generación explícita de resúmenes mediante callbacks y agentes auxiliares con ledger, reconciliación y uso del mismo presupuesto raíz; evaluación del contrato por tarea con fixtures. La evaluación de calidad/costo de modelos remotos permanece abierta en fase 2.
    - [x] Kernel Rust de claims/resultados, fuente ligada por hash y costo atribuible por permisos, sin usar diferencias de snapshots globales; persistencia raíz formato 4 y pruebas de incertidumbre/reapertura.
    - [x] Callbacks npm/Python, propagación del ámbito por delegación/checkpoints, cancelación y paridad Node → Python → Node; sesiones atribuidas formato 5.
    - [x] Evaluación del contrato por tarea con fixtures reproducibles; la evaluación de calidad/ahorro de modelos remotos sigue abierta en fase 2.
- [x] Exponer el ID durable de la tool como clave de idempotencia para integraciones; guardar resultados y checkpoint de manera atómica. La idempotencia del sistema externo sigue siendo responsabilidad de esa integración.

Aceptación parcial disponible: reapertura, ámbitos por inquilino, conflictos, checkpoints, ledger, contexto acotado, BPE local, perfiles JSON, entrada acumulada, ventana combinada, recuperación local, resúmenes explícitos presupuestados y reanudación. Hay evaluación reproducible por tarea con fixtures. Sigue abierta la evaluación con modelos remotos. Los callbacks se registran otra vez al reabrir; no se ofrecen garantías exactly-once sobre sistemas externos.

## Fase 4 — MCP y delegación

- [x] MCP stdio y Streamable HTTP para revisiones 2025, catálogo acotado, validaciones compartidas, mismos presupuestos de tools y reconciliación de respuestas inciertas. Ver `docs/mcp.md`.
- [x] Delegación a agentes del mismo proceso, con presupuesto global compartido entre padre e hijos, profundidad limitada y cancelación propagada.
  - [x] Objetivo explícito dentro del proceso, contador/reservas atómicos Rust, consumo compartido de tools y modelos, profundidad y cancelación heredadas, cierre del ámbito de hijos y paridad de bindings.
  - [x] Presupuesto raíz durable coordinado con checkpoints: reserva/claim y liquidación/respuesta en transacciones HiveDB; identidad y profundidad persistidas, reapertura y reconciliación sin reiniciar saldos.
- [x] Verificación explícita mediante callbacks, veredictos estrictos, caché por clave y reconciliación durable sin repetir efectos. Revisores opt-in consumen el mismo presupuesto y registran referencia de evaluación/justificación; evaluación de fixtures en `evals/verifier-contract.json` sin modelos de pago.
- [x] Triggers recurrentes o proactivos separados del ciclo de un turno; desactivados por defecto y sin reiniciar presupuestos de un objetivo.
  - [x] Kernel Rust para eventos, fecha UTC e intervalos con coalescencia, deduplicación y claims/resultados durables bajo el presupuesto raíz.
  - [x] Bindings de callbacks, watcher opt-in y pruebas de cancelación/paridad de los triggers en npm/Python. Reapertura Node → Python → Node tras cierre real del proceso, sin repetir el callback.

Aceptación: ninguna ruta de delegación, MCP, revisión o reintento puede omitir los límites del objetivo raíz.

## Fase 5 — Distribución pública

- [x] Reducir copias mediante handles Rust, alternativas públicas e I/O durable interno en workers; medir intercambio e impacto local en el event loop con historial y concurrencia. Las APIs síncronas explícitas se conservan como compatibilidad; la medición no certifica rendimiento de producción.
  - [x] Handle propietario para agentes en memoria, rollback del turno, consultas de contexto y exportación explícita del historial en npm/Python; medición reproducible de bytes frente al puente anterior.
  - [x] Handles para sesiones durables sin snapshots completos en el ciclo; workers para begin/resume/check/claim/advance/reject/commit, cancelación con join, guards y medición reproducible del puente con HiveDB real.
  - [x] Factories asíncronas de store/agente/objetivo y alternativas explícitas de ámbitos, inspección, recuperación, resolución, cierre, planes y cancelación de objetivo; guards y join con limpieza de recursos tardíos. Ver `docs/async-storage.md`.
  - [x] Migrar checks/inspecciones y claims/finalizaciones internos de Objective/Trigger a rutas asíncronas, proteger ámbitos desde antes del claim y medir un fixture durable concurrente con historial en npm/Python. Ver `docs/objective-runtime.md`; los constructores y métodos síncronos explícitos todavía pueden bloquear.
- Publicar crates del motor y paquetes npm/PyPI con versiones coordinadas.
- Compilar y probar Linux x64 GNU/musl y ARM64 GNU, macOS x64/ARM64 y Windows x64; producir paquetes opcionales npm y wheels compatibles.
  - [x] Selector npm y generador de paquetes opcionales por plataforma, sin binario de desarrollo dentro del paquete principal; versión coordinada y metadata del build.
  - [x] Construcción release local de ambos bindings y primer wheel abi3 instalado en entornos Python 3.10 y 3.15 sin Rust/Bun en PATH.
  - [x] GNU Linux x64 actual reconstruido el 10 de noviembre en manylinux 2.28; siete instalaciones aisladas: Node 24/26 y Python 3.10/3.15 locales, Node 24 en Debian y Python 3.10/3.15 en manylinux limpio. Factories asíncronas y contabilidad durable comprobadas; hashes de fuentes/artefactos en `evals/distribution-linux-x64-gnu.json`. No sustituye la matriz ni las instalaciones desde registros.
  - [x] Linux x64 musl construido en musllinux 1.2 con compilador nativo musl; cuatro consumidores limpios pasan (Node 24/26 en Alpine y Python 3.10/3.15 en musllinux). ELF, fuentes y avisos Cargo verificados; reporte `evals/distribution-linux-x64-musl.json`. El wheel incluye libgcc_s y su atribución queda pendiente por separado.
  - [ ] Ejecutar matriz completa; GNU x64 no certifica musl, ARM64, macOS ni Windows.
- Verificar instalación en máquinas limpias sin Rust ni Bun; probar Node >=24 y Python >=3.10 en la matriz soportada.
- Añadir CI de compilación, paridad, empaquetado y release; publicar únicamente artefactos que hayan pasado sus pruebas.
  - [x] Workflows de regresión y builds de seis plataformas preparados y validados con actionlint; mínimo Node 24.
  - [x] Rust 1.95 comprobado en Linux x64 con check del workspace/all-targets y 14 pruebas de handles/schema; job MSRV dedicado preparado en CI.
  - [x] Inventario offline ligado a Cargo.lock, textos con SHA-256/procedencia y avisos dentro de previews npm/wheel; check reproducible y bloqueo de generación de nativos públicos incompletos.
  - [ ] Cerrar las cinco revisiones de atribución/avisos registradas en `third_party/inventory.json`; el inventario sigue marcado incomplete.
    - MIT de OpenAI incorporado para el código vendorizado de tiktoken-rs con cadena offline de submódulo/fuente; su revisión pendiente se acota a assets. 225 documentos, sin habilitar publicación.
  - [ ] Resolver la atribución y avisos de libgcc_s copiado por Maturin al wheel musl, con paquete APK/hashes en `third_party/runtime-review.json`; esta biblioteca adicional no está cubierta por el inventario Cargo.
  - [ ] Ejecutarlos en el repositorio destino, completar licencias y conectar publicación a los artefactos verificados.
- Completar la guía de inicio para instalaciones nuevas y retirar `private: true` de npm solo al tener los binarios listos.

**Cierre posterior (tanda de «terminar el SDK»).** Hecho y probado en Linux x64: transformaciones de imagen ([artifacts.md](docs/artifacts.md)); PDF cifrados RC4/AES-128/AES-256 ([office.md](docs/office.md)); triggers cron de `Objective` ([triggers.md](docs/triggers.md)); límite de fallos de login por cuenta ([rate-limits.md](docs/rate-limits.md)); WhatsApp QR opt-in sin probar con un teléfono ([whatsapp-qr.md](docs/whatsapp-qr.md)); aprobaciones por A2UI en el chat web, por texto en los canales y con botones en Slack ([policy.md](docs/policy.md), [channels.md](docs/channels.md)); login que entrega un token propio a los usuarios no propietarios ([gateway-multiuser.md](docs/gateway-multiuser.md)); razonamiento de ida y vuelta, interruptores de NVIDIA, llamadas en texto y saneado de huérfanos ([providers.md](docs/providers.md)); `tools/list_changed` y refresco de herramientas MCP ([mcp.md](docs/mcp.md)); hilos, comandos con barra y botones de Slack; y en el escritorio Rune la aprobación, la elección de cámara y el cambio de aparatos en caliente. **Sigue pendiente y declarado:** todo lo que exige cuentas, claves, modelos o decisiones tuyas (voz embebible, `computer_use`, OCR, WhatsApp QR real), la validación en seis plataformas y la publicación.

Aceptación: instalación real de los paquetes construidos en la matriz objetivo y todos los criterios de las fases anteriores. No sustituir ni publicar sobre el paquete anterior de forma implícita.
