# Handles durables y workers de checkpoints

Los agentes con SessionStore conservan configuración, revisión CAS y checkpoint dentro de un handle Rust por agente. El ciclo intercambia comandos pequeños y acciones con el wrapper. No recibe snapshots completos ni acepta del anfitrión un estado que sustituya el motor durable. Cada nueva acción de modelo conserva el contexto seleccionado que necesita el proveedor.

Se reutilizan las operaciones transaccionales de SessionStore. Begin guarda el prompt; claim reserva antes de cualquier callback; advance liquida el uso y confirma la respuesta o resultado de tool con el checkpoint. El handle conserva la revisión y exige el execution ID vigente al confirmar. Las colecciones se mantienen; presupuestos raíz, atribución de resúmenes y fencing siguen en las transacciones existentes. El ámbito optativo por turno usa schema 6, como se describe abajo.

## I/O del ciclo y cancelación

Node ejecuta begin/resume, check, claim, advance, reject y commit mediante `spawn_blocking`, esperando su Promise. Python envía esas operaciones a `asyncio.to_thread`; el binding libera el GIL. Ningún callback se invoca bajo el mutex ni antes de confirmar el claim.

Una escritura en curso no se cancela a mitad de una transacción. El agente espera su fin antes de liberar la ocupación del store. Python protege y drena el worker incluso si el Task recibe cancelaciones repetidas. Luego informa cancelación. Una respuesta conocida que alcance a guardarse conserva su uso liquidado y puede reanudarse sin otro modelo. Un claim guardado antes de cancelar conserva su operación pendiente: no se supone que nunca hubo un efecto externo ni se repite automáticamente.

Durante esos waits, close/recover/resolve siguen bloqueados por el guard de ejecución del store. Las consultas del historial usan una referencia visible bajo un lock separado y breve, sin esperar el mutex de I/O. Antes de commit se conserva el historial visible previo; abrir con resume muestra el checkpoint cargado, igual que antes. Cerrar el store invalida nuevos despachos y libera su referencia a HiveDB. El agente todavía permite consultar su copia visible del historial/contexto.

Un error deja el checkpoint durable para recuperación explícita; no aplica el abort que usan los agentes en memoria. Reabrir exige la misma configuración. Un handle obsoleto no puede reemplazar un checkpoint más reciente; la comparación de revisión sigue en Rust/HiveDB.

## Medición del puente

Después de compilar los bindings, ejecutar `node scripts/measure-durable-bridge.mjs`. Usa dos bases temporales: una con el puente anterior y otra con el handle nuevo. Compara acciones y revisiones tras cada transición, realiza los claims y escrituras reales, y confirma que el historial final es idéntico. El [reporte reproducible](../evals/durable-bridge.json) cuenta bytes UTF-8 de comandos/respuestas durante la tarea, incluyendo begin, claims, advances y commit; excluye ingestión histórica y consulta explícita de messages.

| Historial | Tools | Contexto máximo | Puente anterior, bytes | Handle, bytes |
| --- | ---: | ---: | ---: | ---: |
| 20 turnos, 1 KiB por mensaje | 3 | Sin límite | 1443146 | 350461 |
| 60 turnos, 1 KiB por mensaje | 12 | 16 KiB | 7733209 | 419262 |

El contexto de las acciones de modelo está incluido. Esto mide serialización local, no tokens ni facturación. El script imprime avance de un timer durante el trabajo combinado y tiempos informativos; se excluyen del reporte determinista. Ese avance no constituye una cota de latencia ni una prueba de carga de toda la aplicación.

## Alcance pendiente

Hay [factories y alternativas públicas asíncronas](async-storage.md) para apertura, planes, inspección, recuperación, resolución, cierre y control de Objective. Las variantes síncronas se conservan y pueden esperar I/O/locks compartidos. Los flujos internos de Objective/Trigger ya usan workers para el control durable; hay [pruebas de ámbitos y medición local bajo concurrencia](objective-runtime.md), sin garantía de latencia de producción. Los checkpoints aún se serializan dentro de Rust para HiveDB; el cambio elimina su transporte completo por el puente del ciclo.

Pruebas: reapertura sin replay, operación incierta, IDs obsoletos, CAS entre handles, plan de contexto, getters mientras el lock del store está ocupado y cierre. Los bindings añaden gates de worker para cancelación antes de begin/claim/advance, ocupación hasta terminar y liquidación conocida bajo presupuesto raíz. Los gates controlan la frontera de la espera; el despacho real al almacenamiento se realiza al liberarlos.


## Objetivo durable por turno

Una sesión sin objetivo fijo puede iniciar un turno con un objetivo existente mediante `SessionStore::begin_run_with_objective` o `DurableEngineHandle` con `begin_run`, `objective_id` y `objective_depth`. El ID queda en el checkpoint del turno y en `frame.objective_id`. La configuración del motor conserva su objetivo fijo, incluido null. Claim/reject/advance cargan y liquidan el objetivo efectivo en la misma transacción del checkpoint. Las sesiones ligadas a una raíz rechazan el override y un turno interrumpido no puede cambiar de ámbito. Solo después de completar el turno puede iniciarse otro objetivo conservando el historial de la conversación.

Los registros que llevan ese ámbito usan schema 6 y rechazan lectores anteriores. No se migran ni reanudan sesiones automáticamente. La integración y el caso de uso están en [delegación del gateway](gateway-durable-delegation.md); los wrappers Agent mantienen su contrato de objetivo fijo.
