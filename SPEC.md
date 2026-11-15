# Especificación inicial — 0.1.0 de desarrollo

Ampliación del 15 de noviembre: [servicios embebidos](docs/embedded-services.md), [navegador CDP opcional](docs/native-browser.md) y [matriz de paridad con TS](docs/parity.md). El alcance nuevo incluye capacidades del harness anterior, con APIs propias y sin gestión de proyectos ni migración de datos. La matriz identifica las capacidades que todavía no están implementadas o verificadas.

## Arquitectura

`hive-agent-core` contiene el motor y transporte HTTP. `hive-agent-napi` y `hive-agent-python` convierten entradas/salidas y liberan el hilo anfitrión durante HTTP. Los wrappers despachan las acciones del motor y ejecutan callbacks; no deciden presupuestos, orden de tools ni terminación.

La máquina de estados es serializable. Los agentes en memoria usan un handle Rust propietario: conserva el confirmado y un estado de trabajo clonado al iniciar el turno; las transiciones intercambian comandos y acciones sin enviar snapshots completos al anfitrión. Confirmar una acción terminal publica el estado; errores/cancelación/cierre del iterador descartan el trabajo. El camino durable usa un handle Rust que conserva revisión y checkpoint; sus transiciones del ciclo ejecutan I/O en workers sin transportar snapshots completos. Se ofrecen factories open y alternativas públicas asíncronas; el control durable interno de Objective/Trigger también usa workers. Los métodos síncronos explícitos de compatibilidad pueden bloquear. Los puentes internos no son un formato público de sesión, no validan snapshots externos y no deben exponerse como endpoint ni aceptar estado de terceros. Ver [alcance y medición](docs/engine-handles.md).

## Contratos públicos actuales

- `Agent`: modelo, system prompt opcional, tools, proveedor opcional, máximo de llamadas, política y hook de eventos. Un agente mantiene una conversación en memoria.
- `Tool`: nombre único, descripción, JSON Schema de objeto, callback y opt-out del detector de repeticiones de resultados ejecutados. Entradas y resultados deben ser valores JSON. Los schemas se compilan al registrar; los argumentos se validan antes de cada ejecución, incluida una tool pendiente al reanudar un checkpoint anterior.
- `Provider.complete(request)`: recibe modelo, mensajes y tools; devuelve contenido/calls y uso opcional. Puede ser síncrono/asíncrono. `Provider.stream` es opcional y devuelve un iterador de deltas/uso terminado por un único evento `reply`. Se admite un proveedor solo-stream; `run` lo consume internamente. Sin método stream, el iterador del agente entrega ciclo de vida y resultado, sin fabricar tokens.
- `OpenAICompatible`: cliente asíncrono Rust reutilizable por adaptador, configurado y validado al construirlo. `baseUrl`/`base_url`, API key opcional, timeout de solicitud (60 s) y `maxResponseBytes`/`max_response_bytes` (16 MiB). Soporta Chat Completions JSON y SSE; sin redirecciones ni reintentos automáticos.
- `Anthropic`: Messages API nativa para texto y tools del cliente, JSON/SSE, con transporte compartido. API version 2026-10-21 por defecto, configurable; default de salida 4096 cuando el request neutral no trae cap. Usa Bearer y anthropic-version. Convierte system/bloques/tools y agrupa resultados. Entrada reportada suma input ordinario y caché; uso SSE acumulado solo se confirma con el reply. No se serializan credenciales. Ver [contrato y alcance](docs/anthropic.md).
- `run(prompt)`: devuelve `{status, content, stop_reason, stats}`. `status` es `completed` o `stopped`. Una parada no se presenta como éxito del objetivo.
- `Gemini`: GenerateContent de Developer API, JSON/SSE, texto y functionCall completos. Base URL v1beta, key en x-goog-api-key y modelo en la ruta. El cap neutral mapea a maxOutputTokens. Input incluye prompt y tool-use prompt; output candidatos y thoughts. Caché ya incluida en prompt no se suma otra vez. Ver [alcance](docs/gemini.md).
- `resume()`: disponible para un agente construido con `resume: true` / `resume=True` y store. Continúa el mismo turno desde el checkpoint, sin prompt nuevo ni reinicio de presupuesto.
- `stream(prompt)` y `resumeStream()` / `resume_stream()`: iteradores asíncronos de acciones, `text_delta`, `tool_call_delta`, `usage` y un último evento `result`. El evento proveedor `reply` lo consume el motor, no se publica como resultado del agente. Consumir hasta el final o cerrar explícitamente el iterador.
- `cancel()`: solicita cancelación del turno activo y devuelve bool; no significa que toda función anfitriona ya haya terminado. npm acepta además `{signal}` en run/resume/stream; Python acepta `cancel_event`. HTTP se aborta; otros callbacks requieren cooperación o espera de su terminación.
- `stats`: llamadas al modelo, tools ejecutadas, `tool_rejections`, tokens reportados, llamadas sin medición y `reserved_output_tokens`. Los campos nuevos tienen default cero en snapshots anteriores. La reserva no equivale a gasto confirmado. No se estiman costos monetarios.
- `messages`: copia del historial confirmado. Las credenciales del adaptador no se guardan como configuración del motor; los mensajes siguen siendo contenido de la aplicación.
- `SessionStore`: directorio HiveDB y tenant por defecto `default`. Opcional en `Agent`; si se usa, agent ID y session ID son obligatorios. `forTenant` / `for_tenant` comparte la base. `inspect` consulta operaciones/checkpoints; `resolveTool` y `resolveModel` (snake_case en Python) aportan resultados reconciliados con revisión e ID de operación. `recover` abandona un turno explícitamente. `close` libera el handle.
- Contexto de tool: IDs `run_id`, `execution_id`, `tool_call_id`; IDs durables con store, sin él run/execution son null. npm añade `context.signal` no enumerable. Python habilita el segundo argumento con `with_context=True`: `ToolContext` conserva IDs como dict serializable y añade el atributo no persistido `context.cancel_event`.
- `onEvent` / `on_event`: recibe acciones `model`, `tool`, `tool_rejected`, `complete`, `stopped`. Es observación previa al despacho, con backpressure; no ejecuta otro modelo por defecto. Un hook que falla aborta el turno.
- `close()`: impide nuevas llamadas; rechaza cierre durante una ejecución activa. No cancela tareas en curso.

Los contratos JSON internos usan snake_case en ambos lenguajes. Las opciones públicas de npm usan camelCase, excepto el objeto `policy`, cuyo esquema compartido usa snake_case. Python usa snake_case. Los wrappers son responsables de traducir metadata de tools al motor.

`policy.structured_output: {name, schema}` configura salida JSON final. El motor valida offline el schema y la respuesta final sin tools; texto inválido detiene con `invalid_structured_output`, conservando mensaje y uso conocido, sin llamada de reparación. La respuesta válida conserva el texto original. Los adaptadores envían el schema sin reescribir y los presupuestos de entrada lo cuentan. Compatibilidad de modelos/subconjuntos, streaming y reapertura en [salida estructurada](docs/structured-output.md).

## Transiciones y límites

ModelReply y Message añaden provider_state opcional, inicialmente `{provider:'gemini',version:1,parts:[...]}`. El motor valida coherencia con content/tool_calls antes de mutar y conserva partes/firmas completas en historial y checkpoints. Texto thought no es contenido visible; sus partes permanecen en estado si existen. OpenAI/Anthropic rechazan ese estado antes de conectar. Registros sin el campo siguen legibles; no hay migración implícita entre protocolos.

```text
idle → model → complete
         ↓
        tool → model
         ↓       ↓
       stopped ← presupuesto / repetición
```

| Límite | Valor inicial | Comportamiento |
|---|---|---|
| Llamadas al modelo | 8 por `run` | Si la última llamada pide tools sin margen para continuar, detener antes de ejecutarlas. |
| Intentos de tool | 16 por `run` | `tool_calls + tool_rejections`; comprobar también dentro de un lote. |
| Observaciones idénticas | 2 por `run` | Comparar nombre + argumentos + resultado como JSON, sin considerar ID de llamada. Detener antes de seguir consultando al modelo. |
| Tokens totales reportados | Sin límite por defecto | Si se configura, parar al alcanzar el umbral o ante uso ausente. Puede sobrepasarse dentro de una llamada ya facturada. |
| Salida por llamada | Sin límite por defecto | `max_output_tokens`, solicitado al proveedor antes de inferir. |
| Salida agregada | Sin límite por defecto | `max_total_output_tokens`: reservar salida restante, reconciliar uso y bloquear continuación al agotarse. |
| Tiempo del turno | Sin límite por defecto | `max_run_ms`: deadline absoluto guardado una vez; no se reinicia en checkpoints. |
| Entrada JSON | Sin límite por defecto | `max_context_bytes`: bytes UTF-8 neutrales; detener o retirar turnos antiguos completos según `context_overflow`. |
| Estimación BPE de entrada | Sin límite por defecto | `input_token_budget`: codificación explícita, máximo y margen sobre JSON neutral antes de despachar. |

Las observaciones se cuentan en toda la ejecución, no solo consecutivamente. La detección identifica repetición exacta, no equivalencia semántica ni prueba ausencia de progreso. Tools de polling pueden desactivarla individualmente; otros límites permanecen.

Razones estables: `model_call_limit`, `tool_call_limit`, `repeated_tool_result`, `token_limit`, `usage_unavailable`, `output_limit`, `deadline_exceeded`, `context_limit`, `invalid_structured_output`. `content` es null para paradas; el diagnóstico está en `stop_reason` y `stats`. Un agente detenido es terminal en esta fase: no recibe un prompt nuevo ni reinicia sus límites. Se puede finalizar un checkpoint terminal todavía pendiente de confirmación.

Una respuesta final no requiere verificador adicional. Esto acredita fin de la conversación, no éxito verificable de una tarea externa. La comprobación independiente es opt-in; no se ofrece un bucle autónomo que reintente hasta conseguir un veredicto favorable.

`Objective.verify(key, value, callback, options)` ofrece esa comprobación explícita y no reinicia el ciclo Agent. Cada claim consume un intento raíz; exige veredicto booleano estructurado, guarda entrada/opciones/ID y reutiliza resultados confirmados. Un claim incierto no se repite: `resolveVerification` / `resolve_verification` aporta el resultado con fencing por execution ID. El contexto permite revisores bajo el mismo presupuesto y profundidad, solo con evidencia de evaluación/justificación explícitas. Referencias y límites en `docs/verification.md`.

## Preparación de contexto

`max_context_bytes` admite entero positivo o null; `context_overflow` es `stop` por defecto o `drop_oldest_turns`, que exige límite de bytes o tokens locales. Se miden bytes UTF-8 de JSON compacto `{model,messages,tools}` neutral, incluidos escapes y schemas. No son tokens ni bytes del vendor. `ModelRequest.context` opcional informa `serialized_bytes` y `dropped_messages`; el HTTP nativo no envía esa metadata.

El modo stop produce `context_limit` antes de reservar salida o despachar. La alternativa elimina turnos user→assistant/tools/resultados completos desde el más antiguo, solo de la solicitud. Sistema y turno activo quedan completos. Si no caben, se detiene; nunca se corta un schema, cadena o par de tool. El historial y ledger permanecen íntegros, incluido un resultado de tool conocido que impida continuar por su tamaño. Al reanudar se obtiene la misma selección sin modificar contadores. No hay inferencia auxiliar ni garantía de ventana de tokens. Ver [contexto](docs/context.md).

## Estimación BPE de entrada

`input_token_budget` opcional contiene `encoding` (`cl100k_base` / `o200k_base`), `max_tokens` positivo y `overhead_tokens` no negativo (cero por defecto, menor que el máximo). Cuenta BPE ordinario del JSON neutral completo en orden Rust, sin metadata; añade margen explícito. Codificaciones desconocidas, campos inválidos y límites incoherentes fallan al construir; no hay adivinación por nombre o fallback. El reporte `context.token_estimate` contiene encoding, serialized_tokens, overhead_tokens y total_tokens. Se persiste la configuración con el modelo y se reconstruye el mismo reporte al reanudar. No se mezcla con el uso real de stats.

El límite local excedido también produce `context_limit` y usa la misma selección de turnos completos. Con ambos límites activos, la selección debe caber en bytes y BPE; el conteo no se suma por fragmentos. No hay despacho ni reserva nueva si la entrada obligatoria no cabe. El deadline se vuelve a comprobar después del cómputo local. No representa el framing real del proveedor ni garantiza ventana o factura. Ver [tokens](docs/tokens.md).

## Concurrencia y fallos

- Una ejecución activa por agente. Agentes independientes pueden correr concurrentemente; tools de un lote se ejecutan secuencialmente.
- Cada turno confirma historial solo al completar o detenerse por política. Errores de protocolo/proveedor/hook conservan el historial anterior y no generan reintento automático.
- Fallos o resultados no serializables de tools se convierten en feedback `{error:{message}}`. No incluir credenciales en mensajes de excepciones personalizados.
- IDs duplicados/vacíos y tools desconocidas son errores de protocolo del lote. Argumentos inválidos, incluidos valores que no son objetos, generan feedback por llamada antes de ejecutar su callback. Una tool válida previa de un lote puede haberse ejecutado antes del rechazo de otra.
- Python ejecuta callbacks síncronos en threads; los asíncronos permanecen en el event loop. Sus callbacks deben respetar esta semántica. npm ejecuta callbacks JS en el event loop; trabajo CPU intensivo necesita workers del consumidor.
- No hay transacciones para efectos externos ni garantías exactly-once. HTTP se cancela realmente; eso no garantiza que el proveedor remoto deje de generar o facturar. Callbacks síncronos no se matan por la fuerza. Los IDs durables pueden usarse como claves de idempotencia donde se soporten.

## Persistencia implementada

HiveDB 0.7.3 se consume como crate Rust fijado al commit `10fcd921d79872db881914f2e3f45bb3c20ed917`. No depende de los paquetes npm/Python de HiveDB ni de la carpeta local `../hiveBD`. Esta dependencia Git deberá publicarse o empaquetarse apropiadamente antes de publicar el crate del SDK en crates.io.

Colección `hive_sdk_rust_sessions_v1`, clave formada por la tupla JSON `[tenant, agent, session]`. Las ejecuciones conservan los formatos 2–5 según objetivo, contexto y atribución de resumen; los registros anteriores siguen legibles. Los handles no cambian esos formatos. Incluye identidad, historial confirmado, estado `ready` / `running` y checkpoint del turno. Las funciones y credenciales del proveedor se registran nuevamente al abrir. Los identificadores no son una barrera de autorización frente a la aplicación que posee el handle.

`begin_run` guarda el prompt y genera un run ID. Antes de proveedor/tool, `claim` guarda un execution ID y la operación pendiente. `advance` aplica la respuesta al motor autoritativo y guarda el checkpoint junto al resultado de tool, sin confiar en contadores calculados por el wrapper. Cada operación exige la revisión vigente. El mismo directorio tiene un único handle escritor; compartir `SessionStore` o sus ámbitos entre agentes.

La colección `hive_sdk_rust_runs_v1` conserva cada turno por `[tenant, agent, session, run_id]`, con estado, operación pendiente y ledger de tools `started` / `completed` / `rejected`. Un rechazo de schema se aplica localmente y confirma ledger, contador y checkpoint en una sola transacción, sin operación externa pendiente. El documento de sesión y el de turno se actualizan con CAS. Los turnos abandonados se conservan; no hay retención/borrado automático.

Reabrir requiere el mismo modelo, system prompt, tools en el mismo orden y política. El SDK se construye desde cero y usa bases nuevas. Solo reabre el estado generado por su propio store. Importar datos, snapshots o configuraciones del SDK anterior queda fuera de alcance.


Ante crash/error, la carga normal falla con `SESSION_INTERRUPTED`. La apertura explícita para reanudar admite solo checkpoints sin operación pendiente y conserva turnos, tokens, observaciones y límites. Una operación pendiente provoca `SESSION_OPERATION_UNCERTAIN`: no se repite automáticamente, aunque el corte pudiera haber ocurrido antes del despacho real.

`resolveTool` / `resolveModel` validan revisión, tipo e ID antes de aplicar el resultado aportado. `recover` descarta el turno y conserva su registro abandonado. Ambas rutas invalidan escrituras tardías por CAS, pero no detienen efectos externos de un ejecutor vivo: finalizarlo antes de intervenir. Los hooks pueden repetirse al observar de nuevo una acción tras reanudar.

Errores adicionales: `STORAGE_ERROR`, `SESSION_CONFLICT`, `SESSION_INTERRUPTED`, `SESSION_CONFIG_MISMATCH`, `SESSION_OPERATION_UNCERTAIN`. Los checkpoints del ciclo usan un handle Rust con workers y join de escritura al cancelar. Hay factories open y alternativas Async/_async de apertura, inspección, recuperación, resolución, cierre, planes y control de Objective. Las variantes síncronas se conservan; checks/inspecciones y claims/finalizaciones internos de Objective/Trigger ya usan rutas asíncronas durables. Los ámbitos se retienen antes del primer await y la cancelación espera workers y resultados conocidos. Hay [medición local con historial y concurrencia](docs/objective-runtime.md), sin cota de latencia de producción. Ver [APIs asíncronas](docs/async-storage.md). Ver [handles durables](docs/durable-handles.md).

No hay reintento automático de operaciones ambiguas ni ejecución externa exactly-once. Una sesión detenida por política conserva su estado terminal al reabrir: no se reinician límites como mecanismo de continuación. Un turno del formato anterior sin checkpoint requiere abandono explícito; no se reconstruye especulativamente.

## Objetivo de la v1 pública

Mantener el núcleo anterior y ampliar proveedores, contexto y delegación bajo presupuesto compartido. MCP stdio y Streamable HTTP están disponibles para revisiones 2025, con catálogo fijo validado y sin reintentos. Las respuestas inciertas conservan el claim durable y exigen reconciliación; ver `docs/mcp.md`. Los detalles operativos y aceptación pendientes están en `PLAN.md`.

`Objective` es un contador compartido Rust explícito para delegación dentro del proceso. Reserva llamadas/salida antes del proveedor, cuenta tools y rechazos, limita profundidad y conserva reservas ante resultados desconocidos. El deadline comienza al construirlo y no se reinicia entre turnos. Falta de uso o exceso del cap impiden continuar. `context.delegate` hereda objetivo y cancelación; el padre espera a sus hijos antes de continuar, también ante errores. No habilita verificadores, triggers ni llamadas auxiliares por defecto. Con `SessionStore` e ID explícito, la reserva raíz se confirma junto al claim y su liquidación junto a la respuesta/checkpoint en la misma transacción HiveDB. Se conserva la vinculación del objetivo y la profundidad al reabrir; ver `docs/objectives.md`.

Los triggers de eventos, fecha UTC e intervalos se registran desactivados en el mismo objetivo. `emit` y `tick` ejecutan callbacks explícitos; `watch` solo espera al consumir su iterador, fuera del loop Agent. Cada ocurrencia nueva consume un intento raíz, sin reiniciar saldos, y `context.delegate` comparte presupuesto/profundidad/cancelación. Los intervalos atrasados producen un solo claim coalescido. Calendario, claim y contador se guardan juntos; un resultado incierto bloquea otros despachos del trigger hasta reconciliación explícita. Un recibo confirmado se puede consultar sin repetir el efecto, incluso si su resultado es JSON null. No se serializan callbacks ni se inicia un scheduler al reabrir. Ver `docs/triggers.md`.

`input_token_budget` admite perfiles explícitos `neutral_json`, `openai_chat_json`, `anthropic_messages_json` y `gemini_generate_content_json`, con BPE local y margen configurables. Los perfiles del adaptador reutilizan su constructor JSON y excluyen salida y streaming; no representan un tokenizer privado. `max_total_tokens` dentro de ese presupuesto limita entrada acumulada del turno con carga máxima entre estimación y uso reportado. `context_window_tokens` comprueba entrada estimada + cap explícito de salida antes del despacho. Selección y carga se conservan al reabrir, sin alterar el uso reportado ni los límites globales de Objective. Ver `docs/tokens.md`.

El plan de contexto admite resumen proporcionado y recuperación explícita de turnos históricos completos, con fuentes ligadas por SHA-256 al historial original. La búsqueda es léxica local y acotada, sin modelos auxiliares. El plan se modifica entre turnos con CAS y usa formato de sesión 4; no altera mensajes originales ni checkpoints activos. Sistema, resumen, fuentes recuperadas y turno activo participan íntegros en bytes/BPE y no se eliminan para hacer caber una solicitud. La generación explícita usa callbacks bajo el objetivo raíz, ledger y costo por permisos; delegados anidados y checkpoints conservan el ámbito. Los checkpoints atribuidos usan formato 5. No se resume automáticamente al desbordar contexto ni se repiten callbacks inciertos. Ver `docs/memory.md` y `docs/summary.md`.

## Streaming y transporte

Tokio ejecuta HTTP asíncrono. El binding npm consume futures; Python usa un runtime Tokio compartido y lectores del binding en su executor, sin bloquear el event loop con la espera de red. Un cliente por adaptador reutiliza el pool de conexiones. El timeout cubre envío/lectura/ensamblado/backpressure de cada solicitud; no es un deadline global del agente.

SSE se decodifica incrementalmente, con UTF-8 fragmentado, LF/CRLF/CR, comentarios y campos data multilínea. La cola nativa tiene 32 eventos, cada evento SSE como máximo 1 MiB y la respuesta total un máximo configurable de 16 MiB por defecto. Chat Completions solicita `include_usage` y exige finish_reason aceptado y `[DONE]`. Anthropic exige `message_stop` con stop_reason aceptado. Gemini exige STOP y final HTTP/framing SSE limpios, conservando cada parte firmada sin fusionarla. Los índices neutrales de tools omiten bloques de texto. EOF prematuro, truncación o contenido no representable son errores; no se despachan tools con deltas parciales.

Los deltas no son checkpoints durables ni incrementan uso por sí solos: solo `reply` completo pasa al motor. Un error/cancelación a mitad de HTTP deja el claim persistente incierto. `CANCELLED`, `TIMEOUT` y `DEADLINE_EXCEEDED` son errores, sin consulta extra al modelo. Python expresa cancelación como `asyncio.CancelledError`, timeout HTTP como `TimeoutError` y deadline global como `DeadlineExceeded`, subclase de TimeoutError. Si no hay operación incierta y el motor puede confirmar una parada, devuelve `stopped/deadline_exceeded`.

Los proveedores personalizados npm reciben `{signal}` como segundo argumento. En Python pueden optar por `with_context=True` para recibir `{cancel_event}`. Los callbacks antiguos sin contexto siguen funcionando. Las tools cooperativas pueden consultar su contexto; si devuelven un resultado conocido tras cancelar, se guarda antes de detener la continuación. En Python un callback síncrono en thread mantiene el agente ocupado hasta terminar. Hooks/funciones que ignoran cancelación pueden retrasar el cierre; no hay terminación forzosa.

## Reserva y deadline global

Opciones nuevas positivas o null/None: `max_output_tokens`, `max_total_output_tokens`, `max_run_ms`; todas desactivadas por defecto. Antes de cada ModelRequest se calcula `min(cap_por_llamada, salida_agregada_restante)` para los límites presentes. Si no queda salida, no hay nuevo despacho. La reserva activa se guarda en el checkpoint antes de marcar la operación, y no se suma de nuevo al comprobar/reanudar el mismo request.

El contrato neutral añade `ModelRequest.max_output_tokens` y `deadline_unix_ms` opcionales. El adaptador compatible envía el cap como `max_completion_tokens`; `outputTokenParameter` / `output_token_parameter` permite elegir `max_tokens` explícitamente. El deadline y los nombres neutrales no van en el body del vendor. Un proveedor personalizado debe respetar el cap. No hay reintento automático con otro parámetro.

Uso conocido liquida la reserva y suma salida real. Uso ausente con presupuesto agregado detiene con `usage_unavailable` y conserva la reserva. Un proveedor que reporte salida superior al cap produce `output_limit`, sin ocultar el sobreconsumo. Una respuesta final natural puede completar exactamente en el límite; si pide tools sin salida restante se detiene antes de ejecutarlas.

Al iniciar se guarda `Engine.deadline_unix_ms`, calculado con reloj Unix del sistema y limitado al rango de enteros seguros de JavaScript. Incluye el tiempo pausado entre procesos; presupone reloj del sistema confiable. `preflight` y CAS vuelven a comprobar antes de cada despacho; el transporte usa el menor tiempo entre timeout HTTP y deadline restante. Timers anfitriones propagan la señal a callbacks cooperativos. No se fabrican timestamps nuevos en una reanudación.

Una operación HTTP/modelo incierta mantiene su reserva y no puede reanudarse automáticamente aunque haya vencido. Un checkpoint limpio vencido se confirma como parada y libera su reserva no despachada. Resultados conocidos que llegan tarde se registran antes de detener; no se paga otra llamada para sintetizarlos. Los registros anteriores siguen legibles con campos nuevos por defecto; activar límites distintos requiere una sesión/configuración nueva.

Estos presupuestos cubren un turno del harness, no una cuenta de facturación ni llamadas propias que un callback haga fuera del motor. No se cuenta todavía la entrada exactamente; `max_total_tokens` sigue siendo control de uso reportado. La garantía económica de salida depende de que el proveedor honre el parámetro y reporte uso fiable.

## Validación de argumentos

Motor `jsonschema` Rust con Draft 2020-12 por defecto y detección del draft declarado. Formatos reconocidos se validan; formatos o keywords personalizados no reconocidos siguen las reglas de anotación del validador. No hay recuperación HTTP/file de referencias. Los schemas compilados se comparten en una caché FIFO de hasta 64 entradas por proceso.

`INVALID_TOOL_SCHEMA` impide registrar un schema inválido o con referencias no disponibles. Una llamada incompatible produce `tool_rejected` con resultado `{error:{code:"INVALID_TOOL_ARGUMENTS",tool,message,issues,truncated}}`. Cada issue indica `instance_path`, `schema_path` y mensaje con valores de instancia enmascarados. Se devuelven como máximo 8 issues y 512 caracteres por mensaje; no se modifica la entrada, insertan defaults ni coercionan tipos.

El motor consume el rechazo como mensaje de tool para permitir corrección en una llamada normal posterior al modelo. Se cuenta dentro de los límites existentes, no hay un loop adicional ni un modelo validador. Errores idénticos repetidos participan del detector de estancamiento incluso si la tool permite polling. El detector global configurable y los límites de llamadas siguen vigentes.
