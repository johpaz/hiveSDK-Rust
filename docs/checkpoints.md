# Checkpoints y operaciones inciertas

Las sesiones persistentes guardan el estado después de cada respuesta del modelo y resultado de tool. Un UUID identifica el turno y otro cada operación. El uso reportado, llamadas, observaciones y límites sobreviven a una reanudación.

## Continuar un checkpoint limpio

Detener el ejecutor original y volver a registrar las mismas tools, configuración y proveedor:

```javascript
const agent = new Agent({ ...options, store, agentId: 'a', sessionId: 's', resume: true });
const result = await agent.resume();
```

```python
agent = Agent(**options, store=store, agent_id='a', session_id='s', resume=True)
result = await agent.resume()
```

`resume()` continúa el mismo turno sin otro prompt. Solo se permite para un agente abierto explícitamente en ese modo. Una respuesta terminal ya guardada se entrega sin consultar otra vez al modelo. Una parada por política no habilita otra ejecución ni reinicia presupuesto.

## Si hay una operación incierta

La operación se registra **antes** de despacharla. Un corte entre ese registro y el guardado del resultado es ambiguo: incluso puede ocurrir antes de que el servicio externo reciba la llamada. El SDK rechaza reanudar con `SESSION_OPERATION_UNCERTAIN` tanto para tools como para modelos.

```javascript
const snapshot = store.inspect({ agentId: 'a', sessionId: 's' });
const operation = snapshot.run.in_flight;
// Obtener verifiedResult del sistema externo, usando la identidad de la operación.
store.resolveTool({ agentId: 'a', sessionId: 's', revision: snapshot.revision,
  executionId: operation.execution_id, result: verifiedResult });
// Ahora se puede construir el agente con resume: true y llamar resume().
```

```python
snapshot = store.inspect(agent_id='a', session_id='s')
operation = snapshot['run']['in_flight']
store.resolve_tool(agent_id='a', session_id='s', revision=snapshot['revision'],
    execution_id=operation['execution_id'], result=verified_result)
```

Para una respuesta de modelo disponible externamente, usar `resolveModel({... , reply})` / `resolve_model(..., reply=...)`, con el contrato neutral de `ModelReply`, incluido el uso reportado si está disponible. Esta operación aporta un resultado; no consulta al proveedor.

Las resoluciones exigen la revisión y el ID actuales. Una respuesta tardía de un ejecutor anterior no puede sobrescribir el checkpoint resuelto. Esto no detiene sus efectos externos: finalizar al ejecutor original antes de intervenir.

## Idempotencia y ledger

npm entrega al callback `execute(args, context)` un contexto con `run_id`, `execution_id` y `tool_call_id`. Python permite `Tool(..., with_context=True)` para recibir el mismo segundo argumento. Sin store, los dos primeros campos son null / None.

La integración puede usar `execution_id` como clave de idempotencia si el servicio externo la soporta. El SDK no añade esa garantía a servicios que no la ofrecen, ni reintenta automáticamente llamadas ambiguas.

`store.inspect` devuelve el checkpoint actual, operación pendiente y ledger de tools. Cada entrada tiene ID, llamada, estado `started` / `completed` / `rejected` y resultado. Los rechazos de argumentos no despachan callbacks y se guardan atómicamente con su feedback y contador. Se conserva también un registro por turno: consultar `runId` / `run_id` permite leer turnos anteriores del mismo ámbito. Los resultados pueden contener datos sensibles de la aplicación; no son logs anonimizados.

`store.recover(...)` descarta un turno inconcluso explícitamente, preserva su registro como abandonado y restaura la conversación anterior. No deshace efectos externos. No hay borrado automático de registros históricos; definir retención antes de un despliegue prolongado.

## Compatibilidad y límites

Los registros anteriores de este SDK siguen siendo legibles. Las nuevas ejecuciones escriben el formato de sesión 2. Un turno antiguo sin checkpoint solo puede descartarse con `recover`; no se inventa su estado intermedio.

Una tool pendiente de un checkpoint anterior se vuelve a validar al reanudar y al reclamarla para despacho. Si no cumple el schema actual, pasa a `tool_rejected`; nunca se ejecuta su callback por haber quedado programada con una versión previa.

Los hooks observan acciones antes de despacharlas y pueden repetirse tras reanudar. No tienen garantías de efectos exactamente una vez. La marca de operación precede al proveedor/tool, no al hook; un hook que falla deja un checkpoint limpio.

Las pruebas usan cortes reales de procesos durante callbacks. No cubren pérdida de energía o corrupción del almacenamiento. El ciclo usa un [handle Rust y workers para los checkpoints](durable-handles.md), sin snapshots completos en el puente. Hay [alternativas públicas asíncronas](async-storage.md) de apertura, inspección, recuperación, resolución, Objective y planes. Se conservan APIs síncronas explícitas; el control interno durable ya usa workers y tiene [medición local concurrente](objective-runtime.md), sin certificar latencia de producción. La retención automática sigue pendiente.

`resumeStream()` / `resume_stream()` permiten entregar deltas al continuar un checkpoint limpio. Los deltas parciales no se guardan como respuesta final. Cancelar o agotar el timeout de HTTP conserva la operación incierta hasta reconciliarla; no dispara reintentos. Si una tool cooperativa retorna un resultado conocido tras cancelar, se confirma ese resultado y después se detiene el turno, sin otra consulta al modelo.
