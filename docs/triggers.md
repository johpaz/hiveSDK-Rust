# Triggers explícitos

Los flujos durables emit/tick/watch usan workers para inspección, claims y finalización. Para registrar y controlar sin I/O síncrono usar `objective.triggerAsync` / `trigger_async`, `enableAsync` / `enable_async`, `disableAsync` / `disable_async`, `inspectAsync` / `inspect_async` y `resolveAsync` / `resolve_async`. Ver [ámbitos, cancelación y medición local](objective-runtime.md).

Rust y los bindings npm/Python admiten eventos externos, fecha UTC única e intervalos. Los callbacks se ejecutan fuera del loop de `Agent`; registrar un trigger no crea timers ni llamadas al modelo.

Registrar un trigger no lo activa. Habilitarlo tampoco crea timers ni llama al modelo. El kernel solo prepara claims al recibir `poll_trigger_at` o `emit_trigger`; la aplicación ejecuta el trabajo fuera del ciclo Agent y aporta su resultado.

```rust
use hive_agent_core::objective::{Objective, ObjectivePolicy, TriggerSchedule};
use serde_json::json;

fn example() -> hive_agent_core::Result<()> {
    let objective = Objective::new(ObjectivePolicy::default())?;
    objective.register_trigger("receipt-events".into(), TriggerSchedule::Event)?;
    objective.enable_trigger("receipt-events", true)?;
    if let Some(claim) = objective.emit_trigger(
        "receipt-events", "event-123".into(), json!({"receipt": "r1"}),
    )? {
        if claim.result.is_none() {
            // Aquí se ejecuta el trabajo explícito del host, usando el claim como identidad.
            let result = json!({"observed": true});
            objective.finish_trigger("receipt-events", &claim.occurrence_key,
                &claim.execution_id, result)?;
        }
        // claim.result contiene un recibo confirmado cuando el evento ya se procesó.
    }
    Ok(())
}
```

No iniciar un proveedor desde un driver Rust sin reservar antes su llamada/salida en el mismo `Objective`, transmitir cap/deadline y reconciliar uso. El kernel no ejecuta callbacks ni constituye un sandbox.

## npm y Python

```js
const trigger = objective.trigger('receipts', { kind: 'event' }, async (input, context) => {
  return await context.delegate(worker, `Procesa el recibo ${input.receipt}`);
});
trigger.enable();
const receipt = await trigger.emit('event-123', { receipt: 'r1' });
```

```python
async def process(input, context):
    return await context.delegate(worker, f"Procesa el recibo {input['receipt']}")

trigger = objective.trigger('receipts', {'kind': 'event'}, process)
trigger.enable()
receipt = await trigger.emit('event-123', {'receipt': 'r1'})
```

El callback devuelve JSON; el recibo contiene `trigger_id`, `occurrence_key`, `execution_id` y `result`. El contexto incluye esos IDs y `objective_id`, además de `signal`/`cancel_event` y `delegate`. La delegación hereda el presupuesto raíz, la cancelación y profundidad 1; el ejecutor espera todos los hijos iniciados, incluso si el callback falla. El contexto deja de aceptar hijos al terminar el callback. Para conservar estos límites, iniciar agentes desde `context.delegate`.

Calendarios npm: `{kind: 'at', unixMs}` o `{kind: 'interval', everyMs, anchorUnixMs?}`. Python usa `unix_ms`, `every_ms` y `anchor_unix_ms`. `inspect()` devuelve los nombres persistidos en snake_case en ambos lenguajes.

`tick()` observa el reloj real y ejecuta a lo sumo una ocurrencia vencida. Para esperar explícitamente:

```js
const timer = objective.trigger('periodic', { kind: 'interval', everyMs: 60_000 }, processTick);
timer.enable();
for await (const receipt of timer.watch({ signal })) {
  console.log(receipt.result);
}
```

```python
timer = objective.trigger('periodic', {'kind': 'interval', 'every_ms': 60_000}, process_tick)
timer.enable()
async for receipt in timer.watch(cancel_event=cancel_event):
    print(receipt['result'])
```

Crear `watch()` no inicia trabajo: hay que consumir el iterador. Un watcher deshabilitado termina sin efectos. Solo se permite un watcher por wrapper; una fecha única termina tras su recibo. Cerrar el iterador libera su ámbito. Durante esperas y callbacks el store permanece ocupado para impedir su cierre prematuro. No hay daemon ni scheduler que se reinicie al abrir el SDK.

`disable()` impide claims futuros y despierta el watcher local; no cancela efectos activos. `cancel()` cancela cooperativamente los ejecutores locales, sin deshabilitar la definición ni cancelar todo el objetivo. También se admiten `signal` en npm y `cancel_event` en Python en `emit`/`tick`. El deadline raíz se aplica durante las esperas. Cancelar no interrumpe por la fuerza un callback síncrono; si devuelve un resultado conocido se guarda antes de informar la cancelación. Desactivar desde otro wrapper/proceso se observa al siguiente tick; no hay notificación distribuida.

## Calendario y deduplicación

`TriggerSchedule::At { unix_ms }` dispara una sola fecha UTC. `Interval { every_ms, anchor_unix_ms }` usa un período positivo y ancla opcional; sin ancla, el primer vencimiento se calcula una sola vez como registro + período. Reabrir la misma definición conserva ese calendario y estado enabled. Cambiar la definición requiere otro ID.

Un intervalo atrasado coalesce al último vencimiento observado: un poll devuelve a lo sumo un claim, con `scheduled_for_unix_ms`, `observed_unix_ms` y `skipped_occurrences`. No procesa una cola de todos los intervalos perdidos. La siguiente fecha avanza desde el vencimiento programado; una observación que retrocede no reinicia el calendario. Son intervalos UTC, no cron ni zonas con DST.

Los eventos usan una clave estable. Evento confirmado + misma entrada devuelve el recibo sin otro intento; otra entrada con esa clave falla. Desactivar un trigger impide nuevos claims y no borra sus operaciones pendientes. Los timestamps deben ser enteros portables no negativos; IDs, 1–256 bytes; entrada/resultado, hasta 64 KiB JSON. Hay como máximo 64 definiciones por objetivo.

## Presupuesto y recuperación

Un claim consume un intento raíz, compartido con tools y verificaciones. No concede otro saldo de modelos ni salida por ocurrencia. Un claim pendiente impide otro despacho de ese trigger, incluso con otra clave, hasta conocer su resultado. No se repite por timeout, cierre o reinicio.

`finish_trigger` exige ocurrencia e ID de ejecución vigentes; un resultado repetido o ID antiguo se rechaza. Un resultado JSON null se conserva como resultado confirmado, distinto de un resultado desconocido. Es posible guardar un resultado conocido después de cancelar el objetivo, pero no autoriza nuevos efectos.

Para persistir, `SessionStore.open_objective` y `objective_command` ofrecen `trigger_register`, `trigger_enable`, `trigger_inspect`, `trigger_emit`, `trigger_tick` y `trigger_finish`. Claim, calendario y contador se confirman en el mismo documento/transacción HiveDB. El ledger de objetivos usa formato 4 desde el kernel de resúmenes y lee formatos previos conservando sus saldos. Reabrir no elimina incertidumbre ni activa callbacks automáticamente.

Un callback fallido o resultado inválido deja `TriggerUncertain`, sin repetirlo automáticamente. Antes de reconciliar, detener al ejecutor anterior y comprobar el efecto externo. npm: `trigger.resolve({occurrenceKey, executionId, result})`; Python: `trigger.resolve(occurrence_key=..., execution_id=..., result=...)`. Los IDs se obtienen con `inspect().executions`; se exige el ID vigente. El cierre de un proceso no garantiza rollback ni exactamente una ejecución en un servicio remoto.
