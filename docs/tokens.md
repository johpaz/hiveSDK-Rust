# Presupuesto local de tokens de entrada

El motor Rust puede contar tokens BPE antes de despachar, sin solicitar conteo remoto ni hacer una inferencia auxiliar. La misma policy funciona en npm y Python:

```javascript
const policy = {
  input_token_budget: {
    encoding: 'cl100k_base',
    max_tokens: 8192,
    overhead_tokens: 256,
    framing: 'openai_chat_json',
    max_total_tokens: 24000,
    context_window_tokens: 16384,
  },
  max_output_tokens: 2048,
  context_overflow: 'drop_oldest_turns',
};
```

Es un ejemplo de configuración, no un perfil recomendado para un modelo concreto. La aplicación elige `cl100k_base` u `o200k_base` según su integración. No se deduce a partir de nombres de modelo o alias. Se persiste junto con la policy y el modelo; cambiarlo requiere otra sesión. `input_token_budget: null`/None o ausente desactiva esta política.

## Contrato de conteo

Por defecto, `framing: 'neutral_json'` tokeniza la serialización Rust compacta de `{model,messages,tools}`, en ese orden de campos, que también se usa para medir contexto en bytes. Incluye sistema, JSON Schema, argumentos, resultados y escapes JSON. La librería está fijada a `tiktoken-rs` 0.12.1; los vocabularios están incluidos en el binario y cada codificación se inicializa una sola vez por proceso. No se lee un tokenizer mutable desde disco ni se descargan pesos durante un turno.

Los perfiles `openai_chat_json`, `anthropic_messages_json` y `gemini_generate_content_json` tokenizan la entrada JSON compacta del constructor real de cada adaptador. Se excluyen parámetros de salida (`max_completion_tokens`, `max_tokens`, `generationConfig.maxOutputTokens` y `generationConfig.candidateCount`) y el indicador `stream`; un generationConfig vacío se omite. MIME/schema de salida estructurada permanecen contados. Por tanto el perfil de entrada es igual para JSON y SSE. Incluyen las transformaciones de mensajes, schemas, argumentos/resultados y partes firmadas admitidas por ese protocolo. No incluyen ruta URL, headers ni credenciales. El reporte de bytes sigue contando el contrato neutral. No se infiere un perfil por nombre del modelo; el transporte nativo rechaza un perfil de otro protocolo antes de conectarse. El perfil neutral se admite en todos los adaptadores.

Se usa conteo ordinario: cadenas como `<|endoftext|>` permanecen datos del usuario. El algoritmo y sus codificaciones se pueden revisar en la [documentación de CoreBPE](https://docs.rs/tiktoken-rs/0.12.1/tiktoken_rs/struct.CoreBPE.html) y las [fuentes con vocabularios incluidos](https://docs.rs/tiktoken-rs/0.12.1/src/tiktoken_rs/tiktoken_ext/openai_public.rs.html).

`request.context.token_estimate` contiene `encoding`, `framing`, `serialized_tokens`, `overhead_tokens` y `total_tokens`. El total es la suma de tokens locales y el margen explícito. `max_tokens` debe ser positivo; el margen es cero por defecto y debe ser menor que ese máximo. Codificaciones/perfiles desconocidos o campos inválidos fallan al construir, sin fallback.

## Aplicación del límite

Con `context_overflow: 'stop'`, el valor predeterminado, excederlo devuelve `context_limit` antes de reservar salida o marcar una operación del proveedor. Con `drop_oldest_turns`, se retiran turnos antiguos completos de la solicitud hasta que el conteo local quepa. Sistema y turno activo quedan íntegros. La selección no modifica historial, ledger ni resultados conocidos.

Puede combinarse con `max_context_bytes`: ambos deben cumplirse en la misma selección. Primero se aplica el límite de bytes y luego se recalcula BPE sobre cada candidato necesario; no se suman conteos de fragmentos como si los tokens fueran independientes de sus fronteras. Con la política BPE activa se construye el JSON candidato en memoria para tokenizar; el modo solo bytes conserva el writer sin buffer completo.

Al reanudar se reconstruye el mismo conteo y selección sin descontar otra vez consumo real. La tokenización local consume CPU y el deadline se comprueba también al terminar esa preparación, antes de permitir despacho.

## Alcance

`input_token_budget.max_total_tokens` limita la entrada acumulada de un turno. Antes del despacho la estimación debe caber en el saldo restante; si no cabe y no se pueden omitir más turnos antiguos, se devuelve `input_limit` sin otra reserva de salida. Cada respuesta conocida carga el máximo entre estimación y entrada reportada. Un exceso reportado devuelve `input_limit` antes de tools o continuación; uso ausente devuelve `usage_unavailable`. Esta propiedad anidada no es `LoopPolicy.max_total_tokens`, que mantiene su límite de uso reportado de entrada + salida.

`context_window_tokens` exige `max_output_tokens` o `max_total_output_tokens` explícito en LoopPolicy. Entrada estimada + cap de salida reservado deben caber juntos. Se aplica la misma selección de turnos completos; no se recorta la salida en silencio. Si el turno activo y sistema no caben, devuelve `context_limit` antes de inferir.

`stats.reserved_input_tokens` conserva la estimación de la solicitud pendiente. `estimated_input_tokens` acumula estimaciones de respuestas conocidas y `charged_input_tokens` acumula las cargas preventivas descritas arriba. Son métricas distintas de `input_tokens`, que sigue siendo exclusivamente uso reportado. Repetir preflight o reabrir no acumula otro cobro. Un modelo incierto conserva su reserva durable y exige reconciliación explícita; no se repite para obtener uso. Empezar otro turno reinicia estos límites locales; no reinicia el saldo global de `Objective`.

Los campos nuevos del presupuesto tienen defaults al leer sesiones anteriores. Lectores antiguos que desconocen esos campos los rechazan mediante el contrato estricto del presupuesto antes de escribir; no abrir la misma base con escritores de versiones diferentes.

Este es un conteo exacto del JSON elegido con esa codificación y una **estimación de entrada para el proveedor**. El vendor usa plantillas de chat, codificación y framing que este contrato neutral no reproduce. Un margen configurable no demuestra que cubra esas diferencias. Tampoco se incluyen mensajes que un proveedor personalizado añada fuera del contrato.

No garantiza la ventana real del modelo ni un máximo facturado: aunque el perfil reutilice su JSON, el proveedor puede usar otro tokenizer y plantilla interna. El control combinado usa la estimación elegida y requiere un margen adecuado para la integración. `stats.input_tokens` conserva exclusivamente uso reportado; no se sustituye por la estimación.

El reporte de conteo es metadata para la aplicación; el adaptador HTTP no lo envía al vendor. No hay reintento ni resumen automático. Ver [contexto](context.md) y [reservas de salida](budgets.md).
