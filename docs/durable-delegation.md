# Delegación durable a especialistas

Con el feature `harness`, `DurableQueue` permite crear grupos de especialistas y reunir sus resultados en un trabajo de síntesis. El núcleo Rust guarda el grupo y todos sus trabajos hijos en una transacción. Los bindings Node y Python usan ese mismo contrato; no hay procesos auxiliares.

## Contrato

`createGroup(id, spec)` / `create_group(id, spec)` requiere un objetivo durable existente, un coordinador y entre 1 y 64 tareas con IDs únicos, especialista y prompt. La lista queda cerrada al crearla. Repetir ID y especificación devuelve el mismo grupo; cambiarla produce `GROUP_IDEMPOTENCY_CONFLICT`. El grupo completo tiene un máximo de 1 MiB y cada payload respeta la política de la cola. Los trabajos cuentan para `max_jobs`; si alguno no cabe, no se guarda ninguno.

Rust expone `GroupSpec`, `SpecialistTask` y métodos equivalentes en `DurableQueue`. Node exporta los tipos de especificación, informe y evidencia en sus declaraciones. Python exporta `GroupSpec` y `SpecialistTask`.

Cada grupo tiene `specialist_kind` y `synthesis_kind` propios. Registrar esos kinds en `runOnce` / `run_once` permite compartir cola y límites con otros grupos sin consumir sus trabajos. Los callbacks reciben `payload.prompt` y `payload.delegation`, con `agent_id`, `session_id`, etapa y IDs del grupo/tarea. Las sesiones se derivan de tenant, cola, grupo y tarea: diferentes tareas tienen sesiones distintas aunque usen el mismo especialista.

El callback crea un Agent ligado al mismo store/objetivo y a los IDs asignados. `context.runAgent` / `context.run_agent` comprueba esas vinculaciones antes de ejecutar; hereda la profundidad y cancelación. Las tareas usan `spec.depth` (1 por defecto); el coordinador usa profundidad menos uno. No se reconstruye el presupuesto al abrir un grupo o una sesión. Claims, modelos, tools y verificaciones siguen cargándose al objetivo compartido.

Los callbacks son código de confianza de la aplicación. Deben registrar proveedores, herramientas y especialistas apropiados; un ID de catálogo no instala un ejecutor. Un `RunResult` detenido debe traducirse a un outcome fallido con su motivo; `completed` significa que el ejecutor confirmó el resultado. Las excepciones sin confirmación permanecen inciertas, como en la cola.

## Uso en Node

Este ejemplo recibe el proveedor y el verificador determinista de la aplicación:

```js
import { Agent } from '@johpaz/hive-sdk-rust';

async function delegate({ store, objective, provider, validateReceipt }) {
  const queue = await store.openQueue('specialists');
  const group = await queue.createGroup('turn-42', {
    objective_id: objective.id,
    coordinator_agent_id: 'lead',
    require_verification: true,
    tasks: [{ id: 'review', specialist_id: 'reviewer', prompt: 'Revisa la entrega y su evidencia.' }],
  });
  const execute = async (job, context) => {
    const { agent_id, session_id } = job.input.payload.delegation;
    const agent = await Agent.open({ store, objective, provider,
      model: 'configured-model', agentId: agent_id, sessionId: session_id });
    try {
      const result = await context.runAgent(agent, job.input.payload.prompt);
      return result.status === 'completed'
        ? { type: 'completed', result }
        : { type: 'failed', error: result.stop_reason || 'Agent stopped' };
    } finally { agent.close(); }
  };
  await queue.runOnce({ [group.specialist_kind]: execute });
  const report = await queue.group(group.id);
  for (const { task, job } of report.tasks) {
    if (job.status !== 'completed') continue;
    const input = await queue.groupVerificationInput(group.id, task.id);
    const key = `turn-42:${task.id}:acceptance`;
    await objective.verify(key, input, async () => validateReceipt(job.result));
    await queue.recordGroupProof(group.id, task.id, key);
  }
  // If a task is still running/uncertain or lacks its required proof, this fails.
  await queue.synthesizeGroup(group.id);
  await queue.runOnce({ [group.synthesis_kind]: execute });
  return queue.group(group.id);
}
```

Para más tareas que capacidad de cola, el consumidor dirige los siguientes lotes explícitamente. No hay watcher o bucle automático al abrir un grupo. Un estado incierto requiere inspección y reconciliación; no debe resolverse con un bucle ciego.

En Python las operaciones correspondientes son `queue.create_group`, `queue.group`, `queue.group_verification_input`, `objective.verify`, `queue.record_group_proof`, `queue.synthesize_group`, `queue.cancel_group` y `queue.run_once`. El callback usa `await context.run_agent(agent, prompt)`; se conservan los IDs y campos JSON en snake_case.

## Fan-in, verificación y cancelación

`group()` devuelve tareas en el orden original y sus trabajos, estado de verificación y evidencia. Sin síntesis, el grupo está `waiting`, `blocked_uncertain`, `ready` o `cancelled`. Con síntesis, el informe incluye su trabajo y estado de cola. Abrir o consultar no expira leases ni ejecuta tareas; el consumidor usa el mantenimiento explícito de la cola.

`synthesizeGroup` espera a que todos los trabajos sean terminales. Un resultado incierto ocupa capacidad y bloquea la síntesis hasta reconciliarlo. Los fallos y cancelaciones individuales son resultados terminales que se entregan al coordinador. La publicación del trabajo de síntesis y su referencia en el grupo comparten CAS; incluso entre handles distintos solo se crea uno. Repetir la llamada devuelve ese trabajo, incluido si ya falló o terminó.

La verificación es optativa. Sin una prueba guardada, el resultado dice `unchecked`. Con `require_verification=true`, cada trabajo completado necesita una prueba; los fallidos/cancelados conservan sus motivos sin necesitar una prueba de éxito. Un veredicto fallido permite sintetizar para explicar el fallo: no convierte la entrega en aprobada.

`groupVerificationInput` solo admite trabajos completados y produce una referencia con tenant, cola, grupo, tarea, objetivo, job, ejecución y SHA-256 del resultado JSON guardado. El consumidor usa ese valor como input exacto de `Objective.verify`. `recordGroupProof` lee el ledger durable del objetivo: rechaza checks pendientes, otro input o una verificación de otro ámbito. Para `passed=true`, exige evidencia no vacía (objeto, lista o texto). Un booleano favorable aislado no basta. El verificador sigue siendo responsable de comprobar que la evidencia respalda el resultado.

La prueba es inmutable y se publica antes de la síntesis. Repetir la misma clave es idempotente; sustituirla falla. Si el proceso muere después de verificar pero antes de publicar, puede publicarse después desde el ledger sin volver a ejecutar el check. No se invoca un modelo evaluador automáticamente; esa revisión mantiene el opt-in del objetivo.

El prompt de síntesis contiene resultados, fallos y pruebas completos. No se recortan silenciosamente. Si rebasa `max_payload_bytes`, falla la transacción y el grupo sigue listo sin trabajo de síntesis; la aplicación debe elegir una política adecuada al abrir la cola.

`cancelGroup` cancela los trabajos pendientes y solicita cancelación de los que están ejecutándose, en una sola transacción. Los callbacks se unen antes de liberar sus lanes. Los trabajos inciertos siguen requiriendo reconciliación; cancelar no afirma que sus efectos externos no ocurrieron. El grupo cancelado no publica síntesis nueva.

## Validación y alcance restante

Las pruebas cubren rollback, IDs repetidos, aislamiento, carreras de publicación, bloqueo por incertidumbre, cancelación, límites sin truncar, pruebas vinculadas al resultado y rechazo de evidencia vacía. Node y Python ejecutan agentes con proveedores deterministas. `tests/delegation-parity.py` termina un proceso Node antes de confirmar un hijo; Python reconcilia, verifica y sintetiza, y otro Node comprueba persistencia y ausencia de duplicados. La comparación TS seleccionada cubre espera de todos los hijos, síntesis única y preservación de fallos/estado sin verificar.

El [gateway local](gateway-durable-delegation.md) integra consultas de texto individuales/en paralelo y su narración, usando la continuación del checkpoint padre. Bus general, selección automática de especialistas, revisión iterativa de tareas, goal runner completo y scheduler siguen pendientes. Esta entrega no declara paridad completa con TS ni validación con modelos remotos.

Comprobaciones locales de esta entrega: 35 pruebas del lib Rust con `harness` (8 nuevas de grupos), 24 Node de cola/objetivos/verificación, 7 de cola en Node 24, 24 Python 3.10 seleccionadas y 7 de cola en Python 3.15. Pasaron build, formato, Clippy de todos los features/targets, consumidor TS estricto, las cuatro comparaciones seleccionadas TS/Rust y ambas recuperaciones Node → Python → Node. Python usó un heartbeat externo del event loop por las restricciones del sandbox. Dos pruebas Python antiguas de salida de subproceso quedaron fuera de ese barrido seleccionado; la nueva recuperación de grupos entre procesos sí se ejecutó. No se certifica la regresión completa de red ni CI remoto. Evidencia y hashes: [delegation-extension.json](../evals/delegation-extension.json).
