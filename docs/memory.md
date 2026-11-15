# Fuentes, recuperación local y resúmenes proporcionados

El SDK conserva el historial completo y permite seleccionar contexto explícitamente entre turnos. La búsqueda y la instalación de un resumen proporcionado no ejecutan modelos, embeddings ni callbacks. También se puede [generar un resumen mediante un callback presupuestado](summary.md), con ledger, cancelación y uso explícito del objetivo raíz. Una inferencia realizada por la aplicación fuera de ese ámbito no se considera gratuita ni medida por este.

```js
// Después de terminar los turnos: los índices son los de agent.messages.
const source = agent.captureContext(1, 5);
const found = agent.searchContext('pedido R7', { maxTurns: 2, maxBytes: 16384 });
agent.setContextPlan({
  summary: { id: 'summary-v1', source, content: 'R7 se entrega el viernes; R8 está pagada.' },
  retrieved: found.hits.map(hit => hit.source),
});
await agent.run('¿Qué queda pendiente?');
// Restaura la selección normal para el siguiente turno:
agent.setContextPlan(null);
```

```python
source = agent.capture_context(1, 5)
found = agent.search_context('pedido R7', max_turns=2, max_bytes=16384)
agent.set_context_plan({
    'summary': {'id': 'summary-v1', 'source': source,
                'content': 'R7 se entrega el viernes; R8 está pagada.'},
    'retrieved': [hit['source'] for hit in found['hits']],
})
await agent.run('¿Qué queda pendiente?')
agent.set_context_plan(None)
```

Los índices del ejemplo presuponen un mensaje system y dos turnos simples terminados. En una conversación con tools hay más mensajes: usar los índices reales o fuentes devueltas por búsqueda, sin cortar sus pares llamada/resultado.

## Identidad de las fuentes

`ContextSource` contiene `start`, `end` exclusivo y `sha256`. Solo se capturan turnos user completos y cerrados; se excluye el turno activo o detenido parcialmente. Una fuente de resumen debe cubrir un prefijo desde el primer user. Cada fuente recuperada corresponde exactamente a un turno, sin duplicados, con un máximo de 16 por plan. `readContext` / `read_context` devuelve sus mensajes originales y vuelve a comprobar la identidad.

SHA-256 se calcula sobre el JSON compacto de los mensajes, con claves ordenadas. Los números integrales exactamente portables se normalizan (`1.0` → `1`, `-0.0` → `0`). Incluye argumentos, respuestas de tools y estado del proveedor, incluidas firmas. Cambiar un valor cambia la identidad; anexar turnos no altera fuentes previas. El hash verifica la fuente, no la exactitud del texto resumido. ID de resumen: 1–256 bytes; contenido no vacío de hasta 64 KiB UTF-8.

## Búsqueda local

La búsqueda compara términos alfanuméricos en minúsculas con contenido de mensajes, nombres de tools y argumentos JSON. No usa stemming, embeddings ni inferencia semántica. No busca claves estructurales del mensaje o firmas opacas. Cada término distinto coincidente suma uno; a igual puntuación se prefiere el turno más reciente. Query: 1–32 términos y hasta 4096 bytes. `maxTurns` / `max_turns`: 1–64, por defecto 4. `maxBytes` / `max_bytes`: 2–1048576, por defecto 65536.

Devuelve `hits`, con fuente, puntuación y mensajes completos. `serialized_bytes` cuenta exactamente el JSON UTF-8 compacto de `hits`, incluida metadata. Un turno que no cabe se omite entero; no se corta una tool ni su provider_state. La búsqueda no modifica historial, plan ni consumo de modelos.

## Selección y presupuesto

El resumen sustituye el prefijo cubierto solo en la solicitud. Se inserta como datos JSON de un mensaje user con `context_summary`, ID y fuente; no añade instrucciones system. Las fuentes recuperadas conservan sus mensajes originales y orden, incluso dentro del prefijo resumido. Las fuentes recuperadas fuera de ese prefijo se mantienen una sola vez. Todo el resto del historial continúa sujeto a la selección normal.

Sistema, resumen, fuentes recuperadas y turno activo quedan protegidos. Si `drop_oldest_turns` necesita espacio, elimina otros turnos antiguos completos. Si lo protegido no cabe, devuelve `context_limit` o `input_limit` antes de otra inferencia. Todos los datos realmente enviados participan en bytes, framing y BPE, incluidos el resumen y su procedencia. `request.context` informa `summary_id`, `retrieved_sources` y cuántos mensajes originales se omitieron, sin contar el mensaje sintético como historial nuevo.

`agent.messages` permanece íntegro. Leer `contextPlan` / `context_plan` devuelve una copia. La modificación se admite entre turnos, sin ejecutor activo ni checkpoint pendiente de reanudación. En modo durable se guarda con CAS: otro escritor o un checkpoint activo impiden sustituirla. Reabrir conserva el plan y no ejecuta trabajo auxiliar.

Las sesiones con memoria usan formato 4; los checkpoints atribuidos a generación de resúmenes usan formato 5. Los lectores nuevos admiten 1–5. Quitar el plan conserva el formato necesario para cercar escritores que ignorarían datos todavía presentes en checkpoints. El ledger de objetivos usa formato 4 desde el [kernel de resúmenes presupuestados](summary-kernel.md). La comparación de snapshots admite solo representaciones integrales equivalentes dentro del rango portable; conserva conflictos por revisiones, valores distintos y pérdida de precisión de enteros grandes.
