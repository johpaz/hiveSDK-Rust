# Cola durable y pool de ejecutores

El feature Rust opcional `harness` añade DurableQueue y WorkerPool. No necesita gateway, Node/Bun ni dependencias nuevas. Los bindings Node/Python lo incluyen y ofrecen una cola sobre el mismo SessionStore, con callbacks propios de su lenguaje.

```javascript
const store = await SessionStore.open({ path: '/tmp/hive-jobs', tenantId: 'customer-a' });
try {
  const queue = await store.openQueue('jobs', { max_concurrency: 4 });
  await queue.enqueue({ kind: 'echo', lane: 'conversation-1', payload: { text: 'Hola' }, idempotency_key: 'request-1' });
  const outcomes = await queue.runOnce({
    echo: async job => ({ type: 'completed', result: job.input.payload }),
  });
} finally { await store.closeAsync(); }
```

```python
store = await SessionStore.open('/tmp/hive-jobs', tenant_id='customer-a')
try:
    queue = await store.open_queue('jobs', policy={'max_concurrency': 4})
    await queue.enqueue({'kind': 'echo', 'lane': 'conversation-1', 'payload': {'text': 'Hola'}})
    async def execute(job, context):
        return {'type': 'completed', 'result': job['input']['payload']}
    outcomes = await queue.run_once({'echo': execute})
finally:
    await store.close_async()
```

Abrir, encolar o inspeccionar no inicia ejecutores. runOnce/run_once ejecuta un lote acotado; se vuelve a invocar explícitamente para drenar pendientes. La prioridad descendente y el orden de llegada deciden el próximo trabajo elegible; los tipos sin ejecutor y trabajos futuros permanecen pendientes. Una lane admite un propietario activo y el límite global se aplica **por cola**, incluso entre pools que comparten el store. La selección, los reintentos, los estados y los límites viven en Rust; los bindings conducen los callbacks.

Rust permite registrar futuros con WorkerPool::register, ejecutar un lote con run_once, o activar polling con run(poll_ms). stop solicita cancelación cooperativa y las operaciones deben seguir esperándose hasta que los callbacks terminen. Un presupuesto raíz agotado falla su trabajo y el loop nativo sigue atendiendo los demás; fallos de almacenamiento detienen el loop y se propagan. No hay inferencias mientras el pool está ocioso.

## Transacciones, leases y recuperación

Cada mutación escribe el coordinador versionado y los registros de trabajo en un único col_batch de HiveDB. La clave de idempotencia se inserta en esa misma transacción. Claims ligados a un objetivo incluyen también el cargo de un intento de tool al presupuesto raíz; una carrera perdida no consume presupuesto ni deja un claim parcial. Para deduplicar efectos externos entre intentos se usa una clave estable con el ámbito tenant/cola/ID (o una clave de aplicación adecuada); execution_id cambia por intento y es un fence, no esa clave estable. Una clave repetida con la misma entrada devuelve el trabajo original, incluso terminal; cambiar su entrada produce JOB_IDEMPOTENCY_CONFLICT.

Las claves son tuplas JSON de tenant/cola/ID; listados, cursores, deduplicación y referencias no cruzan ese ámbito. El host autoriza al tenant antes de entregar su store. HiveDB tiene un proceso propietario: se comparten handles del mismo DB entre pools; no se prometen aperturas simultáneas de los archivos por varios procesos. Sí se comprueba recuperación tras la salida abrupta de un proceso y reapertura desde otro lenguaje.

Cada claim genera un execution_id único. Heartbeat y finish exigen ese fence y un lease vigente; una respuesta de otro intento o un lease vencido no puede confirmar resultados. cancelExecution/cancel_execution puede registrar cancelación del intento actual incluso si su lease acaba de expirar, para impedir un replay seguro no deseado; un fence de otro intento se rechaza. El arranque no recupera trabajos implícitamente. maintain examina expiraciones explícitamente; run_once lo llama antes de despachar.

El trabajo pasa por pending → running → completed/failed/cancelled. Una cancelación activa pasa primero a cancelling: conserva lane/capacidad hasta que el ejecutor confirme su salida. Los resultados tardíos después de cancelar se descartan. Los drivers renuevan el lease incluso mientras esperan una salida cooperativa lenta; no abandonan el callback para liberar la lane antes de tiempo. Cancelar un callback no revierte efectos externos ni mata hilos síncronos a la fuerza.

Una excepción, un panic o una respuesta no confirmada deja el trabajo uncertain; también un lease perdido sin autorización de replay. Ese estado conserva lane/capacidad hasta reconciliar. reconcile exige el execution_id original y permite aportar resultado, fallo o cancelación. El operador debe detener ejecutores antiguos y comprobar el efecto externo antes de resolver; el fence impide escrituras tardías en HiveDB, no efectos externos de un proceso vivo.

## Reintentos y presupuesto

Los defaults son max_attempts=1 y retry_safe=false. Solo con retry_safe=true y saldo de intentos se reencola un fallo confirmado retryable o un lease expirado sin cancelación. Un callback que lanza una excepción se considera incierto, incluso si declara retry_safe: no se transforma automáticamente en un fallo confirmado. Las reservas y saldos de un objetivo nunca se reinician por reintentar o reabrir.

El backoff parte de retry_delay_ms, se duplica por intento y tiene techo de 24 horas; no usa jitter. Un intento nuevo recibe un fence nuevo. Declarar retry_safe autoriza duplicación segura: un ejecutor antiguo podría seguir actuando después de perder el lease, por lo que una integración debe ser idempotente. La concurrencia de propietarios lógicos no certifica la cantidad de efectos remotos activos durante una partición.

objective_id debe referir a un Objective durable existente en el mismo tenant. Cada claim aceptado consume un intento raíz de tool y comprueba profundidad; el default depth=1 representa una delegación. Heartbeats y finish comprueban cancelación/deadline sin volver a cobrar. Un rechazo de presupuesto se guarda como failed antes de propagar ObjectiveLimit; los callbacks ya despachados se esperan igualmente. Un runOnce puede por ello rechazar tras haber confirmado otros trabajos del lote: consultar get/list para sus resultados durables.

Para ejecutar agentes, el callback usa context.runAgent(agent, prompt) / context.run_agent(agent, prompt). El agente debe estar ligado al mismo store y objetivo durable; se heredan profundidad y cancelación y el motor mantiene el conteo de modelos/tools/tokens. resume es explícito y no reconcilia operaciones inciertas automáticamente. No se crea un objetivo nuevo en cada intento. Los callbacks personalizados siguen siendo código de confianza de la aplicación; sus llamadas externas fuera del Agent no pueden medirse por el SDK.

Los callbacks devuelven un outcome explícito: completed con result, failed con error y retryable opcional, cancelled, o uncertain con error. Confirmar un fallo significa que la aplicación conoce el resultado del intento. Los errores y resultados se validan en Rust. Python ejecuta callbacks síncronos en un executor separado del usado para almacenamiento/heartbeats y espera su terminación; los asíncronos permanecen en el event loop. Node invoca callbacks JS en su event loop; trabajo CPU intenso necesita su mecanismo de ejecución apropiado.

## Política y límites

La política se persiste al crear la cola y debe coincidir al reabrirla; no hay reconfiguración o purga implícita. Los defaults son 4 ejecuciones concurrentes, lease de 30 segundos, timeout de 30 minutos por intento, backoff inicial de 1 segundo, 10 000 trabajos retenidos y 64 KiB por payload/resultado. task_timeout_ms=null desactiva únicamente el timeout de trabajo; no altera el deadline absoluto del objetivo. El timeout se comprueba también al confirmar, aunque el callback termine antes del próximo heartbeat. La cancelación sigue siendo cooperativa.

Se admiten 1–256 propietarios, lease de 100 ms–1 hora, timeout de trabajo de 1 ms–24 horas o null, hasta 100 intentos y payloads/resultados de hasta 1 MiB según política. Los payloads son objetos JSON; los resultados pueden ser cualquier valor JSON. Errores tienen un techo de 4096 bytes, identificadores de 512 bytes y páginas de 200 trabajos. Los estados terminales y las claves idempotentes se retienen: al alcanzar max_jobs se rechazan nuevas entradas hasta que se implemente una estrategia de archivo/purga explícita.

El coordinador contiene metadatos de trabajos no terminales y se reescribe con CAS en cada transición. Esta primera versión prioriza atomicidad y límites claros; aún no tiene medición de throughput de producción ni un índice de scheduling para grandes colas. Los [grupos de especialistas](durable-delegation.md) añaden fan-in y pruebas por tarea; el bus, scheduler y rutas del gateway siguen pendientes.

## Validación

```bash
cargo test -p hive-agent-core --features harness --lib harness --locked
node --test --test-isolation=none packages/npm/test/queue.test.mjs
PYTHONPATH=packages/python/python python3.10 -m unittest discover -s packages/python/tests -p test_queue.py -v
PYTHONPATH=packages/python/python python3.10 tests/queue-parity.py
python3 scripts/compare-ts-parity.py --scenario queue
```

Las pruebas verifican carreras CAS entre handles, presupuesto raíz compartido entre colas, prioridad/FIFO/lanes, expiración y fencing, reintentos seguros, deadlines, cancelación con espera de callbacks, incertidumbre y reapertura. La prueba entre lenguajes termina Node antes de finish, reabre en Python, reconcilia un efecto de fixture y verifica desde Node que ni el efecto ni el cargo raíz se repitieron. La comparación TS/Rust cubre resultados seleccionados del job-store, no equivalencia total de recuperación, defaults, errores o formatos.

Resultados de esta entrega: 18 pruebas nuevas de harness (27 del lib completo con ese feature), cinco pruebas Node de cola en Node 24/26 y seis Python en 3.10/3.15. También pasaron la recuperación entre procesos/lenguajes, la comparación seleccionada con TS, el consumidor TypeScript en strict y Clippy de todos los features/targets. En este sandbox Python requirió un heartbeat externo del event loop, igual que las pruebas de almacenamiento anteriores; no se cambió el runtime de la aplicación para sortear esa restricción. [Evidencia con hashes del código](../evals/queue-extension.json).
