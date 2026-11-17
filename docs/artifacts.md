# Artefactos e información de imagen

Un artefacto es un blob opaco que un agente produjo o recibió (una captura, un documento generado, un resultado demasiado grande para el contexto). Los bytes viven en disco y los metadatos en HiveDB, siempre dentro del ámbito de un inquilino. Las transformaciones de imagen necesitan la feature `images` (el crate `image` con solo PNG, JPEG, GIF y WebP); los bindings npm y Python la incluyen.

```js
const artifacts = store.artifacts('/datos/artefactos');
const agent = new Agent({ model, tools: await artifacts.createTools() });
await artifacts.expire();            // retira lo caducado; nada se borra solo
```

```python
artifacts = store.artifacts('/datos/artefactos')
agent = Agent(..., tools=await artifacts.create_tools())
```

En Rust: `SessionStore::artifacts(root)` → `ArtifactStore` (`create`, `get`, `read_bytes`, `read_text`, `list`, `delete`, `expire`, `tool_definitions`, `execute_tool`).

## Contrato

- **Ruta**: `<root>/artifacts/<día UTC>/<id>.<ext>`. El id es un UUID generado aquí y la extensión sale de una lista fija (`png jpg webp gif pdf txt md csv json`, si no `bin`); ningún texto del llamante llega a una ruta. En los metadatos la ruta es relativa al root, así que el directorio se puede mover. Los ids que no son UUID se rechazan antes de tocar nada.
- **Tipo**: el tipo declarado se sustituye por el que muestran los bytes (PNG, JPEG, GIF, WebP, PDF). De las imágenes se leen ancho y alto **solo de la cabecera** (sin decodificar píxeles); cabeceras truncadas, de tamaño cero o con recorridos de marcadores no acotados se tratan como no imagen.
- **Integridad**: se guarda el SHA-256 y el tamaño, y se **vuelven a comprobar en cada lectura**; un archivo alterado o truncado falla con `ARTIFACT_CORRUPT`, uno ausente con `ARTIFACT_MISSING`.
- **Límites**: de 1 byte a 25 MiB por artefacto; `kind` de 1 a 64 caracteres; etiquetas `run_id`/`task_id` de 1 a 256, sin caracteres de control. Se valida todo antes de escribir; un fallo al guardar los metadatos retira el archivo.
- **Escritura**: se crea con `create_new` (nunca sobrescribe) y se sincroniza. No hay deduplicación por contenido.
- **Aislamiento**: la clave incluye el inquilino. Otro inquilino no ve, lee ni borra el artefacto (`ARTIFACT_NOT_FOUND` / `false`) y los cursores de lista son del ámbito que los emitió. Es una capacidad con alcance, no una autorización: el host decide qué inquilino entrega a cada llamante.

## Retención

Siete días por defecto, configurable por artefacto (`expires_in_days`; `null` = para siempre). **Nada caduca por sí solo**: `expire()` retira cada artefacto activo cuya fecha pasó (estado `expired`, `expired_at`, archivo borrado). Primero se registra el nuevo estado y luego se borra el archivo, de modo que un fallo intermedio deja un archivo huérfano, nunca un registro activo sin contenido. `delete` hace lo mismo con estado `deleted`. Leer un artefacto no activo falla con `ARTIFACT_EXPIRED` / `ARTIFACT_DELETED`; los metadatos siguen consultables.

## Herramientas

`artifact_save` (texto o base64; el servidor decide el tipo real), `artifact_inspect`, `artifact_read` (texto, o `encoding: "base64"` para cualquier tipo; el texto rechaza binarios y UTF-8 inválido), `artifact_list` (paginada), `artifact_delete` e `image_info`. Como el resto de herramientas nativas, el Agent loop sigue siendo la autoridad de presupuesto e incertidumbre: guardar no es idempotente (cada llamada crea un artefacto nuevo).

## No implementado

- **Transformaciones de imagen** (redimensionar, recortar, convertir, miniaturas): requieren un códec y no forman parte de este contrato; solo hay metadatos de cabecera.
- Descarga por HTTP y vínculo con el gateway/canales (adjuntar un artefacto a un mensaje de Telegram, etc.).
- Reglas de propiedad por usuario dentro de un inquilino (la referencia filtra por `user_id`; aquí el inquilino es la frontera).
- Barrido automático de caducidad en segundo plano y recuperación de archivos huérfanos.
- Recibos de resultados MCP grandes (`artifact_ref`): el almacén los puede contener, pero el normalizador de MCP no está conectado.

## Evidencia

`crates/hive-core/tests/artifacts.rs` (11 pruebas: cabeceras PNG/JPEG/GIF/WebP y rechazos acotados, tipo detectado, ruta generada, validación previa a escribir, ids no UUID, aislamiento entre inquilinos, hash y archivo ausente, lectura de texto, retención/expiración, borrado, paginación, herramientas con base64 byte a byte y comando por `SessionStore`) y una prueba por binding.

## Transformar imágenes

`artifact_transform_image` (y en Rust `image_transform::apply`) crea un **artefacto nuevo** a partir de uno de imagen; el original no se toca y el resultado hereda `run_id`/`task_id`.

```js
const out = await call('artifact_transform_image', { id, crop: { x: 0, y: 0, width: 800, height: 600 },
  rotate: 90, max_width: 1024, format: 'jpeg', quality: 80 });
// { artifact: {...nuevo...}, source: id, width, height }
```

Operaciones, **en este orden**: `crop` → `rotate` (90/180/270 en sentido horario) → `flip` (arriba↔abajo) → `flop` (izquierda↔derecha) → ajuste dentro de `max_width`×`max_height` → `grayscale` → codificación a `png`, `jpeg` o `webp` (por omisión, el formato de origen). Hace falta al menos un cambio.

- **Ajuste.** Conserva la proporción y **nunca agranda**. Para dejar una foto lista para un modelo basta `max_width`/`max_height` de 1024.
- **Orientación EXIF.** La de un JPEG (foto de teléfono) se aplica antes de todo, así que sale derecha.
- **JPEG** no tiene transparencia: lo transparente se aplana sobre **blanco** (no negro). `quality` 1–100, 85 por omisión; solo afecta a JPEG.
- **WebP se escribe sin pérdida** (mismos píxeles, pero pesa más que un JPEG). Un GIF se lee como su **primer fotograma** y sale como PNG: no hay animación.
- **Se pierde** el perfil de color y los metadatos. No hay AVIF ni HEIC, filtros ni texto.
- **Límites (imagen hostil).** Antes de decodificar, la cabecera debe declarar como mucho **50 megapíxeles**; el decodificador tiene 256 MiB de memoria máxima; el lado de salida, como mucho 16 384; el resultado cuenta contra los 25 MiB del artefacto. Una imagen truncada o corrupta es un error limpio, nunca un pánico. Un recorte fuera de la imagen, una rotación distinta de 90/180/270 o una calidad fuera de 1–100 se rechazan antes de decodificar.
- **Aislamiento.** Lee y escribe en el inquilino del almacén; el de otro no puede transformarla.

**Validación:** `tests/image_transform.rs` con imágenes reales de cuadrantes de color (píxel a píxel): recorte, giros, espejos, ajuste sin agrandar, conversiones y calidad, WebP idéntico al releerlo, transparencia sobre blanco, escala de grises, GIF a PNG, **orientación EXIF 6 inyectada a mano**, cabecera de 100 MP rechazada en milisegundos, cuerpo truncado, entradas inválidas y la herramienta (original intacto, herencia de `run_id`, otro inquilino, no-imagen); más una prueba de extremo a extremo en npm y otra en Python.

