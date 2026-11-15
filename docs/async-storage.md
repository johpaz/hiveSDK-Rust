# APIs públicas asíncronas de almacenamiento

Las factories `SessionStore.open`, `Agent.open` y `Objective.open` devuelven un objeto listo después de completar su apertura y validación. Las variantes asíncronas de almacenamiento usan workers; se conservan constructores y métodos síncronos para compatibilidad. Requieren Node >=24 o Python >=3.10, como el resto del SDK.

```js
import { Agent, Objective, SessionStore } from '@johpaz/hive-sdk-rust';

const store = await SessionStore.open({ path: './data/sdk' });
try {
  const objective = await Objective.open(
    { max_model_calls: 3, max_output_tokens: 256, max_total_output_tokens: 512 },
    { store, id: 'task-1' },
  );
  const agent = await Agent.open({
    model: 'your-model', provider, store, objective,
    agentId: 'assistant', sessionId: 'conversation-1',
  });
  const result = await agent.run('Consulta explícita');
  console.log(result.status, await objective.inspectAsync());
} finally {
  await store.closeAsync();
}
```

```python
from hive_sdk import Agent, Objective, SessionStore

store = await SessionStore.open('./data/sdk')
try:
    objective = await Objective.open(
        {'max_model_calls': 3, 'max_output_tokens': 256, 'max_total_output_tokens': 512},
        store=store, objective_id='task-1',
    )
    agent = await Agent.open(
        model='your-model', provider=provider, store=store, objective=objective,
        agent_id='assistant', session_id='conversation-1',
    )
    result = await agent.run('Consulta explícita')
    print(result['status'], await objective.inspect_async())
finally:
    await store.close_async()
```

Estos fragmentos presuponen un proveedor configurado; deben ejecutarse dentro de un contexto que permita await. El paquete sigue en desarrollo y no está publicado. En el checkout npm se importa desde `./packages/npm/index.mjs`.

## Métodos disponibles

| Operación | npm | Python |
| --- | --- | --- |
| Abrir store | `await SessionStore.open({path, tenantId})` | `await SessionStore.open(path, tenant_id=...)` |
| Abrir ámbito | `await store.forTenantAsync(tenantId)` | `await store.for_tenant_async(tenant_id)` |
| Cargar agente o checkpoint | `await Agent.open(options)` | `await Agent.open(**options)` |
| Abrir objetivo | `await Objective.open(policy, {store, id})` | `await Objective.open(policy, store=..., objective_id=...)` |
| Inspeccionar sesión | `await store.inspectAsync(options)` | `await store.inspect_async(**options)` |
| Abandonar turno explícitamente | `await store.recoverAsync(options)` | `await store.recover_async(**options)` |
| Resolver tool incierta | `await store.resolveToolAsync(options)` | `await store.resolve_tool_async(**options)` |
| Resolver modelo incierto | `await store.resolveModelAsync(options)` | `await store.resolve_model_async(**options)` |
| Cerrar store | `await store.closeAsync()` | `await store.close_async()` |
| Instalar plan | `await agent.setContextPlanAsync(plan)` | `await agent.set_context_plan_async(plan)` |
| Inspeccionar objetivo | `await objective.inspectAsync()` | `await objective.inspect_async()` |
| Resolver resumen | `await objective.resolveSummaryAsync(options)` | `await objective.resolve_summary_async(**options)` |
| Resolver verificación | `await objective.resolveVerificationAsync(options)` | `await objective.resolve_verification_async(**options)` |
| Cancelar objetivo | `await objective.cancelAsync()` | `await objective.cancel_async()` |

Argumentos, valores de retorno, tipos de error, revisión CAS y execution ID mantienen los contratos de las variantes síncronas. La apertura de un agente valida opciones/callbacks antes de cargar y conserva el estado en el handle Rust. Un objetivo local sin store sigue usando su kernel síncrono en memoria: una API awaitable no elimina el costo de CPU de esa operación.

## Guards y cancelación

Lecturas, aperturas derivadas y cargas retienen el store hasta terminar. Recuperar, resolver operaciones de sesión y cerrar adquieren un guard exclusivo antes de esperar: nuevos turnos, cargas, escrituras de planes y llamadas síncronas al store reciben BUSY mientras está pendiente. Una intervención no puede comenzar con ejecutores u otros workers activos. La reconciliación de Objective excluye nuevos ejecutores por esa instancia y conserva el fencing transaccional entre instancias.

Instalar un plan ocupa el agente y retiene el store hasta guardar el resultado. `agent.cancel()` devuelve false durante esa operación: no hay un turno/modelo activo que cancelar. Cerrar o ejecutar el agente mientras tanto falla con BUSY. Los getters cacheados de historial/contexto no acceden al almacenamiento.

Node no añade AbortSignal a estas operaciones de almacenamiento. Python espera el worker aunque el Task reciba cancelaciones repetidas. Si una apertura produce un store/ámbito después de cancelar, se cierra ese recurso en un worker antes de informar CancelledError. No se abandona una transacción en segundo plano ni se ejecuta otra llamada de modelo para reparar una operación.

Una cancelación puede llegar después de escribir. Un plan conocido se instala en el objeto antes de liberar sus guards; una cancelación de objetivo conocida también avisa a los callbacks activos aunque se cancele su waiter. Tras cancelar recuperación/resolución/cierre, inspeccionar o reabrir para conocer el estado; no asumir que la escritura se deshizo ni repetir un efecto externo. Las operaciones inciertas siguen exigiendo reconciliación explícita.

## Alcance pendiente

Los flujos internos de Agent/Objective/Trigger ya usan el despachador asíncrono para control durable, incluidos checks, claims y finalizaciones. Hay factories y variantes asíncronas de registro/inspección/enable/disable/resolución de triggers. Ver [ámbitos y medición local de concurrencia](objective-runtime.md). Los métodos síncronos explícitos se conservan como compatibilidad; la medición local no acredita una cota de latencia de producción.

Los constructores/métodos síncronos siguen pudiendo esperar I/O. Incluso con un worker, serializar argumentos o parsear una inspección/historial grande tiene costo en el lenguaje anfitrión. Las pruebas comprueban los workers y sus guards; no acreditan una cota de bloqueo ni ahorro facturado.

`tests/async-storage-parity.py` abre en Node, instala un plan, reconcilia una verificación en Python, ejecuta un turno y consulta el resultado desde Node usando estas APIs. No cambia formatos ni repite modelos/tools por el cambio de transporte local.
