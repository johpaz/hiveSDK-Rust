# Scheduler embebido y calendario nativo

El feature opcional `scheduler` del núcleo incluye `harness` y añade cálculo de calendario y publicación durable de trabajos. Rust decide las fechas, deduplicación, estado y política de fechas perdidas. Los bindings Node/Python conducen esas operaciones y ejecutan los callbacks de la aplicación; no requieren gateway ni servicios auxiliares.

## Definiciones y ejecución

`queue.createSchedule(id, spec)` / `queue.create_schedule(id, spec)` guarda una definición inmutable, inicialmente pausada. Repetir ID y especificación conserva estado, próxima fecha y contador; cambiar la especificación produce `SCHEDULE_IDEMPOTENCY_CONFLICT`. Para reemplazar una definición, se pausa la anterior y se crea otra con ID nuevo. No hay edición, borrado o purga implícita del historial.

La especificación contiene `schedule`, `job` y opciones de límites:

```js
const queue = await store.openQueue('calendar');
await queue.createSchedule('daily-review', {
  schedule: { kind: 'cron', expression: '0 8 * * MON-FRI', timezone: 'America/Bogota' },
  job: { kind: 'review', lane: 'daily-review', payload: { prompt: 'Revisa la evidencia pendiente.' }, objective_id: objective.id },
  max_runs: 10,
  misfire_policy: 'skip',
  misfire_grace_ms: 60000,
});
await queue.enableSchedule('daily-review');
const result = await queue.tickSchedule('daily-review');
```

`job` usa el contrato de la cola. El scheduler asigna `not_before`, `idempotency_key` y `payload.schedule`; estos campos no pueden configurarse con otro valor. La metadata contiene ID, `scheduled_for` y `observed_at`. La política de bytes de la cola también debe admitir esa metadata. Los reintentos siguen la política explícita del trabajo; una excepción del callback no se convierte en fallo seguro.

Tick publica como máximo una ocurrencia. El trabajo, su clave idempotente y la siguiente fecha se confirman junto con el coordinador de cola en una transacción CAS. Si falla una cuota, validación, root check o escritura, no se avanza la fecha ni se consume una publicación. Carreras entre handles no duplican el trabajo.

Publicar no invoca ejecutores ni consume modelos. Un worker reclama después el trabajo mediante `runOnce` / `run_once` / `WorkerPool`, cargando el intento al objetivo y respetando lanes, leases y fencing. El callback puede usar `context.runAgent` / `context.run_agent` con un Agent ligado al mismo store y objetivo. El scheduler no instala agentes/tools ni crea una raíz nueva para renovar un presupuesto agotado.

## Calendario y límites

Se admiten:

- `{ kind: 'at', unix_ms }`: fecha única, incluso pasada si se decide explícitamente dispararla.
- `{ kind: 'interval', every_ms, anchor_unix_ms? }`: el anchor es la primera ocurrencia. Sin anchor, la primera fecha es creación + intervalo. Las siguientes conservan su fase.
- `{ kind: 'cron', expression, timezone, dom_and_dow? }`: cinco campos, o seis incluyendo segundos; listas, rangos, pasos, `?`, nombres ingleses de meses/días y domingo 0/7. DOM/DOW usan OR por defecto si ambos restringen, o AND explícito.

Los cálculos cron buscan hasta ocho años. Una hora local inexistente por cambio de horario se omite; una hora ambigua representa su primera ocurrencia. `nextOccurrence` / `next_occurrence` exige un instante estrictamente posterior al observado. Rust también expone `Schedule::next_at_or_after` para fronteras inclusivas. Las zonas proceden de `chrono-tz` 0.9 fijado en Cargo.lock; su catálogo puede diferir del Intl del SDK TS.

Los timestamps son enteros no negativos, hasta 2^53−1; el cálculo cron exige además el rango del calendario de chrono. El intervalo admite 1 ms–365 días. `start_at` es inclusivo y `stop_at` exclusivo. `max_runs` admite 1–10000 publicaciones y cuenta trabajos publicados, independientemente de su éxito. El límite de trabajos retenidos de la cola sigue aplicándose.

`misfire_policy` es `skip` por defecto: una fecha vencida por más de `misfire_grace_ms` se omite sin ejecutar; la gracia predeterminada es 60000 ms y admite 0–24 horas. `fire_once` publica una sola ocurrencia pendiente y conserva su fecha original en la metadata. Ambas políticas avanzan después de la observación actual, sin una ráfaga para recuperar cada fecha perdida.

Mientras el último trabajo no sea terminal, una nueva ocurrencia vencida devuelve `blocked`. Esto incluye `uncertain`, aunque haya expirado el lease. No se presume que el efecto anterior no ocurrió. El consumidor mantiene/reconcilia la cola explícitamente; solo después puede publicarse otra ocurrencia. Tick no expira leases por sí solo. El parámetro opcional de observación sirve para pruebas/integraciones; no puede retroceder respecto al último tick confirmado. No cambia el reloj del objetivo ni permite reclamar un trabajo antes de su not_before. Si no hay fecha vencida o la definición está pausada, devuelve `idle`.

`queue.schedule(id)` / `queue.get_schedule` en Rust devuelve especificación, contador, próxima fecha y último trabajo enlazado. El informe usa `paused`, `active`, `blocked` o `completed`; `enabled` indica por separado si se permite publicar. Un trabajo pendiente puede mantener el estado `blocked` aunque ya no queden más fechas. `listSchedules` / `list_schedules` pagina por tenant y cola, con máximo 200 filas; los cursores no sirven en otro ámbito.

## Sondeo optativo

Abrir, crear o consultar no arranca timers. Para publicar periódicamente:

```js
const stop = new AbortController();
const producer = queue.runScheduler({ pollMs: 1000, signal: stop.signal });
// La aplicación dirige sus workers por separado.
// Al detener el productor:
stop.abort();
await producer;
```

Python ofrece `await queue.run_scheduler(poll_ms=1000, cancel_event=event)` y cancelación de Task; Rust ofrece `queue.run_scheduler(poll_ms, CancellationToken)`. El periodo admite 10–60000 ms. Los bucles paginan definiciones y llaman al kernel de tick; no calculan cron ni deciden misfires en JS/Python. Los wrappers retienen el store hasta cerrar el productor y esperar escrituras en curso.

Un error de almacenamiento o publicación termina el bucle con error. No se oculta ni reintenta una escritura incierta. Parar el productor no cancela los trabajos ya publicados; para eso se usa la API de cancelación de la cola y se espera a los ejecutores. Pausar una definición también detiene únicamente sus publicaciones futuras.

## Relación con el gateway y los triggers

**El cron del gateway corre sobre este mismo kernel** (cola `gateway.cron`, kind `gateway.cron`, `WorkerPool` propio y `run_scheduler`). El contrato con el escritorio no cambia: `cronJobs` sigue siendo la **definición** que se edita y `taskRuns` el **historial** que se lee, pero ya no son el motor.

- **Definición → schedule.** Cada fila activa se refleja como un schedule inmutable `cron.<id>.<huella>`, donde la huella resume tipo, intervalo/cron/zona, `fire_at`, `start_at`, `stop_at` y `max_runs`. Editar una tarea crea un schedule nuevo y retira el anterior (nunca se modifica uno existente); pausar, completar, error o borrar la fila lo desactiva en el mismo instante (las rutas pause/resume/DELETE sincronizan al momento, así que una tarea pausada no dispara en el tick siguiente). `max_runs` se descuenta de `run_count` al recrear. Los `next_run_at` y estados `completed`/`missed`/`error` de la fila se proyectan desde el schedule.
- **Ocurrencia → job.** Cada ocurrencia es un job en el carril `cron.<id>`: no se solapan, una fecha perdida más de 60 s se omite (no se reproduce) y una ocurrencia se publica una sola vez aunque haya varios ticks.
- **`taskRuns` derivado.** Un consumidor del log de eventos de la cola escribe `run-<job>` (`queued`/`running`/`completed`/`failed`/`uncertain`/`cancelled`, `reason` `scheduled` o `manual`, resultado, error y fechas) y cuenta cada ejecución terminada **una sola vez** en `run_count`/`error_count`/`last_error` de la tarea, también tras reiniciar el proceso. El ciclo proyecta antes de sincronizar, de modo que una ejecución que acaba de quedar incierta desactiva su schedule en ese mismo ciclo.
- **Incertidumbre.** Un worker que muere deja su job `running` hasta que vence el lease (2 s en esta cola) y entonces queda `uncertain`; el schedule se desactiva, el carril queda ocupado y el trigger manual responde 409. Nada se reintenta solo: hay que revisar la ejecución. Los rechazos anteriores al despacho (herramienta externa sin adaptador) son fallos conocidos (`failed`).
- **Entrega a canales.** El campo `channel` de la tarea (`all`, `telegram`, `discord`, `slack`, `whatsapp`) envía además el resultado —o un aviso si falla— a los canales activos de su dueño, de modo que una ejecución sin nadie delante no se pierde; ver [channels.md](channels.md). El informe queda en `taskRuns.delivery`.
- **Trigger manual.** Encola un job en el mismo carril; se rechaza (409) si hay una ejecución en cola, en curso o incierta.
- **Retención.** Una cola retiene todos los jobs, así que cada ciclo purga los `gateway.cron` terminados hace más de una hora (el historial queda en `taskRuns`) y poda los eventos ya proyectados.
- **Migración de filas.** No hay que migrar datos: al arrancar se crean los schedules a partir de las filas existentes (un intervalo conserva su `next_run_at` como ancla). Las ejecuciones anteriores (id UUID, no `run-…`) que estaban `running` pasan a `uncertain`; las que estaban `queued` y nunca empezaron pasan a `cancelled` con una nota, para que no se disparen de golpe.

Los triggers de `Objective` conservan su API de eventos/fechas/intervalos y callbacks ligados a un objetivo. No se trasladan sus recibos al almacenamiento del scheduler ni se cuentan dos claims para una ocurrencia de cola. El scheduler embebido utiliza los permisos ordinarios de los jobs y del Agent que ejecuta cada trabajo. Los triggers cron de la API `Objective` están en [triggers.md](triggers.md). El goal runner está en [goals.md](goals.md).

## Validación y referencia TS

La comparación fijada con TS cubre nueve resultados iguales: segundos, rangos/listas/aliases, Bogotá, cambio de horario de Nueva York, bisiesto tras ocho años, domingo 7, DOM/DOW OR/AND y una fecha imposible. Registra además una divergencia intencional: `0 0 */2 * *` respeta los días 1,3,5… en Rust. La referencia TS 0.7.3 ignora esa restricción al combinar los campos y devuelve el día 2 en el fixture. La regresión nativa falló antes de corregir el indicador de campo restringido. No se declara equivalencia completa con el scheduler TS.

Las pruebas verifican atomicidad entre handles, rollback por cuota/bytes, calendario, límites sin renovar presupuesto, paginación/aislamiento, productores cancelables y bloqueo por incertidumbre. La prueba Node → Python → Node termina Node después de un efecto de fixture, exige reconciliarlo en Python y verifica que la siguiente ocurrencia es distinta, sin repetir la primera ni sus cargos. No usa modelos reales ni cuentas. Evidencia: [scheduler-extension.json](../evals/scheduler-extension.json).


Checks locales de esta entrega: 44 pruebas del lib Rust con scheduler (9 nuevas de calendario/publicación/sondeo), 57 del gateway, 10 Node 26 y 10 Python 3.10 seleccionadas. Las tres pruebas de scheduler también pasaron en Node 24 y Python 3.15. Pasaron build del workspace/bindings, formato, Clippy de todos los features/targets, consumidor TypeScript estricto, checks de core sin features/con solo harness y comparación seleccionada con TS. El gateway excluye tres pruebas TCP por las restricciones conocidas y su recuento incluye el helper de subproceso. Python requirió un heartbeat externo para notificaciones de workers en este sandbox. No se certifican CI remoto, proveedores reales, regresión completa de red ni distribución de seis plataformas; el inventario de avisos requiere actualizarse para Cargo.lock antes de publicar.
