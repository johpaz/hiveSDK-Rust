# Imágenes, audio y documentos en los mensajes

Un turno de usuario puede llevar adjuntos junto al texto. `agent.run` / `agent.stream` y `begin_run` aceptan el prompt como cadena (forma de siempre) o como objeto; el texto sigue siendo obligatorio.

```js
await agent.run({ text: 'Describe la imagen', attachments: [
  { kind: 'image', media_type: 'image/png', data: base64 },
  { kind: 'image', media_type: 'image/jpeg', url: 'https://example.com/a.jpg' },
] });
```

```python
await agent.run({'text': 'Resume el PDF', 'attachments': [
    {'kind': 'document', 'media_type': 'application/pdf', 'data': b64, 'name': 'spec.pdf'}]})
```

En Rust: `Engine::start_prompt(Prompt::Parts(PromptParts { text, attachments }))` y `SessionStore::begin_run_with_input`. `Engine::start(String)` y `begin_run*` conservan su firma. Los mensajes sin adjuntos se serializan exactamente igual que antes (el campo `attachments` se omite cuando está vacío), así que las sesiones existentes se leen sin cambios.

## Contrato

`Attachment { kind: image | audio | document, media_type, data | url, name? }`. Exactamente uno de `data` (base64 estándar) y `url` (http/https, sin credenciales). `media_type` es `tipo/subtipo` en minúsculas y debe corresponder al tipo (`image/*`, `audio/*`; un documento es cualquier otro). Se valida todo antes de añadir el mensaje: un turno inválido no deja rastro y el proveedor no se llama.

Límites: 8 adjuntos por mensaje, 8 MiB de base64 por adjunto (≈6 MiB decodificados) y 16 MiB de base64 por mensaje. Los adjuntos solo van en mensajes `user`; cualquier otro rol con adjuntos falla en los tres adaptadores en vez de descartarlos. `initial_history` sigue aceptando solo texto.

## Qué acepta cada proveedor

No se simula compatibilidad: una combinación no soportada falla con `INVALID_INPUT` antes de enviar nada.

| Tipo | OpenAI chat | Anthropic Messages | Gemini |
| --- | --- | --- | --- |
| Imagen `data` | `image_url` (data URL) | `image` base64: jpeg, png, gif, webp | `inlineData` |
| Imagen `url` | `image_url` | `image` source `url` | rechazada |
| Audio `data` | `input_audio`: `audio/wav`, `audio/mpeg` | rechazado (sin entrada de audio) | `inlineData` |
| Audio `url` | rechazado | rechazado | rechazada |
| Documento `data` | `file` (`name` → `filename`, por defecto `document`) | solo `application/pdf` (`name` → `title`) | `inlineData` |
| Documento `url` | rechazado | solo `application/pdf` | rechazado |

Gemini solo mapea datos en línea: los `fileData` apuntan a la API de archivos, que no se usa. Que un **modelo** concreto admita visión o audio lo decide el host (el catálogo del gateway expone `capabilities`); el núcleo no lo consulta y el proveedor responderá con su propio error.

## Presupuestos

- **Bytes de contexto** (`max_context_bytes`): cuentan el JSON serializado completo, base64 incluido.
- **Tokens de entrada**: el tokenizador nunca ve los bytes del adjunto (se sustituyen por vacío; las URL sí se cuentan). Cada adjunto suma `attachment_tokens` al `overhead_tokens` de la estimación. Un presupuesto sin `attachment_tokens` **rechaza** cualquier turno con adjuntos: el SDK no inventa un coste por imagen, audio o PDF; elígelo según tu modelo (por ejemplo, el tope documentado de tokens por imagen). `total_tokens` incluye esa asignación y `max_total_tokens`/`context_window_tokens` la respetan. Si el uso real que reporta el proveedor difiere, la facturación acumulada sigue usando el reportado.
- Los adjuntos cuentan en el historial: cada petición posterior los reenvía mientras su turno siga seleccionado, y se multiplica su asignación.

## Persistencia y límites conocidos

- Los adjuntos con `data` se guardan dentro del historial de la sesión y se reescriben en cada checkpoint. Prefiere `url` para archivos grandes cuando el proveedor lo admita.
- Resumen y recuperación de contexto trabajan solo con texto: un turno resumido o descartado pierde sus medios para el modelo. El hash SHA-256 de una fuente sí incluye los adjuntos.
- Las respuestas del modelo siguen siendo solo texto/tools; no hay generación de imagen o audio en este contrato.
- Streaming y reanudación usan la misma ruta de construcción del cuerpo, sin caso especial.
- En Python, validar y serializar un adjunto de varios MiB ocurre en el hilo del event loop; con modo debug de asyncio puede avisar de callbacks lentos.

## Evidencia

`crates/hive-core/tests/media.rs` (10 pruebas: validación, forma de cable sin cambios para texto, tres adaptadores con sus rechazos, solo rol user, presupuesto fijo independiente del tamaño del payload en los cuatro framings, parada por presupuesto, sesión durable y comando serde string/objeto) y `packages/{npm,python}` (4 pruebas cada uno: proveedor recibe adjuntos, validación previa, presupuesto, reapertura de sesión). No se ejecutó contra los servicios reales de OpenAI, Anthropic o Gemini; los cuerpos se comprueban contra la forma documentada de cada API, no contra una respuesta real.

## HiveLive: voz en vivo, visión y pantalla compartida (gateway `/realtime`)

El gateway puentea una sesión de voz con Gemini Live (`gemini-3.8-live`): audio PCM16 a 16 kHz del micrófono, PCM a 24 kHz de vuelta, texto y fotogramas JPEG, con transcripciones de ambos lados, interrupción (`interrupt`) y herramientas (`consultar_a_bee`, `estado_de_la_colmena`, `cancelar_tarea`).

- **Fotogramas de vídeo** (`{"type":"video","data":<base64>,"mimeType":"image/jpeg","source":"camera"|"screen"}`): solo JPEG, hasta 768 KiB. **Un fotograma inválido** (no JPEG, `mimeType` o `source` desconocidos, base64 malo, sin datos o demasiado grande) **devuelve `{"type":"error","error":…}` y la llamada sigue**; antes terminaba la sesión entera, y con una cámara enviando un fotograma por segundo uno corrupto habría colgado la conversación. Un mensaje que excede el límite del propio WebSocket sigue cerrando la conexión (el escritorio envía cámara a 640 px y pantalla a 1280 px, 1 por segundo, muy por debajo).
- **`source` no se reenvía a Gemini**: la cámara y la pantalla llegan como el mismo flujo de vídeo y el modelo las distingue por el contenido y por lo que se le pide. Funciona bien con escenas distintas, pero si una cámara apuntara a un monitor sería ambiguo; marcar el origen con una nota de texto sería una mejora posible, no hecha.

**Validación real (2026-11-16)**, con la clave de Gemini del usuario sobre una copia de su perfil y voces/imágenes sintéticas, **sin micrófono, altavoces ni cámara reales**: sesión lista en 0,5 s; texto→voz (primer audio a 0,8 s); voz→texto→voz con transcripción de ambos lados e interrupción; `estado_de_la_colmena` («hay doce especialistas», los 12 agentes del perfil) y `consultar_a_bee` con el coordinador real (17 × 23 = 391, dicho en voz); **cámara** (describió un triángulo azul y un círculo rojo sobre una base café), **pantalla compartida** (describió la página de ajustes de Hive) y **ambas a la vez** (las distinguió). Con el audio sintético por defecto de `espeak` no reaccionó: la voz robótica no se detectó como habla.

**No probado:** voz humana real y eco, rendimiento sostenido, vídeo continuo de varios minutos, el escritorio y el gateway juntos.

