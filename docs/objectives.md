# Objetivos, presupuesto durable y delegación

`Objective` añade un presupuesto compartido en Rust a los límites locales de cada agente. Es opt-in: no crea agentes hijos ni revisores automáticamente. Todas las instancias que comparten el objeto consumen el mismo saldo, también entre turnos completos.

```javascript
import { Agent, Objective } from '../packages/npm/index.mjs';

const objective = new Objective({
  max_model_calls: 6, max_tool_calls: 8, max_depth: 1,
  max_output_tokens: 1024, max_total_output_tokens: 4096,
  max_run_ms: 60_000,
});
const child = new Agent({ model: 'especialista', provider: childProvider });
const parent = new Agent({ model: 'coordinador', provider: parentProvider, objective,
  tools: [{ name: 'consultar', description: 'Consulta al especialista',
    parameters: { type: 'object', required: ['prompt'],
      properties: { prompt: { type: 'string' } } },
    async execute({ prompt }, context) {
      const result = await context.delegate(child, prompt);
      return { content: result.content, status: result.status };
    },
  }],
});
try { console.log(await parent.run('Resuelve la tarea')); }
finally { parent.close(); child.close(); }
console.log(objective.inspect());
```

En Python se usa la misma política y se pasa `objective=objective` al padre:

```python
from hive_sdk import Agent, Objective, Tool

objective = Objective({'max_model_calls': 6, 'max_depth': 1})
child = Agent(model='especialista', provider=child_provider)

async def consultar(args, context):
    result = await context.delegate(child, args['prompt'])
    return {'content': result['content'], 'status': result['status']}

parent = Agent(model='coordinador', provider=parent_provider, objective=objective,
    tools=[Tool(name='consultar', description='Consulta al especialista',
        parameters={'type': 'object', 'required': ['prompt'],
                    'properties': {'prompt': {'type': 'string'}}},
        execute=consultar, with_context=True)])
```

`childProvider` / `parentProvider` y sus equivalentes Python son proveedores configurados por la aplicación. Los callbacks Python que delegan deben ser asíncronos y ejecutar `context.delegate` en su event loop. No se transfiere la función ni el objetivo como metadata JSON a MCP.

## Reglas de consumo

Valores por defecto: 8 permisos de modelo, 16 intentos de tool, profundidad máxima 2, salida por llamada 2048 y salida agregada 8192 tokens. Sin deadline por defecto. Profundidad cero representa al padre; `max_depth: 0` impide hijos. Los límites locales del agente también siguen vigentes.

Antes de invocar el proveedor, un mutex Rust cuenta el permiso y reserva el menor cap entre saldo disponible, límite del objetivo y límite del agente. Los permisos denegados no consumen llamadas. Una cancelación posterior a la reserva la conserva incluso si el proveedor no llegó a empezar. El snapshot cuenta permisos concedidos, no solo respuestas confirmadas; no debe sumarse de nuevo a los contadores locales como si fuera consumo adicional.

Una respuesta conocida liquida la reserva una sola vez mediante su uso reportado. Uso ausente bloquea nuevas operaciones con `usage_unavailable`, conservando la reserva. Salida reportada mayor que el cap también bloquea la continuación. Errores de proveedor conservan el permiso y reserva; no se reintentan. El conteo depende de que el proveedor respete el cap y reporte uso correctamente; no es un techo monetario ni un conteo previo exacto de entrada.

Un despacho denegado por el objetivo produce `ObjectiveLimit` (`OBJECTIVE_LIMIT`); no se convierte en feedback seguido por otra inferencia. Se distingue de las razones de parada del motor local. Si una respuesta durable llega sin uso, el checkpoint local también confirma la parada `usage_unavailable`. Leer el snapshot del objetivo para el diagnóstico global y las estadísticas de cada agente para su turno, sin sumar ambos como consumos distintos.

Las tools rechazadas por JSON Schema también consumen intentos compartidos. Las tools MCP adaptadas recorren la misma ruta que cualquier tool del agente. Llamar directamente a `McpClient.call` o a un proveedor desde código de aplicación queda fuera del driver del agente y de este presupuesto; los callbacks no son un sandbox.

La delegación hereda objetivo, profundidad y señal de cancelación. Un hijo no puede sustituir el objetivo heredado. El padre espera a todos los hijos iniciados por su callback, aunque este no espere sus promesas explícitamente. Al fallar el callback, cancela y espera a los hijos; un resultado de delegación incierto impide otra inferencia del padre. No se permite delegar reutilizando un contexto cuyo callback terminó.

`objective.cancel()` cancela los agentes activos registrados y bloquea nuevas operaciones. El deadline empieza al construir el objetivo, incluye tiempo inactivo y no se reinicia al empezar otro turno. Cancelación de callbacks propios sigue siendo cooperativa. Terminar una llamada no deshace efectos externos.

## Alcance de persistencia

Sin store, el objetivo solo existe dentro del proceso. Para conservarlo, crear un objetivo con store e ID explícitos:

```javascript
const store = new SessionStore({ path: './data/sdk', tenantId: 'empresa' });
const objective = new Objective(policy, { store, id: 'objetivo-123' });
const agent = new Agent({ model, provider, objective, store,
  agentId: 'asistente', sessionId: 'conversacion' });
```

```python
store = SessionStore('./data/sdk', tenant_id='empresa')
objective = Objective(policy, store=store, objective_id='objetivo-123')
agent = Agent(model=model, provider=provider, objective=objective, store=store,
              agent_id='asistente', session_id='conversacion')
```

Importar SessionStore además de los tipos del ejemplo inicial. Reabrir con el mismo ID y política recupera saldos, deadline original y cancelación; no crea otra asignación. El ID está separado por tenant. La sesión conserva la vinculación a ese objetivo y no se puede reabrir omitiéndolo o sustituyéndolo. Usar la misma instancia de SessionStore para el objetivo y sus agentes persistentes; un objetivo sin store no puede asociarse a una sesión persistente.

El claim de una operación y su reserva global se guardan en una sola transacción con CAS. El permiso de modelo queda en `operation.objective_permit`. La respuesta, checkpoint y liquidación de uso se confirman juntos; rechazos de schema y su contador raíz también. Los ámbitos del handle comparten un guard para estas transacciones breves, sin serializar las solicitudes externas.

Si el proceso muere, la reserva queda retenida. Reabrir no consulta otra vez al proveedor: primero se requiere [reconciliar la operación](checkpoints.md). `resolveModel` / `resolve_model` confirma el resultado verificado y liquida su permiso raíz en la misma transacción, una sola vez. Uso ausente conserva la reserva, detiene el objetivo y produce `stopped/usage_unavailable` en la sesión durable. Abandonar una sesión no devuelve presupuesto al objetivo.

Registrar el objetivo también en la construcción de hijos persistentes. Pueden usar otro wrapper Objective del mismo store e ID, sin otra asignación. La ejecución hereda la instancia activa del padre para cancelar sus descendientes. El checkpoint del hijo conserva su profundidad, incluida una reanudación directa posterior. Un hijo sin store puede usar el presupuesto raíz durable heredado, pero no conserva su propio historial/checkpoint: si pierde la respuesta, sus permisos siguen consumidos y el resultado del padre exige reconciliación.

`inspect()` sigue siendo diagnóstico: no hay importación de snapshots ni reset/liberación automática de reservas inciertas. Cancelar un wrapper avisa a los agentes registrados en esa ejecución y guarda el estado terminal del objetivo. Si otro ejecutor usa un wrapper distinto, la marca durable bloquea sus siguientes claims; la aplicación debe señalar también a ese ejecutor para abortar su solicitud activa. Los procesos externos no se controlan a distancia mediante un documento de HiveDB.

El store rechaza `close` mientras haya agentes registrados con ese objetivo. Cerrar los agentes después de que terminen y luego el store. Los callbacks y las credenciales siguen registrándose otra vez al reabrir; no se guardan en el ledger del objetivo.
