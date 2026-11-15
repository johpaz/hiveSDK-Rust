# Adaptador nativo Anthropic

`Anthropic` usa Messages API directamente desde Rust. Comparte cliente reutilizable, cola acotada, límites de bytes, timeout, cancelación y deadline global con el transporte existente. No depende de los SDKs Node/Python de Anthropic y no envía Chat Completions a un endpoint Messages.

```javascript
import { Agent, Anthropic } from './packages/npm/index.mjs';

const agent = new Agent({
  model: process.env.HIVE_MODEL,
  provider: new Anthropic({ apiKey: process.env.ANTHROPIC_API_KEY }),
  policy: { max_output_tokens: 2048, max_total_output_tokens: 8192 },
});
const result = await agent.run('Hola');
```

```python
import os
from hive_sdk import Agent, Anthropic

agent = Agent(
    model=os.environ['HIVE_MODEL'],
    provider=Anthropic(api_key=os.environ['ANTHROPIC_API_KEY']),
    policy={'max_output_tokens': 2048, 'max_total_output_tokens': 8192},
)
result = await agent.run('Hola')
```

El modelo y credenciales se proporcionan explícitamente; los ejemplos no seleccionan un modelo ni hacen llamadas al ejecutar las pruebas del proyecto.

## Configuración

| npm | Python/Rust | Valor predeterminado |
|---|---|---|
| `baseUrl` | `base_url` | `https://api.anthropic.com/v1` |
| `apiKey` | `api_key` | Ausente; útil para pruebas/proxies locales |
| `apiVersion` | `api_version` | `2026-10-21` |
| `defaultMaxOutputTokens` | `default_max_output_tokens` | 4096 |
| `timeoutMs` | `timeout_ms` | 60000 |
| `maxResponseBytes` | `max_response_bytes` | 16 MiB |

Se añade `/messages` al base URL y se envían `anthropic-version` y Authorization Bearer cuando hay key. La versión queda fijada explícitamente; no se cambia al recibir un error. Se consultaron la [referencia Messages](https://platform.claude.com/docs/en/api/messages/create), [versiones](https://platform.claude.com/docs/en/api/versioning) y [autenticación](https://platform.claude.com/docs/en/manage-claude/authentication).

El cap neutral de cada solicitud se convierte en `max_tokens` y prevalece sobre el valor predeterminado del adaptador. Este último solo cubre solicitudes sin cap neutral: no sustituye el presupuesto agregado del motor. Timeout y límite de bytes deben ser positivos; la versión no admite valores vacíos o inválidos para un header.

## Mensajes, tools y uso

El prefijo system pasa al campo superior `system`; texto user/assistant se convierte en bloques. Las llamadas pasan a `tool_use` y los resultados consecutivos se agrupan en un mensaje user con bloques `tool_result`, preservando IDs. Los resultados JSON con clave `error` se marcan `is_error`. Los schemas pasan a `input_schema`; el motor sigue validando argumentos antes de ejecutar callbacks. Los resultados deben preceder a cualquier siguiente mensaje; no se permiten huérfanos o resultados duplicados.

El input reportado suma `input_tokens`, `cache_creation_input_tokens` y `cache_read_input_tokens` una vez. La salida es `output_tokens`. No se suman subdesgloses de caché de nuevo ni se convierte ese total en dinero. En streaming los eventos usage son snapshots acumulados de la solicitud: el motor contabiliza solo el reply confirmado, no la suma de snapshots. Uso ausente queda desconocido y activa las reglas existentes de presupuesto.

## Streaming y recuperación

Se aceptan `message_start`, bloques text/tool_use con sus deltas y cierres, `message_delta`, `message_stop` y pings. Se conserva el índice neutral de tools aunque haya bloques de texto antes. JSON de argumentos se ensambla y valida al cerrar el bloque; ninguna tool se ejecuta hasta el `message_stop` final. Se verificaron las reglas de [streaming de Messages](https://platform.claude.com/docs/en/build-with-claude/streaming).

Solo se confirman respuestas con `end_turn`, `tool_use` o `stop_sequence` coherentes con su contenido. EOF, `max_tokens`, contexto agotado, refusals, `pause_turn`, errores del stream y otros bloques/eventos no soportados son errores de proveedor. No se solicita automáticamente continuar, reintentar o cambiar API. Un claim sin reply confirmado permanece incierto y conserva su reserva; se aplican [checkpoints y reconciliación](checkpoints.md).

Reabrir conserva historial neutral, resultados y consumo confirmado. La aplicación reconstruye el proveedor y sus credenciales; no se guardan en HiveDB. No cambian los contratos de Agent, Tool, eventos o RunResult. Metadata local de contexto/BPE y deadline no se reenvía en el body.

## Alcance actual

Soporta texto y tools del cliente en Claude API directa. Thinking y firmas, imágenes/documentos, citas, tools del servidor, continuaciones pausadas, headers beta y autenticación específica de Bedrock/Vertex/Foundry requieren contratos adicionales. Se rechazan contenidos que el historial neutral no puede conservar en lugar de descartarlos. La estimación BPE local no es un tokenizer ni framing de Claude.

Validación con HTTP/SSE local y payloads deterministas. No se hicieron llamadas de pago ni se certificó compatibilidad de cada modelo o plataforma cloud. Cancelar el socket local no prueba que el proveedor remoto deje de generar o facturar.
