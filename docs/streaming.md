# Streaming, deadlines y cancelación

El transporte nativo usa reqwest asíncrono sobre Tokio y conserva un cliente por adaptador. `run` sigue usando una respuesta JSON; `stream` solicita SSE cuando el proveedor lo ofrece. No hay reintento automático ni una llamada auxiliar para revisar el resultado.

En Rust, crear `Arc<OpenAiCompatible>`, `Arc<provider::anthropic::Anthropic>` o `Arc<provider::gemini::Gemini>`, llamar `start(request, streaming)` dentro de Tokio y consumir `ModelStream.next().await`. `ModelStream.cancel()` y su Drop cancelan el trabajo. El helper/trait síncrono inicial de este preview fue sustituido por esta interfaz asíncrona.

## Consumir eventos

Los mismos eventos neutrales sirven para Chat Completions, [Anthropic Messages](anthropic.md) y [Gemini](gemini.md). Los protocolos exigen respectivamente `[DONE]`, `message_stop` o STOP más final HTTP/SSE limpio. Los eventos usage son snapshots por solicitud, no incrementos que la aplicación deba sumar; el motor registra el uso final una sola vez. Gemini entrega argumentos de functionCall completos; no habilita generación parcial de argumentos.

```javascript
const abort = new AbortController();
for await (const event of agent.stream('Investiga el problema', { signal: abort.signal })) {
  if (event.type === 'text_delta') process.stdout.write(event.text);
  if (event.type === 'result') console.log(event.result);
}
// Desde otra acción de la aplicación: abort.abort() o agent.cancel().
```

```python
from contextlib import aclosing
cancel = asyncio.Event()
async with aclosing(agent.stream('Investiga el problema', cancel_event=cancel)) as events:
    async for event in events:
        if event['type'] == 'text_delta':
            print(event['text'], end='', flush=True)
        if event['type'] == 'result':
            print(event['result'])
# Desde otra tarea: cancel.set() o agent.cancel().
```

Acciones de ciclo de vida: model, tool, tool_rejected, complete, stopped. Los deltas de texto, fragmentos de tools y eventos de uso aparecen entre ellas. `result` es el último evento y contiene el mismo objeto que devuelve `run`.

Una acción `complete` precede a la confirmación final; el evento `result` acredita que el turno se confirmó. Consumir hasta el final o cerrar el iterador explícitamente. JavaScript lo cierra al salir de `for await`; Python requiere `aclosing` o `await iterator.aclose()` si se abandona antes del final. Un iterador pausado aún pertenece al agente hasta cerrarlo o consumirlo.

`resumeStream()` en npm y `resume_stream()` en Python continúan un checkpoint limpio. Mantienen los contadores y no vuelven a ejecutar tools cuyos resultados ya están confirmados.

## Contrato del proveedor

La implementación de Chat Completions sigue los [eventos de streaming oficiales](https://developers.openai.com/api/reference/resources/chat/subresources/completions/streaming-events). Ensambla texto y llamadas a funciones por índice. Los fragmentos de argumentos son strings parciales; las tools esperan el final, JSON válido y validación de schema antes de ejecutarse.

Se solicita `stream_options.include_usage`. El [contrato de creación](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) indica que el chunk de uso final puede tener choices vacío y que un stream interrumpido puede no entregar ese uso. Los eventos usage son observación; solo el uso del reply ensamblado incrementa las estadísticas del motor.

El adaptador exige finish_reason admitido y `[DONE]`. Un EOF anticipado, truncado/filtro, ID de tool cambiado o argumentos incompletos falla sin ejecutar tools parciales. No hay fallback que haga otra llamada de pago. Los endpoints compatibles deben ofrecer ese protocolo; usar `run` si solo implementan JSON no streaming.

Un proveedor personalizado puede implementar complete, stream o ambos. Stream devuelve deltas/uso y termina con un único `{type: 'reply', reply: ModelReply}`. No emitir eventos después de él. El motor consume reply; el iterador del agente expone el resultado solo después de aplicarlo. Si solo existe complete, stream entrega ciclo de vida y resultado sin simular tokens.

En npm se pasa un segundo argumento `{signal}`. Python conserva callbacks anteriores con un argumento y permite `with_context = True` en el proveedor para recibir un segundo dict `{cancel_event}`. El factory stream devuelve un iterador asíncrono; no es una coroutine que primero haya que esperar.

## Cancelación y límites

- `agent.cancel()` solicita cancelar el turno activo; bool indica si había uno, no que ya haya terminado. Esperar el rechazo de run o cerrar/consumir el iterador antes de reutilizar/cerrar el agente.
- npm usa error `CANCELLED`; Python `asyncio.CancelledError`. Timeout HTTP se expresa como `TIMEOUT` / `TimeoutError`. No se consulta al modelo para explicar una cancelación.
- El cliente aborta la solicitud HTTP y deja de leer. Esto no garantiza que el proveedor remoto deje de trabajar o facturar lo ya iniciado.
- Un claim persistente sin respuesta completa sigue incierto. Los deltas no se convierten en una respuesta final ni se repite automáticamente la petición.
- Tools/callbacks de aplicación cooperan: `context.signal` en npm y `context.cancel_event` en Python. Las señales no se serializan con los IDs del contexto. Si el callback devuelve un resultado conocido tras cancelar, se guarda antes de detener la continuación.
- No se matan threads síncronos por la fuerza. Python espera que el callback síncrono termine y mantiene el agente ocupado. Async callbacks pueden cooperar con la cancelación de su tarea. JavaScript no puede interrumpir código síncrono que bloquea su event loop.

`timeoutMs` / `timeout_ms` vale 60 segundos por solicitud y cubre espera HTTP, lectura y backpressure; es distinto de `policy.max_run_ms`, que fija el deadline global persistido. HTTP usa el menor tiempo disponible. `maxResponseBytes` / `max_response_bytes` limita el cuerpo recibido a 16 MiB por defecto. Cada evento SSE tiene un tope de 1 MiB y la cola nativa 32 eventos. Un consumidor lento recibe backpressure; un timeout/deadline puede cortar una solicitud mientras espera al consumidor.

Los métodos del store y el puente de estado siguen siendo síncronos. Python espera eventos del binding en su executor, sobre un runtime Tokio compartido de dos workers; eso no equivale a eliminar todos los threads puente. Contexto y compactación siguen pendientes. Las [reservas de salida y el deadline global](budgets.md) ya se conservan al reanudar; no equivalen a un límite de facturación de entrada+salida.
