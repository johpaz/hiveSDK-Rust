# Goal runner

Un *goal* es un bucle durable **planificar → delegar → sintetizar → revisar** sobre la cola y los grupos de delegación. El SDK **no ejecuta ningún modelo ni proceso por sí mismo**: cada etapa es un job de la cola que ejecutan los workers del host (con `WorkerPool`/`context.run_agent`), y `tickGoal` solo lee sus resultados durables y encola la etapa siguiente.

```javascript
const goal = await queue.createGoal('informe', {
  objective_id: 'root', goal: 'Publicar el informe trimestral',
  planner_agent_id: 'planner', coordinator_agent_id: 'lead',
  specialist_ids: ['research', 'writer'], max_rounds: 3, max_tasks_per_round: 8,
});
// goal.kinds lista los kinds que deben registrar los workers: plan, review y, por ronda, specialist y synthesis.
await queue.tickGoal('informe');   // idempotente; llámalo tras cada job terminado o con un temporizador
```

## Ejecutarlo con tus agentes: `runGoal` / `run_goal`

```javascript
import { runGoal } from '@hive/sdk';
const report = await runGoal(queue, 'informe', {
  agentFor: ({ agentId, sessionId, stage }) =>
    Agent.open({ store, objective, agentId, sessionId, model, provider }),   // se cierra tras cada job
});
```

Python: `await run_goal(queue, 'informe', agent_for=...)`. El bucle hace `tickGoal`, registra los kinds del goal, ejecuta con `runOnce` cada job pendiente a través de `context.runAgent` (que cobra al objetivo raíz) y repite hasta que el goal es terminal, queda bloqueado por un job incierto o no hay nada ejecutable (espera jobs de otros workers). En `plan` y `review` extrae el objeto JSON de la respuesta (limpio, entre ``` o rodeado de texto: `extractJson`); si no hay JSON, el job falla y el goal con él. `specialist` y `synthesis` devuelven `{"text": …}`. No reintenta nada. Nota: la comprobación de sesión que `runAgent` aplica a los jobs de delegación no existe para los jobs `plan`/`review`; `agentFor` es quien debe abrir el agente con el `sessionId` indicado.

## Máquina de estados

`planning → running → reviewing → accepted | failed | cancelled` (y de `reviewing` a `planning` cuando se rechaza una ronda).

| Etapa | Job | Resultado esperado | Transición |
|---|---|---|---|
| plan | `kinds.plan` (agente `planner_agent_id`) | `{"tasks":[{"id","specialist_id","prompt"}]}`, 1..`max_tasks_per_round`, ids únicos, especialistas permitidos | crea el grupo `goal.<id>.r<n>` y pasa a `running` |
| delegación | `kinds.rounds[n].specialist` (uno por tarea) | cualquiera | cuando todas terminan (y, con `require_verification`, tienen prueba durable) → síntesis |
| síntesis | `kinds.rounds[n].synthesis` | cualquiera | encola la revisión |
| revisión | `kinds.review` (`reviewer_agent_id`, por defecto el coordinador) | `{"accepted":bool,"evidence":str,"feedback":str}` | `accepted` (exige `evidence` no vacía) → `accepted`; si no, replanifica con el `feedback` o falla con `MAX_ROUNDS_EXHAUSTED` |

Los textos de especialistas, la síntesis y el feedback se pasan a los prompts **como datos, no instrucciones**.

## Garantías

- **Idempotente y concurrente:** cada transición es un CAS sobre la cola más una clave de idempotencia por (objetivo, etapa, ronda). Seis `tick` simultáneos producen exactamente los mismos jobs que uno.
- **Nada se interpreta a ojo:** un plan sin `tasks`, con ids repetidos, demasiadas tareas o un especialista no permitido, y una revisión sin `accepted` booleano o que acepta sin evidencia, **fallan el goal** con `INVALID_PLAN` / `INVALID_REVIEW`; no se corrigen ni se reintentan.
- **Fallo e incertidumbre:** un job `failed`/`cancelled` falla el goal con su error. Un job `uncertain` (el worker desapareció) **bloquea** el goal (`blocked_uncertain: true`) y no se reencola: se resuelve con `reconcile`, como en la cola.
- **Presupuesto:** todos los jobs cuentan contra el objetivo raíz (`objective_id`, `depth`); rondas ≤ 8 y tareas ≤ 64 por ronda. Un límite de retención o de presupuesto aparece como error del `tick`, sin cambiar el estado.
- **Cancelación:** `cancelGoal` marca el goal primero (un `tick` concurrente ya no encola nada) y cancela los grupos y jobs pendientes; los que corren pasan a `cancelling` y su worker lo ve en su señal de cancelación.
- `createGoal` con el mismo id y la misma spec devuelve el goal existente; con otra spec, `GOAL_IDEMPOTENCY_CONFLICT`.

## API

Rust: `create_goal`, `goal`, `goal_tick`, `cancel_goal` en `DurableQueue`, y los comandos JSON `goal_create|goal_get|goal_tick|goal_cancel`. npm: `createGoal`, `goal`, `tickGoal`, `cancelGoal`. Python: `create_goal`, `goal`, `tick_goal`, `cancel_goal`. Colección `hive_sdk_rust_goals_v1` (versión 1, campos estrictos).

## Validación

8 pruebas Rust (`harness/goal_tests.rs`: camino feliz con recuento exacto de jobs, rechazo y replanificación con feedback, límite de rondas, 6 planes y 3 revisiones inválidos, fallo, job incierto sin reintento, idempotencia, 6 ticks concurrentes, cancelación, despacho JSON), y en npm y Python una prueba de extremo a extremo con `claim`/`finish` y otra con `runGoal`/`run_goal` y un proveedor falso: dos rondas (rechazo con feedback y aceptación), 10 llamadas de modelo cobradas exactamente al objetivo, y un modelo que no responde JSON que falla el goal.

**Alcance honesto:** los resultados de plan y revisión los produce el modelo del host; no se ha evaluado con modelos reales cuánto de fiable es que emitan ese JSON, (el proveedor de las pruebas es un guion). El log de eventos incluye las transiciones del goal (`goal.transition`) y la administración de jobs está en [durable-queue.md](durable-queue.md).
