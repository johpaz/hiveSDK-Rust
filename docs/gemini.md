# Gemini nativo y estado firmado durable

`Gemini` usa GenerateContent de Gemini Developer API directamente desde Rust, para texto y tools del cliente. Comparte transporte, presupuestos, cancelación y checkpoints con los adaptadores anteriores. No depende de google-genai en Python ni de @google/genai en npm.

```javascript
import { Agent, Gemini } from './packages/npm/index.mjs';
const agent = new Agent({
  model: process.env.HIVE_MODEL,
  provider: new Gemini({ apiKey: process.env.GEMINI_API_KEY }),
  policy: { max_output_tokens: 2048, max_total_output_tokens: 8192 },
});
const result = await agent.run('Hola');
```

```python
import os
from hive_sdk import Agent, Gemini
agent = Agent(
    model=os.environ['HIVE_MODEL'],
    provider=Gemini(api_key=os.environ['GEMINI_API_KEY']),
    policy={'max_output_tokens': 2048, 'max_total_output_tokens': 8192},
)
result = await agent.run('Hola')
```

La aplicación elige el modelo. El ejemplo no certifica uno concreto ni hace llamadas durante las pruebas del SDK.

## Transporte y configuración

| npm | Python/Rust | Valor predeterminado |
|---|---|---|
| baseUrl | base_url | https://generativelanguage.googleapis.com/v1beta |
| apiKey | api_key | Ausente; permite proxies/pruebas locales |
| defaultMaxOutputTokens | default_max_output_tokens | 4096 |
| timeoutMs | timeout_ms | 60000 |
| maxResponseBytes | max_response_bytes | 16 MiB |

Se acepta un ID de modelo o `models/<ID>`; se rechazan rutas, queries o fragmentos dentro del ID. Se llama a `models/<ID>:generateContent` o `models/<ID>:streamGenerateContent?alt=sse`. La key viaja en `x-goog-api-key`, no en la URL ni como Bearer. El cap neutral prevalece sobre el default del adaptador y se envía como generationConfig.maxOutputTokens, con un único candidato.

System pasa a systemInstruction, assistant a model y las tools a functionDeclarations con parametersJsonSchema. Los resultados son functionResponse agrupados en user, en orden de llamada. Objetos JSON se conservan; escalares, arrays y null se envuelven como `{result: valor}`. Un objeto error sigue siendo un error para el modelo. No hay adaptación silenciosa de schemas ni consultas de conteo/modelos.

Se consultaron la [referencia GenerateContent](https://ai.google.dev/api/generate-content) y los [tipos oficiales de Google](https://github.com/googleapis/python-genai/blob/main/google/genai/types.py). Este adaptador no implementa Interactions API ni autenticación Vertex/Bedrock/Foundry.

## Firmas y representación neutral

Gemini puede asociar thoughtSignature a partes de texto o functionCall. El SDK guarda el array de partes completo y ordenado en `provider_state: {provider:'gemini', version:1, parts:[...]}`. No mueve firmas, fusiona partes firmadas ni reconstruye la respuesta al enviar resultados de tools. Se conservan texto vacío y partes que contienen solo firma.

`provider_state` es opcional en ModelReply y Message. El motor valida que sus partes representen el mismo contenido visible y llamadas neutrales antes de cambiar contadores o historial. Versiones desconocidas o discrepancias fallan. Texto marcado thought:true no se emite como text_delta ni se incluye en content, pero sigue en las partes durables si el proveedor lo devuelve. La aplicación puede leer esta metadata; no es un campo privado del almacenamiento.

Si functionCall contiene id, functionResponse lo conserva. Si no, el SDK asigna un ID neutral local para coordinar el callback y mantiene nombre/orden en el wire, sin inventar un ID del vendor. Para idempotencia externa usar execution_id durable, no asumir unicidad global de ese ID local.

El estado firmado acompaña al historial y checkpoint, también al pasar de Node a Python. La selección de contexto conserva sistema y turno activo completos; los turnos antiguos se omiten como unidades, con su estado. La metadata sí participa en los límites neutrales de bytes/BPE; esos conteos siguen siendo una estimación del payload del proveedor.

Los adaptadores nativos OpenAI/Anthropic rechazan historial con este estado antes de conectar. No hay migración automática entre proveedores ni un bypass que invente firmas. Proveedores personalizados son responsables de conservar el estado que reciban. Se mantienen legibles los registros anteriores sin ese campo. En Rust, los literales Message/ModelReply nuevos deben inicializar provider_state con None cuando no se usa.

## Streaming y consumo

Texto se emite incrementalmente. Cada functionCall recibido debe ser un objeto completo; se entrega como un tool_call_delta con argumentos completos. No se habilita streaming de argumentos parciales ni ejecución anticipada. Solo se confirma el reply tras STOP, cuerpo HTTP completo y framing SSE cerrado. EOF sin STOP, JSON parcial, seguridad, truncación, múltiples candidatos o continuación automática solicitada son errores sin reintento.

Los errores terminales conservan el código `finishReason` (por ejemplo, `MAX_TOKENS`, `SAFETY` o `MALFORMED_FUNCTION_CALL`) y los contadores acumulados de prompt, candidatos y pensamiento disponibles. Un bloqueo de entrada conserva `blockReason`, incluso si la respuesta JSON no contiene candidatos. No se copian texto generado ni mensajes libres del proveedor al diagnóstico. El gateway de escritorio añade el límite `max_output_tokens` aplicado; reserva el 15% del contexto efectivo de HiveDB hasta 32768 tokens por llamada, con fallback 4096 si no hay contexto. Su presupuesto compartido deriva la cuota de llamadas de las iteraciones del coordinador y de los especialistas habilitados, permitiendo una reserva completa del máximo por llamada admitida. Los defaults del SDK permanecen independientes de esa política de aplicación. Un registro antiguo que solo contiene `truncated, blocked or unsupported finish reason` no permite identificar retrospectivamente la causa.

UsageMetadata es un snapshot acumulado por solicitud. Input suma promptTokenCount y toolUsePromptTokenCount cuando existe. cachedContentTokenCount ya es parte del prompt: no se vuelve a sumar. Output suma candidatesTokenCount y thoughtsTokenCount. Uso ausente permanece desconocido; valores inválidos o contadores decrecientes se rechazan. El motor confirma uso una sola vez y conserva una reserva incierta si la respuesta no se confirma.

Las tools siguen pasando por JSON Schema y por los límites del motor. Un STOP no demuestra éxito de la tarea externa; solo cierra la respuesta. No hay verificador LLM ni otra llamada para conservar o revisar firmas.

## Alcance y validación

Incluye texto, partes firmadas y tools locales. Multimodal, grounding/citas, code execution, búsqueda del servidor, llamadas parciales, herramientas asíncronas, Interactions y configuración explícita de thinking requieren contratos adicionales. Los campos/partes no representables se rechazan.

Validación con HTTP/SSE local, payloads deterministas y claves ficticias. No se hicieron llamadas de pago ni se certifica compatibilidad de cada modelo o servicio remoto. La estimación BPE no reproduce el tokenizer de Gemini y cancelar el socket no acredita que el servicio remoto deje de facturar.
