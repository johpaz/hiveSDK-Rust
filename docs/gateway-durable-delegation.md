# Delegación durable del gateway

El gateway Rust local usa el objetivo durable y el pool del SDK para las consultas de texto a especialistas. Conserva selección, proveedores, historial y narración desde HiveDB. No instala herramientas externas en un especialista por asignarle un nombre de catálogo.

## Flujo

Cada turno nuevo crea un objetivo `gateway.turn.<uuid>`. El ID queda guardado en el checkpoint del coordinador; sus hijos usan exactamente ese objetivo y profundidad 1. Las reservas de modelo, uso confirmado y cargos de herramientas se escriben junto con sus checkpoints mediante CAS. El gateway ya no mantiene un segundo contador de presupuesto en memoria.

`task_delegate` mantiene sus parámetros `worker_id`, `task_description` y `mode: sync` opcional. Ahora ejecuta un trabajo durable antes de devolver el resultado. Varias llamadas individuales conservan su orden; no se convierten implícitamente en un lote paralelo.

`task_delegate_group` recibe consultas independientes:

```json
{
  "tasks": [
    { "task_id": "analysis", "worker_id": "analyst", "task_description": "Analiza los hechos adjuntos." },
    { "task_id": "review", "worker_id": "reviewer", "task_description": "Revisa las limitaciones de la evidencia." }
  ]
}
```

El schema exige IDs de tarea, especialistas habilitados del catálogo visible y descripciones no vacías. El lote admite hasta el menor entre 64 y el número de especialistas visibles. Los IDs de tarea deben ser únicos. La selección se comprueba otra vez antes de ejecutar cada hijo, incluyendo ámbito de usuario, conversación, configuración de delegación y estado del especialista.

La llamada del coordinador se reclama primero. Su ejecución identifica un grupo cerrado; grupo y trabajos hijos se guardan atómicamente. Las sesiones de los hijos son las asignadas por el núcleo, derivadas del ámbito/grupo/tarea. Los trabajos no son reintentables automáticamente.

La cola es `gateway.specialists.<objective_id>`, dentro del tenant del store. Cada turno admite hasta cuatro especialistas concurrentes; un turno incierto no ocupa la capacidad de otro. Mantiene los defaults de lease y timeout de trabajo del SDK, con hasta 1 MiB por payload/resultado. No hay un límite global adicional entre conversaciones. Los límites de llamadas y tokens del objetivo siguen aplicándose a todo el turno.

El gateway dirige lotes explícitos del pool Rust y renueva leases mientras espera. Solo reclama el kind del grupo actual. La narración registra el comienzo real de cada especialista y sus resultados, con claves distintas para tareas paralelas. Los chunks de los hijos no se envían como respuestas del coordinador.

## Reunión de resultados y respuesta final

El resultado de la herramienta se confirma en el checkpoint del coordinador una vez que todos los hijos son terminales. Incluye task/worker/job IDs, contenido, estado, fallos y `checks.status: unchecked` por defecto. Si se publica explícitamente una prueba durable antes de entregar el resultado, se conserva su veredicto/evidencia; un check fallido produce `ok: false`. El gateway no configura un verificador automático. Un fallo conocido de preparación queda como trabajo fallido y llega al coordinador sin afirmar éxito. El contenido del especialista por sí solo no se presenta como comprobado.

Después del tool result, el mismo motor del coordinador continúa su conversación y produce la respuesta final. Este adaptador no abre una segunda conversación ni publica el trabajo independiente de `synthesizeGroup`: utiliza la siguiente operación de modelo del checkpoint padre. Por eso el informe del grupo conserva `ready` y `synthesis: null` aun después de responder el coordinador. El checkpoint padre registra y cerca esa continuación.

La API independiente de grupos Node/Python/Rust mantiene su propia operación `synthesizeGroup` / `synthesize_group`, descrita en [el contrato de delegación](durable-delegation.md). No se deben dirigir ambas rutas de síntesis sobre el mismo flujo del gateway.

La política conserva las cuotas de modelos derivadas de iteraciones y catálogo: `I + I × max(1, W)`. Con especialistas, la cuota de herramientas admite `2 × I × W`, porque se cuentan tanto la herramienta del padre como cada claim de trabajador. Sin especialistas conserva `I` cargos. No se amplía la cuota de modelos ni se recortan sus reservas de salida a un default fijo. Si falta presupuesto, no se crea un objetivo nuevo para continuar.

## Cancelación, caída y recuperación

Detener un turno solicita cancelación del grupo y de sus trabajos en una transacción, detiene nuevas reclamaciones del pool y espera los hijos iniciados. El objetivo queda cancelado de forma durable. Un hijo cuya inferencia ya se despachó puede quedar `uncertain`; su permiso sigue reservado. La cancelación no prueba que un efecto remoto no ocurrió.

Una caída deja guardados el objetivo, grupo, claims, sesiones y permisos. Abrir el gateway o intentar otro mensaje en la conversación interrumpida no reanuda inferencias. Tampoco confirma el resultado desconocido de un proveedor. La prueba de caída termina un proceso después de escribir un efecto de fixture y antes de confirmar el modelo hijo; el siguiente proceso observa un solo intento y la reserva original.

La recuperación sigue siendo administrativa y explícita con `SessionStore`/cola, con los ejecutores anteriores detenidos. Hay que reconciliar por separado la operación del modelo hijo, el trabajo de cola y la operación de herramienta del padre, con evidencia y sus fences actuales. No basta con marcar un job como completado para liquidar un modelo desconocido. Un objetivo cancelado sigue cancelado: reconciliar registra hechos y costos, sin renovar permisos ni habilitar otra inferencia. No se añade todavía una interfaz HTTP/CLI de administración ni un watcher de recuperación.

La configuración de herramientas del coordinador ha cambiado; las sesiones existentes conservan su contrato. Esta entrega no migra ni reinicia conversaciones antiguas o checkpoints fallidos. Para usar las nuevas herramientas se necesita una conversación nueva con la configuración actual.

## Objetivo por turno en el núcleo

Una sesión sin objetivo fijo puede llamar a `SessionStore::begin_run_with_objective` o enviar a `DurableEngineHandle`:

```json
{ "op": "begin_run", "prompt": "Consulta", "objective_id": "existing-root", "objective_depth": 0 }
```

El objetivo debe existir previamente. El checkpoint guarda el ID por turno y devuelve `frame.objective_id`; `frame.state.objective_id` conserva el objetivo fijo de la sesión, que puede ser null. Claim/reject/advance usan el objetivo efectivo y sus escrituras son atómicas con la sesión. Los registros con ese ámbito usan schema 6 para que lectores anteriores los rechacen. Las sesiones con objetivo fijo rechazan el override. Un turno interrumpido no puede cambiar de objetivo; tras completar un turno, la conversación puede comenzar otro bajo una raíz distinta sin perder historial ni modificar la anterior.

Los wrappers Agent de Node/Python conservan su contrato de objetivo fijo; esta API de ámbito por turno se usa desde Rust y los comandos nativos del gateway.

## Validación y límites

Pasaron las pruebas de grupos paralelos con barrera de sincronización, fallos de preparación, caída real de proceso, cancelación durable, consultas a 17 especialistas y narración/historial existentes. Los contratos nuevos del núcleo verifican raíces distintas por turno, historial conservado, imposibilidad de rebind durante interrupción, reconciliación del permiso original, profundidad, cancelación y rechazo de overrides en sesiones ligadas.

No hay llamadas a proveedores reales ni envío de mensajes a cuentas. Las pruebas TCP completas siguen sujetas a las restricciones del sandbox. La integración local no completa la API multi-tenant general, el bus compartido, scheduler, revisiones de tareas, comprobaciones automáticas de aceptación ni goal runner del plan de paridad. Evidencia y hashes: [gateway-delegation-extension.json](../evals/gateway-delegation-extension.json).


Comprobaciones locales: 95 pruebas seleccionadas del núcleo (incluidas 4 nuevas de objetivo por turno), 56 del gateway y 24 de cada binding Node/Python. El recuento del gateway incluye el helper de subproceso, inactivo fuera de su fixture. Se excluyeron tres pruebas TCP del gateway por las restricciones conocidas; en Python se excluyeron dos pruebas antiguas de salida de subproceso y se usó un heartbeat externo del event loop. La prueba nueva de caída real del gateway y ambas recuperaciones Node → Python → Node sí pasaron. También pasaron build, formato, Clippy con todos los features/targets, el consumidor TypeScript estricto y las cuatro comparaciones seleccionadas con TS. Esto no certifica la regresión completa de red, modelos remotos, CI remoto ni la matriz de plataformas.
