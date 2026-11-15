# Fallo de delegación a HiveAgents — 2026-11-11

## Evidencia

Los registros durables del perfil cerrado muestran que el 10 de noviembre a las 17:30:32
(America/Bogota) `email_manager` recibió `PROVIDER_ERROR: HTTP 503` durante la solicitud de modelo.
Su asignación en HiveDB era el proveedor `hiveagents` y `Qwen3.6-35B-A3B-UD-Q4_K_M.gguf`, con
`context_window: 50000`. El coordinador, configurado con Gemini, propagó ese fallo del hijo.
Se inspeccionó una copia aislada; no se reanudó la inferencia ni se alteró el perfil original.

El gateway Rust trataba HiveAgents como OpenAI genérico. El adaptador del proyecto `hive`
(`packages/core/src/agent/llm-providers/hiveagents.ts`) prepara el GGUF mediante `/api/status` y
`/api/load`, y usa el alias `local` al inferir. Esos pasos faltaban en la migración. El registro
antiguo no conserva el cuerpo del 503, por lo que no permite descartar además una indisponibilidad
del servicio remoto.

## Corrección

- Adaptador nativo HiveAgents: obtiene el estado, solicita la carga una vez y comprueba que el
  modelo esperado está listo. Conserva el identificador GGUF en el estado durable del agente.
- Inferencia OpenAI compatible con `model: "local"` y `max_tokens`. Se eliminan los headers
  `user-agent` y `x-stainless-*` configurados, como en el cliente original.
- El contexto de carga respeta HiveDB. Un lease por backend coordina preparación y streaming
  entre instancias del proveedor dentro del mismo proceso.
- La espera respeta cancelación, timeout del proveedor y deadline del turno. Solo se consulta
  repetidamente el estado; no se repiten cargas ni inferencias dentro del intento.
- El error propagado identifica al especialista y se imprime también en la consola, además
  de quedar guardado en `narrationEvents`.

Los checkpoints fallidos se conservan para revisión. Para probar la corrección, reiniciar la app
y el gateway e iniciar una consulta nueva; no se recuperan ni repiten los intentos anteriores.
Una carga ya aceptada puede continuar remotamente aunque el cliente cancele la espera.

## Validación

- Una prueba de regresión falló antes del cambio porque no existía el adaptador nativo HiveAgents.
- 6 pruebas del ciclo de preparación y coordinación, 2 de configuración del adaptador y 16 del
  gateway pasaron, incluida la propagación del error sin repetir la solicitud del hijo.
- 33 pruebas existentes de serialización y streaming de proveedores pasaron.
- Clippy del núcleo y gateway, incluidos sus targets: sin advertencias.
- La prueba HTTP completa de carga y alias está compilada, pero necesita TCP local y queda
  pendiente en este sandbox. Tampoco se hicieron llamadas al servicio real ni cargas remotas.

## Plazo agotado durante delegaciones sucesivas

El error posterior `DEADLINE_EXCEEDED: run deadline elapsed` del especialista
`external_code_agent` corresponde a un límite global añadido por el gateway Rust: tanto
el motor como el objetivo compartido imponían 120 segundos. La regresión reprodujo que
la primera llamada del coordinador, el especialista y la síntesis heredaban exactamente
el mismo vencimiento. `max_output_tokens=1268` describe el presupuesto de salida restante,
no la duración de la llamada.

Hive original (`packages/core/src/agent/agent-loop.ts`, `LLM_CALL_TIMEOUT_MS`) concede tres
minutos a cada operación de modelo y evita un reloj agregado del turno. El gateway sigue
ahora ese contrato: timeout de transporte de 180000 ms por llamada y sin deadline global
implícito. En HiveAgents ese tiempo incluye lease, preparación y streaming. Se mantienen
los límites compartidos de llamadas, profundidad y tokens, así como la cancelación; no se
repiten automáticamente operaciones fallidas. El SDK conserva su soporte optativo de
deadlines durables.

Las sesiones existentes conservan su política durable original. Para probar el cambio hay
que reiniciar la app/gateway y abrir una conversación nueva; no se modifican los checkpoints
pendientes. Esto corrige el plazo de la consulta de texto al especialista; no añade ejecución
de CLI ni herramientas externas al agente de código.

## Salida truncada del Operador de APIs

Una copia cerrada del siguiente perfil registra el fallo de `api_operator` con
`max_output_tokens=2048`. HiveDB le asigna HiveAgents y Qwen con `context_window=50000`,
sin `num_ctx`. El código original calcula la reserva de salida como 15% del contexto,
con techo 32768: en este caso 7500. El gateway había heredado los defaults genéricos del
objetivo del SDK (2048 por llamada y 8192 agregados), recortando esa reserva.

Se restaura el cálculo desde HiveDB en la política de cada agente. El objetivo compartido
admite hasta 16 reservas completas del techo, manteniendo límites de llamadas, profundidad
y cancelación. Sin contexto configurado se usa un fallback explícito de 4096. Las pruebas
de delegación verifican reservas distintas para coordinador y especialista (7500, 15000,
7500), sin recorte por los anteriores defaults. Las sesiones anteriores conservan su política.

El adaptador OpenAI compatible conserva ahora `finish_reason` y el uso de salida conocido,
tanto en JSON como SSE, distinguiendo `length`, `content_filter` y códigos no soportados.
El error antiguo no permite recuperar el código que descartó; el cap insuficiente es una
diferencia confirmada, pero no demuestra que aquel proveedor terminara por `length`.
Las respuestas incompletas siguen sin confirmarse ni reintentarse automáticamente.

## Límite fijo de ocho herramientas

La regresión del siguiente error, `OBJECTIVE_LIMIT: tool_call_limit`, reproduce el corte
del objetivo compartido tras ocho herramientas. Además del tope de 16 llamadas al modelo,
el motor local conservaba su default de 16 herramientas. Estos límites propios de la
migración impedían consultar un catálogo completo en una sola respuesta del coordinador.
Hive original limita las iteraciones mediante `agents.max_iterations`, y cada iteración
puede contener varias llamadas a herramientas.

La política de aplicación se deriva ahora de HiveDB: I iteraciones (default 10) y W
especialistas visibles, habilitados y no archivados. La cuota de herramientas es
I × max(1, W); la cuota de modelos suma I para el coordinador, pues cada especialista
actual responde con una única llamada sin herramientas. El motor local recibe la misma
cuota de herramientas y conserva su límite de iteraciones. El presupuesto de salida total
se deriva de la cuota de modelos, manteniendo el techo por modelo y la cancelación. Esta
política sustituye los topes de 8/16 y las 16 reservas descritas en las correcciones anteriores.
Se rechazan configuraciones inválidas o desbordamientos antes de ejecutar.

Una prueba local falló con el error exacto antes del cambio y ahora completa 17 consultas
distintas y una síntesis: 19 llamadas de modelo en dos iteraciones del coordinador. Otras
pruebas verifican que las cuotas derivadas siguen rechazando llamadas por encima del límite.
No se altera la política por defecto del SDK, ni se reanudan checkpoints fallidos. Hace falta
reiniciar el gateway y usar una conversación nueva para actualizar la política local durable.
