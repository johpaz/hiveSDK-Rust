# Catálogo de proveedores y perfiles de cable

Un **preset** nombra un proveedor alojado y rellena su protocolo, URL base y perfil; un **perfil** (`OpenAiProfile`) agrupa los pocos ajustes de cuerpo que necesita un proveedor compatible con OpenAI. Todo es opcional: la configuración anterior (`openai_compatible`, `anthropic`, `gemini`, `hiveagents` con `base_url`) sigue funcionando igual y sin perfil produce exactamente el mismo cuerpo que antes.

```js
new PresetProvider('deepseek', { apiKey })                      // URL y perfil del preset
new PresetProvider('openrouter', { apiKey, baseUrl: proxyUrl })  // lo que pases gana al preset
```

```python
PresetProvider('kimi', api_key=key, profile={'parallel_tool_calls': False, 'tool_choice': 'auto'})
```

En Rust: `NativeProvider::from_json(r#"{"preset":"groq","api_key":"…"}"#)`; `OpenAiCompatible::with_profile(config, profile)` sin preset. `provider::presets()` lista el catálogo. Una clave de la configuración que ya exista gana al preset (por ejemplo `base_url` hacia un proxy); un `kind` incompatible con el preset falla.

## Presets

| id | protocolo | URL base | perfil |
| --- | --- | --- | --- |
| `openai` | OpenAI chat | `https://api.openai.com/v1` | común |
| `anthropic` | Anthropic Messages | `https://api.anthropic.com/v1` | — |
| `gemini` | Gemini | `https://generativelanguage.googleapis.com/v1beta` | — |
| `groq` | OpenAI chat | `https://api.groq.com/openai/v1` | común |
| `mistral` | OpenAI chat | `https://api.mistral.ai/v1` | común + quitar `additionalProperties:false` |
| `openrouter` | OpenAI chat | `https://openrouter.ai/api/v1` | común + sin tools: `deepseek/deepseek-r1:free` y modelos `-r1`/`/r1` |
| `deepseek` | OpenAI chat | `https://api.deepseek.com/v1` | común + sin tools: `deepseek-reasoner`, `deepseek/deepseek-r1:free` y `-r1`/`/r1` |
| `kimi` | OpenAI chat | `https://api.moonshot.ai/v1` | común + `parallel_tool_calls:false` |
| `nvidia` | OpenAI chat | `https://integrate.api.nvidia.com/v1` | común |
| `qwen` | OpenAI chat | `https://dashscope-intl.aliyuncs.com/compatible-mode/v1` | común |
| `minimax` | OpenAI chat | `https://api.minimaxi.com/v1` | común |
| `opencode-go` | OpenAI chat | `https://opencode.ai/zen/go/v1` | común + cabecera de sesión `x-opencode-session` y `User-Agent` |
| `hiveagents` | OpenAI chat | `https://llm.hiveagents.io/v1` | `ensure_array_items`, `tool_choice:auto` (sin normalizar nombres) |
| `z-ai` | OpenAI chat | `https://api.z.ai/api/paas/v4` | común |
| `modelscope` | OpenAI chat | `https://api-inference.modelscope.ai/v1` | común |
| `ollama` | OpenAI chat | `http://localhost:11434/v1` | común |

*Común* = `normalize_tool_names`, `ensure_array_items`, `tool_choice:"auto"`. `modelscope` usa `.ai`, no `.cn`: son plataformas con cuentas y tokens distintos.

## Qué hace cada ajuste

- **`normalize_tool_names`**: los nombres que no cumplen `[a-zA-Z0-9_-]{1,64}` se reescriben (cada carácter inválido pasa a `__`, las rachas de `__` se colapsan, se antepone `_` si no empieza por letra o `_`, se corta a 64). Las herramientas, las llamadas del historial y la respuesta final usan el mapa inverso, así que el motor siempre ve el nombre original. Dos nombres distintos que colisionen tras normalizar fallan **antes** de enviar. En streaming, los fragmentos `tool_call_delta` conservan el nombre de cable; solo la respuesta completa se devuelve ya con el nombre original.
- **`ensure_array_items`**: añade `items: {}` a cada schema `{"type":"array"}` sin él (algunos validadores lo exigen).
- **`strip_additional_properties`**: elimina `additionalProperties:false` en cualquier profundidad (una *propiedad* llamada así se conserva).
- **`parallel_tool_calls`, `tool_choice`**: se envían solo si la petición lleva herramientas.
- **`no_tool_models`, `no_tool_r1_models`**: un modelo sin soporte de herramientas **se rechaza** con `INVALID_INPUT` antes de enviar nada si la petición lleva herramientas; sin herramientas no hay restricción.
- **`session_header`, `user_agent`**: la cabecera lleva un identificador de 32 hex generado por instancia del proveedor (estable entre llamadas, distinto entre instancias, nunca derivado de datos del usuario). Las cabeceras que fija el host con `with_headers` tienen prioridad.

## Divergencias deliberadas respecto a la referencia TypeScript

- **Sin reintento sin herramientas** (`retryWithoutToolsOnCodes`): reenviar con otra solicitud cambia la semántica y contradice la política del SDK de no reintentar con otra petición. Un 400/422 se devuelve como error.
- **Modelos sin tools**: la referencia omite las herramientas en silencio; aquí se rechaza la petición.
- **Ollama** usa su endpoint compatible con OpenAI (`/v1`), no el protocolo nativo `/api/chat`; no hay bandera `think`.
- **`hiveagents`** como preset es la nube HiveAgents compatible con OpenAI; el `kind: "hiveagents"` existente sigue siendo el backend local administrado.
- El SDK no envía `temperature`, por lo que no hay regla de temperatura fija para Kimi.

## No implementado (sigue pendiente)

Reenvío de `reasoning_content` entre turnos (DeepSeek, Kimi, MiniMax con razonamiento), interruptores de pensamiento de NVIDIA (`chat_template_kwargs`), lectura de llamadas a herramientas emitidas como texto, saneado de `tool_calls` huérfanas del historial y derivación del tope de salida desde la ventana de contexto.

## Evidencia

`crates/hive-core/tests/provider_profiles.rs` (10 pruebas contra un servidor local: catálogo y validación de configuración, normalización y mapa inverso con reglas de la referencia, colisiones, ajustes de esquema, `tool_choice`/paralelismo, rechazo de modelos sin tools, cabecera de sesión, streaming) y una prueba por binding. **No se ejecutó contra ningún proveedor real**: las URL y quirks proceden de la referencia local y no se han verificado en vivo.
