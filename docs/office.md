# Herramientas Office: Word, Excel y PowerPoint

Lectura y escritura de `.docx`, `.xlsx`, `.pptx` y `.pdf` dentro del sandbox de archivos de [`NativeTools`](native-tools.md). No hay procesos auxiliares: el contenedor ZIP, el XML y cada formato los implementa este crate; la única dependencia nueva es `miniz_oxide` (inflate/deflate, con `adler2`). PDF es de **texto** (ver abajo). No hay formatos binarios antiguos (`.doc`, `.xls`, `.ppt`), macros ni objetos incrustados.

```js
const tools = new NativeTools({ root: '/proyecto', office: {} });   // sin `office`, estas herramientas no existen
await tools.execute('office_write_xlsx', { path: 'ventas.xlsx', sheets: [{ name: 'Q1', data: rows }] });
const book = await tools.execute('office_read_xlsx', { path: 'ventas.xlsx', sheet: 'Q1' });
```

```python
tools = NativeTools({'root': '/proyecto', 'office': {'max_rows_per_sheet': 5000}})
```

## Configuración (`office`)

| Campo | Defecto | Efecto |
| --- | --- | --- |
| `max_input_bytes` | 15 MiB (máx. 64) | Tamaño máximo de un archivo a leer. |
| `max_sheets` | 50 (máx. 1000) | Hojas por libro, al leer y al escribir. |
| `max_rows_per_sheet` | 10 000 (máx. 1 000 000) | Filas por hoja; una hoja mayor se **informa** como error, no se trunca. |
| `timeout_ms` | 30 000 (máx. 300 000) | Plazo cooperativo; se comprueba entre pasos grandes, no interrumpe un paso en curso. |

Las rutas son relativas al root y no pueden salir de él (mismas reglas que `fs_*`); la extensión debe coincidir con la herramienta. Las escrituras reemplazan el archivo de forma atómica y exigen `create_dirs: true` si la carpeta no existe.

## Herramientas

| Herramienta | Entrada | Salida |
| --- | --- | --- |
| `office_read_docx` | `path`, `include_tables` (true) | `text`, `paragraphs: [{text, style}]`, `tables: [filas × celdas]`, `path` |
| `office_write_docx` | `path`, `title`, `paragraphs: [{text, type: title1\|title2\|title3\|paragraph\|list, bold, italic}]`, `tables: [{rows: [{cells}], header}]` | `path`, `bytes` |
| `office_read_xlsx` | `path`, `sheet`, `include_headers` (true), `range` (A1), `dates` (`iso`\|`serial`) | `sheets`, `data: {hoja: filas}`, `total_rows` |
| `office_write_xlsx` | `path`, `sheets: [{name, data: [objetos]\|[arrays], headers}]` | `path`, `bytes` |
| `office_read_pdf` | `path`, `start_page`, `end_page` (máx. 200 páginas por petición) | `total_pages`, `pages: [{number, text}]`, `text`, `metadata`, `warnings` |
| `office_write_pdf` | `path`, `content`, `title`, `page_size` (A4\|Letter), `margin` (0–200 pt, 50), `font_size` (6–72, 12) | `path`, `bytes`, `pages`, `replaced_characters` |
| `office_read_pptx` | `path`, `only_slide` (1-indexada) | `total_slides`, `slides: [{number, title, text, paragraphs}]` |
| `office_write_pptx` | `path`, `presentation_title`, `presentation_subtitle`, `theme: {preset, colors, fonts}`, `slides: [{title, content, points}]` | `path`, `bytes` |

Los esquemas rechazan campos desconocidos. Los nombres propios sustituyen a los de la referencia (`office_leer_docx` → `office_read_docx`, `ruta` → `path`, etc.).

## Seguridad del contenedor

Los archivos son entrada no confiable. Antes de usar cualquier tamaño se comprueba: como máximo 4096 entradas; 32 MiB por entrada y 96 MiB en total descomprimidos; razón de expansión ≤ 200:1 en entradas de más de 1 MiB; CRC-32 y tamaño declarado de cada entrada; nombres UTF-8 únicos de ≤ 512 bytes. Se rechazan el cifrado, ZIP64, los archivos multidisco y los métodos distintos de almacenado/deflate. Nada se extrae a disco, así que los nombres solo se comparan. El XML es estricto: **sin DOCTYPE ni entidades propias** (no hay XXE ni expansión de entidades), profundidad ≤ 256 y ≤ 3 millones de nodos; solo se decodifican las cinco entidades predefinidas y las referencias numéricas válidas.

## Comportamiento por formato

**Word.** Se lee el cuerpo (`w:t`, tabulaciones y saltos de línea), el estilo de cada párrafo (`Heading1`, `ListBullet`…) y las tablas; una tabla anidada pasa a texto de la celda que la contiene. No se leen encabezados/pies, notas, comentarios, cuadros de texto ni texto borrado. Al escribir se genera un paquete mínimo con estilos (Title, Heading 1–3, List Paragraph, Table Grid), viñetas reales (`numbering.xml`) y una fila de encabezado repetible; las filas cortas se rellenan, los saltos de línea y tabulaciones se conservan y los caracteres que XML no admite se descartan.

**Excel.** Se devuelven números, booleanos y cadenas tal como están almacenados. **Las celdas con formato de fecha u hora salen en ISO** (`2025-03-01`, `2025-03-01T14:30:00`, `14:30:00`; con `dates: "serial"` se conserva el número de Excel) y **las fórmulas se devuelven con su valor en caché**: un archivo escrito por una biblioteca que no calculó (por ejemplo openpyxl) deja esas celdas vacías. Con `include_headers` la primera fila (la primera del rango, si lo hay) son las claves; las vacías son `__EMPTY`, `__EMPTY_1`…, las repetidas `nombre_1`…; las filas totalmente vacías se omiten. Esto imita a SheetJS pero **no se ha comparado con él**. Al escribir, las celdas son siempre valores (el texto que empieza por `=` nunca se ejecuta), los objetos anidados se guardan como texto JSON y, como `serde_json` no conserva el orden de inserción, **las columnas salen ordenadas alfabéticamente salvo que pases `headers`**. Reglas de Excel para nombres de hoja (1–31 caracteres, sin `[]:*?/\`, únicos sin distinguir mayúsculas, no `History`), celdas de ≤ 32 767 caracteres y ≤ 1000 columnas.

**PDF.** *Escritura con estilos* (`office_write_pdf`): o bien `content` (texto plano) o bien `blocks`: encabezados 1–3, párrafos con `runs` (negrita, cursiva, subrayado, color, tamaño, familia y **enlaces** como anotaciones `/URI` http(s)), listas con viñetas o numeradas (un nivel anidado), **tablas** (fila de cabecera que se repite en cada página, cebra, `align` y pesos de columna, celdas que envuelven), citas, código, reglas, espaciadores y saltos de página; justificado, control de viudas y huérfanas, apaisado, márgenes por lado, `author`/`subject`/`title` en los metadatos. `theme`: preset (`modern`, `classic`, `corporate`, `minimal`) con colores (hex) y familias (`sans`, `serif`, `mono`) sobrescribibles; `header`/`footer` con texto izquierda/centro/derecha y `{page}`, `{pages}`, `{title}` (maquetado en dos pasadas, así que «Página X de N» es exacto). Las fuentes son las 14 estándar (no se incrusta nada), medidas con las **anchuras AFM reales** de Adobe (`pdf_metrics.rs`, generadas de los AFM Core-14; Oblique comparte tabla con la recta y Courier es 600), por lo que las líneas rompen donde las dibuja el visor; una palabra más ancha que la columna se parte. Codificación WinAnsi: lo que no tiene (emoji, cirílico, CJK, `✓`…) pasa a `?` y se cuenta en `replaced_characters`. Hasta 500 páginas, 10 000 bloques, 5 000 filas × 32 columnas. Sin imágenes ni fuentes incrustadas.

*Lectura*: un **extractor**, no un renderizador. Escanea todos los `n g obj` (gana la última definición de cada objeto, como en las actualizaciones incrementales), expande flujos de objetos, recorre el árbol de páginas en orden (con recursos heredados) e interpreta el texto de los flujos de contenido. Decodifica con el `ToUnicode` de cada fuente o con su codificación simple (WinAnsi, MacRoman y `Differences` con nombres de glifo), y decide saltos de línea y espacios por la **posición y el ancho de los glifos** (`Widths`, `W`/`DW`), de modo que un `Td` que continúa exactamente donde acabó el texto anterior (como hace Chrome) no inserta un espacio falso. Filtros: Flate, ASCIIHex y ASCII85; límites de 32 MiB por flujo, 4 millones de caracteres, 2 millones de objetos, 200 páginas por petición y el plazo de la política.

Lo que no puede leer lo **avisa** en `warnings` en lugar de inventarlo: fuentes compuestas sin `ToUnicode` (su texto se omite), flujos con filtros no soportados (LZW, CCITT…) o con predictores, y páginas sin texto extraíble (probablemente imágenes escaneadas; no hay OCR). El texto sigue el **orden del flujo de contenido**, que normalmente pero no siempre coincide con el orden de lectura (poppler reordena por geometría: por ejemplo, un emoji dibujado con una fuente de respaldo puede salir al final de su línea). No se reconstruyen columnas ni tablas más allá de separar celdas por espacios.

**PowerPoint.** El título es el de la forma con marcador `title`/`ctrTitle`, o el primer párrafo; `paragraphs` agrupa las ejecuciones de cada párrafo (la referencia devolvía cada `a:t` suelto).

Al escribir, las presentaciones **tienen estilo** y este vive donde corresponde, no pegado en cada forma: un *tema* (colores y tipografías), un *patrón* (fondo, barra lateral de acento, regla bajo el título, estilos de título y de viñetas con el color de acento) y dos *diseños* con marcadores reales, **Title Slide** (portada con fondo propio, banda y regla) y **Title and Content**. Las diapositivas solo rellenan marcadores y heredan todo, así que en PowerPoint se pueden editar, cambiar de diseño o aplicar otro tema sin perder la estructura. Las 16:9 se ven así con `theme.preset`:

| preset | carácter |
| --- | --- |
| `modern` (por defecto) | fondo claro, títulos azul noche, acento azul y ámbar; portada azul noche |
| `light` | blanco, acento celeste |
| `dark` | fondo casi negro, texto claro, acento cian y violeta |
| `corporate` | blanco, azul marino con dorado, títulos serif (Georgia) |
| `warm` | crema, marrón rojizo y naranja, títulos serif |

`theme.colors` (`background`, `title`, `text`, `accent`, `accent2`, `cover_background`, `cover_text`; seis dígitos hex) y `theme.fonts` (`title`, `body`) sustituyen valores del preset; lo demás se conserva. Los colores se normalizan a mayúsculas y todo se valida contra el esquema (preset desconocido, `#2563EB`, `red` o una fuente con caracteres raros se rechazan antes de escribir). `presentation_title` crea la portada y `presentation_subtitle` su subtítulo. Para que el texto quepa sin esperar a que PowerPoint lo reduzca, los títulos largos y los cuerpos densos bajan un escalón de tamaño (títulos de más de 60 caracteres; cuerpos de más de 6, 9 o 12 elementos o de más de 450, 800 o 1200 caracteres); el resto hereda los tamaños del patrón. Los presets cuidan que el texto del cuerpo mantenga contraste suficiente con su fondo, pero un color personalizado puede romperlo: eso lo decide quien lo pasa. **No hay notas del orador, imágenes, gráficos, transiciones ni más diseños** (sección, dos columnas…), y el esquema rechaza `notes` en lugar de ignorarlo.

## Evidencia

`crates/hive-core/tests/office.rs` (15 pruebas) y `tests/office_pdf.rs` (12 pruebas para PDF): ida y vuelta de los tres formatos; lectura de **archivos escritos por python-docx, openpyxl y python-pptx** (`tests/fixtures/office/`); 13 contenedores hostiles (no ZIP, CRC y tamaño erróneos, cifrado, método raro, duplicados, bomba por razón de compresión, entrada gigante, deflate corrupto, ZIP64, demasiadas entradas…); XML estricto (DOCTYPE con entidad externa, entidades propias, referencias inválidas, desbalance, anidación); sandbox, tamaños, reemplazo atómico y enlaces simbólicos; límites de hojas, filas, nombres y celdas; y una validación opcional que abre lo generado con python-docx/openpyxl/python-pptx (`HIVE_OFFICE_PYTHON=<python con esos paquetes>`; sin ella la prueba se omite). Además se abrieron a mano con LibreOffice 25 los tres formatos generados desde Node (conversión a texto, CSV y PDF), pero esa comprobación no está automatizada. El estilo se comprueba leyendo las partes generadas (tema, patrón, diseños, que las diapositivas no llevan colores ni fuentes propios y los escalones de tamaño), que python-pptx ve los diseños y marcadores, y renderizando a PNG con LibreOffice los cinco presets (revisión visual manual, no automatizada). Una prueba por binding. **No se abrieron con Microsoft Office** ni se comparó el comportamiento con mammoth/SheetJS/pptxgenjs.

## Evidencia de PDF

`tests/office_pdf.rs`: lectura de PDFs de **cuatro productores reales** guardados como fixtures (ReportLab, Chrome/Skia 156, LibreOffice 26.8 y Ghostscript 10.07, este último con flujos de objetos y xref comprimido); **igualdad de caracteres con poppler** (`pdftotext`) sobre los cuatro, y poppler leyendo lo que escribe este crate (se omite si no está instalado). **PDF con estilos:** un informe de 4 páginas con tabla de 80 filas comprueba la paginación, la cabecera de tabla repetida, «Página X de N» en cada página, metadatos, marcadores de lista reales (no `?`), enlace `/URI`, el recuento de caracteres sustituidos, `pdfinfo`/`pdffonts` (Helvetica/Bold/Oblique, Times-Bold, Courier), que **ninguna palabra cruza el margen derecho** según `pdftotext -bbox`, y que `pdftoppm` lo renderiza; además se revisó a ojo el render (cabecera, justificado, cita, código, cebra). Presets, apaisado/Letter, palabras kilométricas y bloques inválidos tienen su prueba. Codificaciones (WinAnsi, MacRoman, `Differences`, `uniXXXX`), operadores de texto (`Td`, `TD`, `Tm`, `T*`, `'`, `TJ` con kerning, anchos declarados), fuentes compuestas con `ToUnicode` (bfchar, bfrange y su forma de lista) y sin él, filtros (Flate, abreviados, ASCIIHex, ASCII85 y cadenas), una bomba Flate de 40 MiB, filtro no soportado, árbol de páginas cíclico, recursos heredados, varios flujos por página, metadatos UTF-16, PDF cifrado, truncados y corrompidos sin pánico, anidación de 5000 niveles y rangos/límites. No se probó con PDFs escaneados reales, cifrados reales, ni con los generados por Word/pdfTeX/Cairo.

## Pendiente

OCR, contraseña de propietario y cifrado de clave pública en PDF, texto PDF con layout (columnas/tablas), imágenes y fuentes incrustadas al escribir PDF, imágenes y gráficos en PowerPoint, comentarios y notas al pie en Word, comparación diferencial con la referencia y apertura en Microsoft Office. El control del escritorio (`computer_use`) sigue pendiente aparte (las transformaciones de imagen están en [artifacts.md](artifacts.md)).

## PDF cifrados

`office_read_pdf` abre PDF cifrados con el manejador de seguridad estándar: **RC4 de 40 y 128 bits** (revisiones 2 y 3), **AES-128** (revisión 4) y **AES-256** (revisiones 5 y 6). Sin `password` usa la contraseña **vacía**, que es la de casi todos los PDF «con permisos» (copiar, imprimir…): se abren sin pedirla. Con `password` se prueba la **contraseña de usuario** (hasta 127 caracteres; en las revisiones 2–4 solo Latin-1).

```js
await call('office_read_pdf', { path: 'informe.pdf' });                       // contraseña vacía
await call('office_read_pdf', { path: 'protegido.pdf', password: 'secreto' }); // contraseña de usuario
```

- **Resultado.** Añade `encryption` (`"RC4-40"`, `"RC4-128"`, `"AES-128"`, `"AES-256"`, o `null` si no estaba cifrado) y una advertencia que dice si se abrió con la contraseña vacía o con la dada. La contraseña **no se devuelve nunca**.
- **Errores claros.** Pide `password` si hace falta una (`give its password in \`password\``), `incorrect password` si no coincide (distingue mayúsculas), y rechaza lo que no soporta: manejadores de clave pública, revisiones o filtros de cifrado desconocidos y diccionarios dañados. Un flujo con el texto cortado a mitad es una advertencia de esa página (`corrupt encrypted data`), no un fallo ni texto basura.
- **Qué se descifra.** Cada flujo (contenidos, `ToUnicode`, flujos de objetos) con la clave de su objeto y generación, y las cadenas de los objetos (título, autor…). Los flujos `XRef` nunca están cifrados y los `Metadata` solo si `EncryptMetadata` no es `false`; un flujo con filtro `Crypt` se deja tal cual.
- **No hace.** No verifica ni aplica permisos (solo recupera los bytes), no acepta la **contraseña de propietario** (hace falta la de usuario), no abre cifrado de clave pública y no escribe PDF cifrados. Los PDF cifrados cuyos flujos de referencias cruzadas usan predictores PNG siguen sin leerse (límite general del lector, no del cifrado).

**Validación.** Cinco revisiones × dos contraseñas (vacía y «secreto»). Las de RC4 las generó **Ghostscript**; las de AES y AES-256 las genera `scripts/gen_encrypted_pdfs.rs`, escrito desde la especificación y **no** desde el lector, y cada fichero se comprobó con `pdftotext` de poppler antes de guardarlo (incluido el hash de la revisión 6 y el rechazo de contraseñas erróneas), así que no se prueba solo contra sí mismo. `tests/office_pdf.rs`: apertura con la contraseña vacía en R2–R6 (texto exacto y método), descifrado de cadenas, contraseña ausente/errónea/con otras mayúsculas/correcta, igualdad de texto con poppler en los cinco, y cinco PDF dañados (clave pública, revisión 9, sin clave, filtro desconocido, texto cifrado truncado). Las primitivas (RC4, MD5, AES-128) se comprueban con vectores publicados (RFC, FIPS-197). Comprobado en Linux x64; no con PDF de Acrobat ni de otros productores reales cifrados.

## Excel: fechas y estilos

**Leer.** Un número cuya celda tiene formato de fecha u hora se devuelve como texto ISO. Qué es una fecha lo decide el formato de la celda: los identificadores integrados (14–22, 27–36, 45–47, 50–58) y los códigos personalizados, sin contar el texto entre comillas, los caracteres escapados, los colores, las condiciones ni las etiquetas de idioma (`"día" 0`, `[Red][<0]0.00` y `0.00%` **no** son fechas). Una celda con solo fecha da `AAAA-MM-DD`, con hora `AAAA-MM-DDTHH:MM:SS`, y una solo de hora `HH:MM:SS` (las horas transcurridas, `[h]:mm:ss`, pasan de 24). Se respeta el **bug de Excel de 1900** (el día 60 es un 29 de febrero que no existió y se muestra como Excel lo muestra), el sistema de fechas **1904** (`date1904`) y las celdas de tipo `d`. Un serial fuera del rango de Excel se deja como número. **Cambio de comportamiento:** antes las fechas salían siempre como números; ahora salen como ISO por defecto y `dates: "serial"` devuelve lo de antes.

**Escribir.** Por hoja, `columns: [{column, format, width, bold, color, fill, align, wrap}]` (la columna se nombra por el texto de su cabecera o por su índice), `header_style: {bold, color, fill, align}`, `freeze_header` y `autofilter`. `format` es `date`, `datetime`, `time`, `integer`, `decimal`, `thousands`, `percent`, `currency`, `text` o un código de formato de Excel (≤ 64 caracteres). **Las cadenas ISO de una columna de fecha se guardan como fechas reales** (un número de serie con su formato); una que no sea una fecha válida (`01/03/2025`, `2025-02-30`, un 29 de febrero de 1900, una hora en una columna de solo fecha) es un error con la celda, y no se escribe nada. Los colores son `#RRGGBB`. Sin estas opciones el archivo sale idéntico al de antes. Las celdas siguen siendo valores: el texto que empieza por `=` nunca es una fórmula.

**Verificado:** conversión serial↔ISO de ida y vuelta para cada día de dos siglos, clasificación de 24 códigos de formato, fixtures hechos a mano (1900 y 1904, 16 casos de formato), el XML escrito (columnas, panel fijo, autofiltro, estilos), la ida y vuelta, y **LibreOffice** como implementación independiente: muestra las celdas formateadas como fecha, moneda, porcentaje y hora (opt-in: `HIVE_TEST_SOFFICE=soffice`). **No se comparó con SheetJS ni se abrió en Microsoft Excel**; la prueba opt-in con openpyxl (`HIVE_OFFICE_PYTHON`) no se pudo ejecutar aquí porque no está instalado.

## Word: encabezado y pie · PowerPoint: notas del orador

**Word.** `office_write_docx` acepta `header` y `footer` (`{text, align: left|center|right}`, hasta 1000 caracteres) que se repiten en cada página; el texto puede llevar `{page}`, `{pages}` (campos vivos que Word actualiza: `PAGE` y `NUMPAGES`) y `{title}`. `office_read_docx` devuelve `headers` y `footers` con los campos escritos de vuelta como `{page}` y `{pages}`, aparte del texto del cuerpo. No se leen los campos complejos ni los encabezados de otras secciones o de la primera página. **Verificado con LibreOffice** (opt-in, `HIVE_TEST_SOFFICE` y `pdftotext`): un documento largo sale con el encabezado en cada página y «Página N de M» correcto en todas.

**PowerPoint.** Cada diapositiva puede llevar `notes` (hasta 100 000 caracteres, un párrafo por línea). Si alguna las tiene, el archivo incluye el patrón de notas y una página de notas por diapositiva que las tenga; un archivo sin notas sale igual que antes. `office_read_pptx` devuelve `notes` por diapositiva (separadas del texto de la diapositiva). **Verificado con LibreOffice**: abre el archivo y exporta las páginas de notas con el texto.
