# Catálogo y ajustes multimodales del escritorio Rust

Revisión: 13 de noviembre de 2026. Ventana solicitada: desde agosto de 2026.

## Comportamiento

El gateway registra de forma aditiva los modelos oficiales revisados en la colección
canónica `models` de HiveDB. El catálogo contiene 94 entradas para los proveedores ya
registrados; no crea proveedores ni credenciales. Incluye algunas entradas anteriores
a agosto necesarias para completar visión y voz. `catalog_reviewed_at` es fecha de
revisión, no de lanzamiento. Se conserva la activación, nombre, precios, contexto,
capacidades existentes y las asignaciones de agentes. Los modelos nuevos se agregan
con `enabled=false, active=false`. Los nombres no verificados no se inventan.

El botón **CONFIG → Actualizar catálogo** consulta el endpoint del proveedor con la
clave cifrada del perfil. No genera respuestas ni ejecuta modelos. Admite la lista
OpenAI, el catálogo paginado de Gemini y Anthropic, la lista de ElevenLabs y las
etiquetas instaladas de Ollama. Respeta los namespaces originales de revendedores,
limita tamaño/páginas y no borra modelos ausentes de la respuesta. Los IDs nuevos sin
capacidades publicadas aparecen como «Capacidades sin publicar» y no se ofrecen a
agentes de texto. Los metadatos de una consulta autenticada se registran aparte de la
fecha de revisión del catálogo integrado.

Imagen y Voz agrupan por `model_type` y capacidades, incluso cuando el proveedor tiene
categoría `llm`. Se muestran modelos desactivados para poder habilitarlos. La selección
predeterminada se guarda por uso (`vision`, `stt`, `tts`) en `config`, por usuario; se
refleja en las claves multimodales originales de sus canales webchat. La visión excluye
los generadores de imagen y endpoints dedicados exclusivamente a OCR. La selección no
cambia al coordinador ni Gemini 3.8 Live. Reuniones respeta el STT guardado cuando no
se solicita uno explícito y ya no fuerza Whisper desde el escritorio.

## Piper

La tarjeta local sustituye la configuración de API key por estado, instalación,
inicio/parada, descarga y selección de las tres voces del original, velocidad, logs
y una prueba de audio con texto editable. Los parámetros `length_scale`, `noise_scale`,
`noise_w` y `sentence_silence` se validan y persisten en HiveDB. La prueba devuelve WAV
real del proceso nativo, se decodifica y suena por la salida elegida en el escritorio.

El servicio está alojado en el gateway autenticado. No necesita Bun, Python ni un
servidor adicional en el puerto 5500. Reutiliza binarios y voces existentes dentro de
`HIVE_TTS_ROOT` o `HIVE_HOME/tts`. La instalación automática conserva el binario original
Piper 2023.11.14-2 para Linux/macOS x64/ARM64. Las voces son Claude, Cortana y Gevy de
[la colección usada por Hive](https://huggingface.co/spaces/HirCoir/Piper-TTS-Spanish/tree/main).
Se descargan a un área temporal y solo se consideran disponibles con ONNX y JSON.
Los procesos tienen límite de tiempo y sus archivos de prueba se eliminan.

Piper trasladó su desarrollo a [OHF-Voice/piper1-gpl](https://github.com/OHF-Voice/piper1-gpl).
Se mantiene el contrato del binario original; no se confunde esa versión con la última
biblioteca de Python ni se reemplaza automáticamente una instalación del usuario.

## Revisión por proveedor

| Proveedor | Entradas revisadas | Modelos / resultado | Documentación oficial |
| --- | ---: | --- | --- |
| gemini | 16 | `gemini-4-carbon`, `gemini-4-argon`, `gemini-4-hydrogen`, `gemini-3.8-flash`, `gemini-3.6-flash`, `gemini-3.5-flash-lite`, `gemini-3.1-flash-lite`, `gemini-3.8-live`, `gemini-3.8-live-extended-thinking`, `gemini-3.8-flash-tts`, `gemini-3.8-flash-lite-tts`, `gemini-3.5-transcribe`, `gemini-nano-banana-2.1`, `gemini-3.1-flash-image`, `gemini-3.1-flash-lite-image`, `gemini-3-pro-image` | [Fuente](https://aistudio.google.com/docs/models) |
| openai | 11 | `gpt-6-astra`, `gpt-6.1-sol`, `gpt-6.1-terra`, `gpt-6-luna`, `gpt-image-2.6-sunburst`, `gpt-image-2.6-flare`, `gpt-live-1`, `gpt-realtime-2.1`, `gpt-transcribe`, `gpt-live-transcribe`, `gpt-realtime-whisper` | [Fuente](https://developers.openai.com/api/docs/models) |
| anthropic | 4 | `claude-fable-5-2`, `claude-opus-5-5`, `claude-sonnet-5-5`, `claude-haiku-5-5` | [Fuente](https://platform.claude.com/docs/en/models/overview) |
| mistral | 3 | `mistral-large-4`, `mistral-ocr-4-1`, `voxtral-mini-tts-3-0` | [Fuente](https://docs.mistral.ai/models) |
| deepseek | 2 | `deepseek-flash`, `deepseek-v4-pro` | [Fuente](https://api-docs.deepseek.com/quick_start/pricing/) |
| kimi | 3 | `kimi-k3.1-pro`, `kimi-k3.1-pro-highspeed`, `kimi-k3` | [Fuente](https://platform.kimi.ai/docs/models) |
| qwen | 12 | `qwen3.8-max`, `qwen3.8-max-0902`, `qwen3.8-flash`, `qwen3.7-plus`, `qwen3.7-flash`, `qwen4-max-preview`, `qwen3.8-omni-flash`, `qwen-audio-3.1-tts-flash`, `qwen-audio-3.1-asr-flash`, `qwen-audio-3.1-asr-flash-filetrans`, `qwen-audio-3.1-asr-flash-streaming`, `qwen-audio-3.1-realtime-flash` | [Fuente](https://www.alibabacloud.com/help/en/model-studio/newly-released-models) |
| z-ai | 4 | `glm-5.3`, `glm-5.3-flash`, `glm-5.3-flashx`, `glm-5.3-flashx-legal` | [Fuente](https://docs.z.ai/guides/overview/overview) |
| minimax | 6 | `MiniMax-M3.1-Flash`, `MiniMax-M3`, `speech-2.8-hd`, `speech-2.8-turbo`, `MiniMax-H3-Image`, `MiniMax-H3-Image-Highspeed` | [Fuente](https://platform.minimax.io/docs/guides/models-intro) |
| groq | 5 | `qwen/qwen3.8-27b`, `whisper-large-v3`, `whisper-large-v3-turbo`, `canopylabs/orpheus-v1-english`, `canopylabs/orpheus-arabic-saudi` | [Fuente](https://console.groq.com/docs/models) |
| elevenlabs | 6 | `eleven_v4`, `eleven_v4_turbo`, `eleven_v3`, `eleven_v3_conversational`, `scribe_v3`, `scribe_v3_live` | [Fuente](https://elevenlabs.io/docs/overview/models) |
| nvidia | 3 | `moonshotai/kimi-k3.1-pro`, `z-ai/glm-5.3-flash`, `z-ai/glm-5.3` | [Fuente](https://docs.api.nvidia.com/nim/reference/llm-apis) |
| opencode-go | 10 | `glm-5.3`, `deepseek-v4-pro`, `glm-5.3-flash`, `kimi-k3.1-pro`, `kimi-k3`, `deepseek-v4.1-flash`, `minimax-m3.1-flash`, `minimax-m3`, `qwen3.8-max`, `qwen3.8-flash` | [Fuente](https://opencode.ai/docs/console/go/) |
| modelscope | 1 | `deepseek-ai/DeepSeek-V4.1-Flash-1017` | [Fuente](https://modelscope.ai/models/deepseek-ai/DeepSeek-V4.1-Flash-1017) |
| openrouter | 5 | `anthropic/claude-opus-5.5`, `google/gemini-4-carbon`, `openai/gpt-6-astra`, `moonshotai/kimi-k3.1-pro`, `z-ai/glm-5.3-flash` | [Fuente](https://openrouter.ai/docs/api/api-reference/models/list-all-models-and-their-properties) |
| ollama | 0 | Local installed models come from /api/tags. No weights are installed or guessed. | [Fuente](https://docs.ollama.com/api/tags) |
| piper | 3 | `es_MX-claude-14947-epoch-high`, `es_MX-cortana-19669-epoch-high`, `es_MX-gevy-10196-epoch-high` | [Fuente](https://github.com/rhasspy/piper/releases/tag/2023.11.14-2) |
| hiveagents | 0 | Public catalog could not be verified. Use authenticated sync against the configured endpoint; no model IDs invented. | [Fuente](https://llm.hiveagents.io/v1/models) |

## Límites comprobados

- HiveAgents: el catálogo público no fue accesible; la consulta autenticada se hace
  desde la app contra el endpoint configurado. No se afirma haber verificado su cuenta.
- Ollama: el catálogo depende de los modelos instalados localmente; no se descargan
  pesos ni se presume que un modelo esté instalado por aparecer en una biblioteca.
- ModelScope: se importa el modelo cuyo ejemplo confirma API internacional. Un
  repositorio de pesos por sí solo no demuestra disponibilidad por API.
- Qwen 4 Max Preview requiere Token Plan, endpoint y clave dedicados; la documentación
  lo limita al 11–25 de noviembre de 2026. Se muestra esa restricción en el modelo.
  Los modelos Qwen de audio dependen de región y contrato.
- OpenCode Go publica endpoints nuevos bajo `inference/go`; se conserva el endpoint
  del perfil. La sincronización comunica errores de endpoint/permisos sin reemplazar
  silenciosamente la URL ni las claves.
- Registrar un modelo de imagen, TTS, STT o realtime no implementa todos los transportes
  particulares del proveedor. HiveLive mantiene el transporte de Gemini; las reuniones
  usan STT compatible con `audio/transcriptions`. Los modelos STT exclusivamente
  realtime no se ofrecen como predeterminados de transcripción por archivo.
- No se hicieron llamadas a modelos ni se instalaron/descargaron binarios o voces en
  el perfil real durante la verificación. El entorno bloquea DNS y sockets TCP locales;
  la instalación, descarga y reproducción real necesitan probarse desde el escritorio.

## Aplicación y pruebas

Reiniciar app y gateway actualizados: contrato de escritorio 4. La migración escribe
en HiveDB al abrir el perfil y puede repetirse conservando las elecciones existentes.

Se verificaron las rutas y persistencia en HiveDB temporales, aislamiento de defaults,
lectura de capacidades en ambas formas del original, namespace de revendedores,
renderizado nativo de las pestañas, ausencia de clave para Piper y síntesis mediante
un proceso local de prueba sin red. Las pruebas de transporte TCP no pueden ejecutarse
en este sandbox. Las fuentes de cada modelo y las restricciones verificadas están en
`crates/hive-gateway/catalog-2026-11.json`.

Validación final: 51 pruebas del gateway aprobadas; 3 pruebas TCP excluidas por
restricciones del entorno. Pruebas nativas de categorías, capacidades y contrato
aprobadas. Clippy del gateway con `-D warnings` y compilación de ambos binarios
completados. El interruptor nativo respeta `active` y `enabled`, con compatibilidad
para respuestas antiguas sin `active`.
