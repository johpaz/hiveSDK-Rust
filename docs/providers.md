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
| `ollama` | OpenAI chat, o `/api/chat` nativo (`kind: ollama`) | `http://localhost:11434/v1` | común |

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
- **Ollama** tiene dos vías: su endpoint compatible con OpenAI (`/v1`, el preset de arriba) y el **protocolo nativo** `kind: "ollama"` (ver «Ollama nativo»). El gateway usa el nativo.
- **`hiveagents`** como preset es la nube HiveAgents compatible con OpenAI; el `kind: "hiveagents"` existente sigue siendo el backend local administrado.
- El SDK no envía `temperature`, por lo que no hay regla de temperatura fija para Kimi.

## Razonamiento, llamadas en texto e historial huérfano

- **`reasoning_content` de ida y vuelta.** La respuesta de un modelo que razona (`reasoning_content`, también por streaming) se conserva en `provider_state` (`{"provider":"reasoning","content":…}`, persistido con el mensaje). **`round_trip_reasoning`** lo reenvía con el mensaje del asistente y sus llamadas: activo en `deepseek` y `kimi`. En todos los demás perfiles se **elimina** del cuerpo (nunca lo ven). Solo viaja por proveedores compatibles con OpenAI: Anthropic y Gemini siguen rechazando el estado de otro proveedor.
- **Interruptores de pensamiento (NVIDIA).** `thinking: true` + `thinking_kwargs` (el preset `nvidia` trae la tabla verificada de la referencia: `glm` → `enable_thinking`+`clear_thinking:false`; `minimax` → `thinking_mode:"enabled"`; `kimi`/`deepseek` → `thinking:true`; `qwen`/`qwq` → `enable_thinking`) añaden `chat_template_kwargs` según el modelo (insensible a mayúsculas; gana la primera regla). **Apagado por defecto** y las familias que no están (Nemotron ya razona) **no reciben nada**, porque una clave que su plantilla no acepta rompe la llamada. Basta `{"preset":"nvidia","profile":{"thinking":true}}`: el perfil del llamador ahora **se fusiona** clave a clave con el del preset (antes lo reemplazaba entero; sus claves siguen ganando).
- **Llamadas a herramientas escritas como texto** (`text_tool_calls`, activo en `ollama`, apagado en el resto). Si la respuesta no trae llamadas reales, se leen los bloques `<tool_call>{…}</tool_call>` o una respuesta que sea solo ese JSON (en cerca de código o como lista). Solo cuenta una llamada a **una herramienta de esa petición** con argumentos en forma de objeto (o de cadena JSON); lo demás, incluido JSON que no es una llamada, queda como texto. Se retira del texto lo que se convirtió. Es opcional porque un modelo que solo *cita* una llamada no debe ejecutarla.
- **Historial huérfano.** Antes de enviar, el cuerpo OpenAI se limpia: un mensaje `tool` sin su llamada anterior se descarta (una ventana o compactación puede dejarlo suelto), una llamada sin respuesta se quita (y el mensaje, si no queda texto) y una respuesta repetida se envía una vez. **Solo la petición**: el historial guardado no cambia, y uno completo sale idéntico.

## No implementado (sigue pendiente)

La reintento sin herramientas sigue sin implementarse a propósito.

## Evidencia

`crates/hive-core/tests/provider_profiles.rs` (10 pruebas contra un servidor local: catálogo y validación de configuración, normalización y mapa inverso con reglas de la referencia, colisiones, ajustes de esquema, `tool_choice`/paralelismo, rechazo de modelos sin tools, cabecera de sesión, streaming) y una prueba por binding. **No se ejecutó contra ningún proveedor real**: las URL y quirks proceden de la referencia local y no se han verificado en vivo.

## Ollama nativo (`kind: "ollama"`)

`POST {base_url}/chat` (`base_url` termina en `/api`, por ejemplo `http://localhost:11434/api`), con NDJSON al hacer streaming. Existe porque es la vía que da lo que el endpoint compatible no: **`num_ctx`** (sin él Ollama recorta el prompt a su ventana por defecto, muy pequeña, y no avisa), `keep_alive`, `think` y la salida estructurada como un esquema simple.

```json
{ "kind": "ollama", "base_url": "http://localhost:11434/api", "num_ctx": 16384, "keep_alive": "10m", "think": false }
```

- **Imágenes** como base64 en el mensaje del usuario; las URL, el audio y los documentos se **rechazan** en vez de descartarse. El texto de `thinking` del modelo no se conserva ni se reenvía.
- **Llamadas a herramientas** completas (no en fragmentos); los servidores que no les dan id reciben uno aquí, único en la conversación. El resultado de una herramienta se envía con `tool_name`.
- **Una respuesta cortada** (`done_reason: length`), un flujo que termina antes de `done`, una línea de error o un JSON roto son un error, nunca una respuesta parcial; el texto de error del servidor no se reproduce.
- `api_key` es opcional (solo para Ollama alojado); acepta `resilience`; el presupuesto de entrada usa el mismo enmarcado que OpenAI.
- **Gateway:** un proveedor `ollama` en HiveDB usa el protocolo nativo. `http://host:11434`, `…/v1` y `…/api` nombran el mismo servidor, y la ventana de contexto del modelo (`context_window` o `num_ctx` del proveedor) se pide explícitamente como `num_ctx`.
- **Verificado** con un servidor falso (6 pruebas), a través del gateway, y contra un **Ollama real (0.40) con un modelo local**: respuesta, streaming y una llamada de herramienta con su resultado (`crates/hive-core/tests/ollama_live.rs`, opt-in con `HIVE_TEST_OLLAMA_MODEL`; úsalo con un modelo local, no uno `:cloud`, que enviaría los prompts a un servicio remoto). No se probó con Ollama alojado ni con imágenes.

## Tope de salida derivado de la ventana de contexto

Si una petición no fija `max_output_tokens`, el proveedor usa: el `default_max_output_tokens` configurado; si no, **el 15 % de `context_window`** (la parte que Hive reserva para la respuesta), entre 1 y 32 768 (`derive_output_cap`); si no hay ventana, el valor de siempre (4096 en Anthropic, Gemini y Ollama; en el compatible con OpenAI no se envía ninguno). Es un campo `context_window` de la configuración del proveedor (en Ollama, la ventana que de verdad se pide, `num_ctx`, tiene prioridad sobre `context_window`, porque una salida no puede pasar de ella); el núcleo no trae un catálogo de ventanas, así que el host la da. Un límite que fija la petición, o el valor configurado, siempre gana, y una ventana o un límite de 0 se rechazan al crear el proveedor. El gateway ya fijaba el tope en cada petición; esto es para quien usa el SDK directamente. Probado con servidores falsos en los cuatro protocolos (`tests/derived_output.rs`).

### De dónde sale la ventana en el gateway

El gateway toma el contexto de la **fila del modelo en HiveDB** (`models.context_window`) y se lo pasa al proveedor, sea del tipo que sea (Anthropic, Gemini, compatible con OpenAI, Ollama o HiveAgents); así el tope de salida derivado y el `num_ctx` de Ollama salen de lo que está guardado, no de un valor fijo. Un `num_ctx` en la fila del *proveedor* es un límite explícito del operador: nunca por encima de lo que el modelo admite. Para **Ollama** se pide la ventana del modelo **hasta 32 768** tokens: un modelo puede admitir 262 144, pero pedirlo entero reserva memoria para una caché de ese tamaño, que un equipo local suele no tener; quien quiera más lo fija en `num_ctx` del proveedor. Una fila de modelo sin `context_window` (muchas del catálogo no lo traen) no pide nada: Ollama usaría su ventana por defecto y el resto de proveedores, el tope de siempre. Probado con servidores falsos para los cuatro tipos y los cuatro casos de Ollama.

### Cómo se rellena `context_window` en HiveDB

1. **Catálogo revisado** (`catalog-2026-11.json`): cada modelo que el SDK conoce trae su ventana, y se mezcla al arrancar **sin pisar** un valor que ya tenga la fila (lo elegido por el operador manda). Los cinco modelos de NVIDIA activos que no la tenían se rellenaron con valores buscados en documentación y fichas: Nemotron 3 Nano Omni 262 144 (NVIDIA), MiniMax M3.1 Flash 1 000 000 (Alibaba Cloud), y Nemotron 3.5 Super VL y GLM-5.3 Flash 1 000 000 (solo agregadores: sin documento oficial a mano). **Nemotron 3.5 Bolt E4B tiene fuentes que discrepan (64 K y 130 K): se guardó 65 536, el valor seguro (nunca se pide más de lo que admite), con una nota; conviene confirmarlo.**
2. **Sincronización** (`POST /api/providers/{id}/sync-models`): lee la ventana del listado del proveedor cuando la trae (`context_window`, `context_length` o `inputTokenLimit`: OpenRouter, Gemini…). **Ollama** no la da en `/api/tags`, así que ahora se pide `/api/show` por cada modelo (hasta 200) y se toma el primer `*.context_length` de `model_info`; un modelo que no se pueda mostrar se deja como está y la sincronización no falla.
3. Lo que ningún listado ni catálogo trae (NVIDIA, por ejemplo) hay que escribirlo a mano en la fila del modelo.

Estado en la base real tras aplicarlo (con copia de seguridad previa): **los 94 modelos de chat habilitados o activos tienen ventana** (antes faltaba en Ollama, Gemini y cinco de NVIDIA).
