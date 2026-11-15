# Control asíncrono de Objective y Trigger

Los flujos de Agent, Objective.verify/summarize y Trigger.emit/tick/watch usan ahora el despachador asíncrono para checks, inspecciones, claims y finalizaciones del objetivo. En objetivos durables esas operaciones se ejecutan en workers. Los callbacks siguen en npm/Python y las transacciones Rust conservan presupuesto, profundidad, atribución, revisión e ID de ejecución. No se añade ninguna llamada al modelo.

El ámbito cancelable se registra y retiene el store antes del primer await. Registrar un ámbito no hace I/O; cada ruta comprueba el presupuesto Rust antes de invocar un callback o proveedor. Durante un claim pendiente, cerrar el store o reconciliar manualmente la misma ejecución falla con BUSY. Cancelar espera al worker y no ejecuta el callback si el claim terminó después de cancelar. Una operación pendiente no se repite: exige resolución explícita.

La finalización de un resultado conocido se espera antes de informar cancelación. El recibo queda disponible por su clave y no se vuelve a ejecutar el callback. Consultar un resultado ya confirmado conserva las reglas de caché, incluso cuando el objetivo está detenido y ya no admite trabajo nuevo. La delegación valida tipo, ocupación y scope inmediatamente; el hijo comprueba profundidad y presupuesto de forma asíncrona antes de inferir.

## APIs de triggers

```js
const trigger = await objective.triggerAsync('receipts', { kind: 'event' }, callback);
await trigger.enableAsync();
const receipt = await trigger.emit('receipt-123', { amount: 10 });
await trigger.disableAsync();
const state = await trigger.inspectAsync();
```

```python
trigger = await objective.trigger_async('receipts', {'kind': 'event'}, callback)
await trigger.enable_async()
receipt = await trigger.emit('receipt-123', {'amount': 10})
await trigger.disable_async()
state = await trigger.inspect_async()
```

También hay `Trigger.open(...)`, `resolveAsync(...)` / `resolve_async(...)`. Argumentos de resolución e ID de ejecución conservan el contrato anterior. Los constructores y métodos síncronos explícitos siguen disponibles para compatibilidad; pueden bloquear por I/O. Un objetivo local sin store sigue usando su kernel en memoria, sin pretender que una firma async elimine su costo de CPU.

Los triggers continúan desactivados por defecto. Un trigger permite una ocurrencia activa; una pendiente bloquea otra para evitar efectos duplicados. Usar IDs de trigger distintos para fuentes independientes concurrentes. Watch sigue siendo opt-in y no consume modelos mientras espera. Disable conocido despierta la espera local, incluso si el Task Python canceló después de escribir. El watcher registra su espera antes de volver a consultar enabled para evitar perder una desactivación frente a una lectura asíncrona antigua.

## Medición local con concurrencia

```bash
node scripts/measure-objective-runtime.mjs
PYTHONPATH=packages/python/python python3 scripts/measure-objective-runtime.py
```

Los scripts generan [reporte Node](../evals/objective-runtime-node.json) y [reporte Python](../evals/objective-runtime-python.json). Usan HiveDB temporal, cuatro turnos históricos por agente, cuatro tareas con tool por agente, dos verificaciones, dos eventos y un resumen por agente. Las ramas de tareas/verificaciones/eventos/resúmenes concurren bajo el mismo objetivo. Cada fuente de eventos tiene su propio trigger. El contexto se acota a 16 KiB; las respuestas/uso son fixtures sintéticos y no hay proveedores remotos.

| Agentes | Relleno por mensaje histórico | Modelos de warmup | Modelos medidos | Intentos de tools medidos | Modelos totales |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 1024 caracteres | 4 | 8 | 9 | 12 |
| 4 | 4096 caracteres | 16 | 32 | 36 | 48 |

La prueba exige esos contadores y reserva final vacía. Verificaciones, eventos y resúmenes programáticos no hacen inferencias auxiliares. Los tokens sintéticos medidos son 24/16 de entrada/salida con un agente y 96/64 con cuatro; no representan facturación. El reporte incluye warmup para no ocultar las llamadas de preparación.

Un timer de 10 ms mide el exceso observado respecto a su intervalo anterior. En esta ejecución local con cuatro agentes, Node 24.15.0 registró p95 ≈0,59 ms y máximo ≈1,05 ms; Python 3.10.22, p95 ≈1,25 ms y máximo ≈2,15 ms. Los scripts se ejecutaron localmente en paralelo; no son una comparación justa entre runtimes ni una comparación con la implementación anterior. Tiempos, percentiles y número de muestras cambian con la máquina y su carga. No hay umbral de rendimiento en CI ni cota de latencia de producción.

El fixture espera todas sus tareas antes de cerrar el store, también si alguna falla. La primera versión del fixture compartía un trigger entre agentes y recibió el rechazo esperado por una ocurrencia pendiente; la versión actual usa fuentes independientes y conserva esa regla del SDK. No hay fallback ni reintento automático para superar ese rechazo.

## Validación y límites

Tripwires npm impiden usar el puente síncrono del store y el despachador síncrono de Objective durante un flujo completo de delegación, verificación, resumen, eventos y watcher. Python comprueba además que el I/O de control se ejecute fuera del hilo del event loop. Los gates prueban claims/finalizaciones pendientes, cancelación repetida, guards, caché con objetivo detenido, resolución con ID vigente y watcher con lectura antigua.

El cambio no altera formatos durables ni migra datos del SDK anterior. Serializar argumentos, exportar historiales, parsear respuestas e invocar callbacks de usuario sigue teniendo costo en el lenguaje anfitrión. Las APIs síncronas de compatibilidad siguen siendo síncronas; elegir las factories y controles asíncronos evita su I/O en esos flujos. La medición local y las pruebas no equivalen a validar toda la matriz de distribución ni a una evaluación de calidad/costo con modelos remotos.
