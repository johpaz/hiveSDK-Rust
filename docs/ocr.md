# Lectura de documentos (OCR)

Convierte PDFs escaneados e imágenes con texto en texto. Vive en el **gateway** (`crates/hive-gateway/src/ocr.rs`), no en el motor, y se configura desde la pestaña **OCR** de Ajustes del escritorio (`hive-desktop-rune`, `src/ocr.rs`).

## Qué hace
- Herramienta `ocr_read` **solo para el coordinador** (los especialistas no la reciben), **apagada por defecto**.
- Lee **únicamente dentro de la carpeta** configurada: rutas relativas, sin `..`, sin enlaces simbólicos que salgan, PDF/png/jpg/webp/gif de hasta 25 MB y hasta `max_pages` páginas.
- Cascada: (1) si el PDF **ya trae texto**, se lee en el equipo y **no se envía nada**; (2) si no, se envía al proveedor elegido: un **endpoint de OCR dedicado** (Mistral `/ocr`) o un **modelo de visión** (documento o imagen adjuntos).
- El resultado siempre dice el **método** (`text_layer`, `ocr_endpoint`, `vision_document`, `vision_image`), si salió del equipo (`sent_out`), el proveedor y el modelo. Las páginas que volvieron vacías se listan en `unread_pages`; no se rellenan ni se adivinan. La transcripción pide literalidad y `[ilegible]` donde no se lee.

## Ajustes (`GET/PUT /api/ocr`, solo administradores)
| Ajuste | Por defecto | Nota |
|---|---|---|
| `enabled` | `false` | Para activar hacen falta carpeta absoluta existente y un proveedor/modelo válido. |
| `folder` | — | Raíz de lectura. |
| `provider_id`, `model_id` | — | Deben estar entre los `candidates` y tener adaptador (`supported`). |
| `ask` | `true` | Una aprobación por conversación antes de enviar un documento fuera. |
| `session_minutes` | `10` | Vigencia de esa aprobación. |
| `max_pages` | `50` | 1–200. |

`candidates` lista los modelos habilitados de proveedores activos **con clave guardada** (o Ollama): tipo `ocr` si el modelo declara la capacidad, `vision` si ve imágenes. Un proveedor de OCR sin adaptador aparece con `supported:false` y una nota; no se oculta ni se finge. Hoy solo **Mistral** tiene adaptador de OCR dedicado. Las claves se guardan en Proveedores, nunca aquí.

### Qué proveedores sirven
Sirven los de la app: cualquier modelo habilitado, de un proveedor activo **con clave guardada** (se comprueba que la clave exista, igual que la página de Proveedores; no hace falta descifrarla para listarlo), con capacidad `ocr` (Mistral) o de visión. **PDF escaneado:** Mistral (OCR dedicado) y los modelos de visión de Gemini, Anthropic y OpenAI reciben el PDF entero. El resto (por ejemplo NVIDIA u Ollama) lee imágenes: para ellos el gateway **extrae la imagen que cada página lleva incrustada** (`hive_agent_core::tools::pdf_page_images`) y la lee **una página por llamada** (`method: vision_page_images`, con `pages`, `unread_pages` y `warnings`). Eso cubre lo que deja un escáner: JPEG (se entrega tal cual) y píxeles Flate de gris/RGB 8 bits o 1 bit, con predictores PNG. **No renderiza**: una página dibujada con vectores y sin texto no tiene imagen y se declara como no leída; tampoco se leen imágenes CCITT, JBIG2, JPEG 2000, LZW ni espacios de color CMYK/indexados (se avisa por página, nunca se adivina). Cuesta una llamada por página.

## Seguridad
- Aprobación por conversación (`ask`) con el motor de políticas; una regla `deny` para `ocr_read` siempre gana.
- La política no se salta con rutas absolutas: se interpretan relativas a la carpeta.
- Los errores del proveedor se resumen (`HTTP 500`) sin repetir su cuerpo.

## Verificado y no verificado
- **Verificado** (pruebas del gateway con servidores falsos y PDF generados): ajustes y validación, solo administradores, candidatos, capa de texto sin salida de datos, Mistral falso (cabecera, data URI, páginas base cero), vía de visión, sandbox de carpeta, aprobación única/negada/regla deny/sin `ask`; y pruebas de la pestaña en el escritorio.
- **No verificado:** contra el endpoint real de Mistral ni con un modelo de visión real; la fidelidad del OCR; páginas escaneadas con mucho contenido. Ver `docs/external-testing.md` (4c).
