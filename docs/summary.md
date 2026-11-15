# Generación explícita de resúmenes

`Agent.summarize`, disponible en npm y Python, genera un resumen solo cuando lo solicita la aplicación. Recibe una fuente de historial, un callback y el `Objective` del trabajo. Devuelve texto ligado a esa fuente y el costo atribuible de los agentes auxiliares. No instala el resumen automáticamente, no lo ejecuta al superar un límite y no añade revisores ni reintentos.

Después de terminar el historial que se quiere resumir:

```js
const history = agent.messages;
const start = history.findIndex(message => message.role === 'user');
const source = agent.captureContext(start, history.length);
const summarizer = new Agent({ model, provider, maxTurns: 1 });
const receipt = await agent.summarize('history-v1', source, async (input, context) => {
  const result = await context.delegate(summarizer,
    `Resume solo los hechos del historial JSON: ${JSON.stringify(input.messages)}`);
  if (result.status !== 'completed' || !result.content) throw new Error('No confirmed summary');
  return result.content;
}, { objective });
console.log(receipt.cost);
agent.setContextPlan({ summary: receipt.summary });
summarizer.close();
```

```python
history = agent.messages
start = next(i for i, message in enumerate(history) if message['role'] == 'user')
source = agent.capture_context(start, len(history))
summarizer = Agent(model=model, provider=provider, max_turns=1)

async def summarize(input, context):
    result = await context.delegate(summarizer,
        f"Resume solo los hechos del historial JSON: {json.dumps(input['messages'])}")
    if result['status'] != 'completed' or not result['content']:
        raise RuntimeError('No confirmed summary')
    return result['content']

receipt = await agent.summarize('history-v1', source, summarize, objective=objective)
print(receipt['cost'])
agent.set_context_plan({'summary': receipt['summary']})
summarizer.close()
```

Python requiere `import json` para el ejemplo. `objective` debe ser el mismo objetivo del agente si este ya está vinculado a uno. Si la fuente es persistente, debe compartir su `SessionStore`. El trabajador puede ser un agente en memoria que hereda el objetivo o uno persistente construido con ese mismo store y objetivo. No hace falta construir un objetivo nuevo para generar cada resumen.

## Callback y resultado

El callback recibe una copia JSON de `{source, messages}` y un contexto con `objective_id`, `summary_key`, `execution_id`, cancelación y `delegate`. Devuelve texto no vacío de hasta 64 KiB UTF-8. Se admiten callbacks programáticos y async; Python también ejecuta callbacks síncronos en su executor. La fuente debe comenzar en el primer user y terminar en un límite de turno cerrado. El input completo tiene un máximo de 1 MiB JSON.

`Objective.summarize(key, input, callback)` ofrece el mismo ejecutor para un `SummaryInput` ya capturado. Este nivel permite resumir otras fuentes de turnos cerrados; para instalar el resultado en un plan, su fuente debe satisfacer el prefijo requerido por ese plan.

El recibo incluye `key`, `execution_id`, `summary` y `cost`. Se valida el hash antes del callback y se conserva el historial original. Una clave confirmada y la misma fuente devuelve el recibo sin ejecutar el callback ni consumir otro intento. Otra fuente con esa clave se rechaza. El hash confirma procedencia, no la veracidad semántica del resumen.

## Costo, profundidad y cancelación

El claim consume un intento del mismo presupuesto de tools que el trabajo principal. `context.delegate` transmite el objetivo, profundidad, cancelación y clave del resumen. Se contabilizan modelos y tools de sus hijos y delegados anidados. Se espera a todos los hijos iniciados, incluidos aquellos que el callback no esperó. Al fallar se cancelan y se espera su cierre. Un agente ocupado se rechaza antes de asumir su ejecución; terminar el resumen no cancela otro trabajo que reutilice un hijo ya terminado.

`cost` informa llamadas de modelo y tools, entrada/salida reportadas, llamadas pendientes, llamadas sin medición y salida reservada. Se asigna por permisos, no por diferencias globales, de modo que no incluye otros resúmenes o tareas concurrentes. `objective.inspect().summary_costs` permite consultar el costo pendiente. Llamar directamente a un proveedor fuera de `context.delegate` no lo convierte en trabajo medido por este ámbito.

Se admiten `signal` en npm y `cancel_event` en Python. `agent.cancel()` cancela esta generación; `objective.cancel()` cancela el objetivo completo. El deadline raíz también se aplica. El store y el agente permanecen ocupados hasta que terminen los callbacks y sus hijos. Python espera a callbacks síncronos incluso ante cancelaciones repetidas; no termina sus threads por la fuerza. Si llega texto conocido tarde, se guarda antes de informar la cancelación.

## Incertidumbre y recuperación

Un callback fallido o texto inválido deja `SummaryUncertain`, subclase de `ToolOperationUncertain`. Repetir la clave pendiente no repite el callback. Un límite raíz se propaga como `ObjectiveLimit`, conservando lo consumido y la entrada pendiente. Para resolver, detener al ejecutor anterior y comprobar el resultado:

```js
objective.resolveSummary({ key, executionId, content: confirmedText });
```

```python
objective.resolve_summary(key=key, execution_id=execution_id, content=confirmed_text)
```

El ID está en `objective.inspect().summaries[key].execution_id`. Un ID obsoleto o una segunda resolución falla. Una llamada de modelo desconocida conserva su reserva; aportar texto no inventa uso ni libera esa reserva. Si se conoce después el uso de ese permiso, el recibo consultado refleja el costo actualizado.

Los checkpoints de hijos conservan `summary_key` y profundidad y usan formato de sesión 5, que cerca lectores sin atribución. Reanudar un checkpoint limpio conserva la cuota y la atribución sin repetir tools conocidas; una operación en vuelo requiere la reconciliación de sesión habitual. Tras confirmar el resultado del hijo, se puede resolver el resumen pendiente sin volver a ejecutar el callback. Los planes de contexto siguen usando formato 4 y el ledger raíz formato 4.

Ver [memoria](memory.md), [kernel](summary-kernel.md) y el reporte determinista [context-contract.json](../evals/context-contract.json). La evaluación de fixtures comprueba conservación de hechos y costo sintético; no acredita calidad ni ahorro de un modelo remoto.
