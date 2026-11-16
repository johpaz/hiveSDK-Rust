# Navegador Obscura

El navegador del SDK es **Obscura**, un navegador headless ligero con su propio motor JavaScript, en un único binario pequeño. **No hay Chromium, Chrome, Playwright ni Puppeteer** en este camino: ni descarga, ni perfil temporal, ni controlador Node/Bun. Rust lanza `obscura mcp` y habla con él por stdio (JSON-RPC de MCP), igual que Hive; las herramientas del servidor (`browser_*`) se exponen como `obscura_*`.

## Instalación y localización

**El SDK descarga la última versión de Obscura si no encuentra ninguna** (al primer uso de una herramienta `obscura_*`, o cuando se pide con `installObscura()` / `install_obscura()`).

**Dónde se guarda:** `~/.hive/bin/obscura` (`obscura.exe` en Windows; `%USERPROFILE%\.hive\bin`), el mismo directorio donde Hive instala el suyo y que el resolvedor ya busca, de modo que ambos comparten binario. Se cambia con `obscura.install_dir` (ruta absoluta). Junto al binario se escribe `obscura.install.json` (versión, asset y SHA-256 instalados) para saber si una actualización hace falta. No se usa un directorio del proyecto ni `PATH`.

**Dónde se busca**, en este orden: `obscura.executable`, `OBSCURA_PATH`, `obscura` en `PATH`, `~/.hive/bin`, `~/.hivecode/bin`. Si no hay ninguno y la instalación automática está activa, se descarga. **Una ruta explícita (`executable`/`OBSCURA_PATH`) que no existe es un error y nunca se sustituye por una descarga.**

**Cómo se descarga:**
1. Se lee `releases/latest` de `github.com/h4ckf0r0day/obscura` (API de GitHub); se rechazan borradores, prereleases y etiquetas que no sean `vX.Y.Z`.
2. El asset se elige por plataforma: `obscura-{x86_64|aarch64}-{linux|macos}.tar.gz` o `obscura-x86_64-windows.zip` (la build con renderizado, para que `screenshot` y `pdf` funcionen; no las variantes `no-render` ni `stealth`). Otras plataformas fallan con un mensaje claro.
3. Solo HTTPS y solo hosts de GitHub (`github.com`, `*.github.com`, `*.githubusercontent.com`), también tras redirecciones (máx. 5). La URL de descarga se construye, no se toma del JSON.
4. **El SHA-256 del archivo debe coincidir con el `digest` que GitHub publica para ese asset**; una release sin digest se rechaza (no se instala algo imposible de verificar). Tamaño máximo 300 MB y nunca más de lo declarado.
5. Se extrae con código propio y límites (400 MB; los nombres de entrada solo se comparan, nunca se usan como ruta) y solo el binario `obscura` (`obscura-worker` es el modo por lotes de `scrape` y el modo MCP no lo usa).
6. Se escribe `obscura.part` y se renombra: un proceso que muere nunca deja un binario a medias. Restos de intentos anteriores se limpian.

`installObscura()` instala si falta y conserva lo que haya; `installObscura({update:true})` lo sustituye solo si la última release es otra. Devuelve `{path, version, asset, sha256, already_current}`.

**Desactivarlo** (entornos sin red o que lo gestionan aparte): `obscura.auto_install: false` o `HIVE_OBSCURA_AUTO_INSTALL=0` (la misma variable que usa Hive). Esta descarga es del propio SDK y **no pasa por `allowed_hosts`** de las herramientas web, que solo gobiernan lo que navega el modelo.

## Activación

```javascript
const tools = new NativeTools({ root, obscura: { allowed_hosts: ['example.com'] } });
```

Sin `obscura` en la configuración las herramientas no existen. Política (`ObscuraPolicy`): `executable`, `auto_install`, `install_dir`, `stealth`, `proxy`, `user_agent`, `allow_private_network` (Obscura bloquea localhost y redes privadas por defecto), `allowed_hosts`, `allow_evaluate`, `allow_credentials`, `startup_timeout_ms` (10 s) y `call_timeout_ms` (60 s). Las variables de entorno de Hive para ajustar el navegador (`OBSCURA_STEALTH`, etc.) **no** se leen: la configuración es explícita (solo `OBSCURA_PATH` y `HIVE_OBSCURA_AUTO_INSTALL` se consultan).

## Herramientas (31 por defecto)

`obscura_navigate`, `back`, `forward`, `reload`, `close`; lectura: `snapshot`, `markdown`, `links`, `interactive_elements`, `detect_forms`, `extract` (mapa campo→selector), `get_attribute`, `count`, `search`, `network_requests`, `console_messages`; acción: `click`, `fill`, `type`, `fill_form`, `press_key`, `select_option`, `scroll`, `wait_for`, `wait_for_text`; salida: `screenshot`, `pdf`; pestañas: `tab_new`, `tab_list`, `tab_switch`, `tab_close`. **Opt-in:** `obscura_evaluate` (JavaScript arbitrario, `allow_evaluate`) y las de cookies y estado de almacenamiento (`allow_credentials`, entregan secretos de sesión al modelo).

Todas aceptan `timeout_ms` (solo del lado del SDK, no se reenvía). Solo se reenvían al servidor los argumentos declarados en el esquema, validado antes de llamar. Hay **una sesión viva**: las referencias (`e3`) valen solo para el último `snapshot`.

## Garantías

- **Una llamada a la vez** por sesión (mutex). Un timeout, una cancelación, una caída del proceso o una respuesta ilegible dejan la página en estado incierto: **se descarta la sesión** y la siguiente llamada arranca otra; una acción dudosa nunca se reintenta sobre la anterior. Un error de la herramienta (`isError`) es un error conocido y la sesión sigue.
- **`allowed_hosts`**: se comprueba la URL pedida a `navigate`/`tab_new` (solo http(s), sin credenciales) **y** dónde acaba la página tras `navigate`, `back`, `forward`, `reload`, `click`, `press_key`, `fill_form`, `tab_new` y `tab_switch`. Una dirección ilegible falla cerrada y la página se cierra. No es un sandbox de red: no filtra subpeticiones.
- **Binarios** (`screenshot`, `pdf`): con `path` se guardan dentro de la raíz del sandbox (`create_dirs`); sin él se devuelven en base64 solo si caben en `max_output_bytes`. Si la versión instalada no soporta la herramienta, el error del servidor llega tal cual (la 0.2.4 sí las soporta; su PDF es una captura, ver Validación).
- Proceso hijo con entorno mínimo (`env_clear` + `PATH`, `HOME`, locale, temporales), stderr descartado y `kill_on_drop`. `NativeTools.close()` lo detiene; el wrapper npm además lo hace al salir el proceso.
- Handshake: se pide `2025-06-18` y se acepta la revisión que devuelva el servidor si es `2024-11-05`, `2025-03-26`, `2025-06-18` o `2025-11-25` (Obscura 0.2.x responde con una anterior); cualquier otra se rechaza.

## Skills

`obscura_browser`, `office_document_manager` y `pdf_report_writer` ([skills/](../crates/hive-core/skills)) se distribuyen con el crate (`services::skills::BUNDLED`) y se ven desde npm/Python con `store.services.bundledSkills()` / `bundled_skills()` (solo lista, no habilita nada) e `importBundledSkill(name)` / `import_bundled_skill(name)`. Importar exige que las herramientas que la skill nombra estén en el catálogo `tools` (se registran con las definiciones de `NativeTools`); si no, se rechaza con `reference not found`, igual que cualquier flujo sin ejecutores. Una prueba verifica que cada herramienta y acción que nombran existe en el SDK y que todas llevan la regla de **no usar nunca Chromium, Chrome, Playwright ni Puppeteer**; la de navegador exige parar y pedir instalar Obscura si falta, en lugar de buscar otro navegador.

## Validación

Tres niveles:

- **Servidor MCP falso** (`tests/fixtures/fake_obscura.py`): `tests/obscura_tools.rs` (8 pruebas), `packages/npm/test/obscura-tools.test.mjs` y `packages/python/tests/test_obscura_tools.py` cubren lo que un binario real no deja provocar a voluntad: handshake antiguo, ruido, `ping`, errores, bloqueos, caídas, cancelación y los límites.
- **Binario real** (`real_*` en `tests/obscura_tools.rs`, se omiten sin `OBSCURA_TEST_EXECUTABLE=/ruta/a/obscura`): contra **Obscura 0.2.4** (la instalada por Hive en `~/.hivecode/bin`) **y 0.2.7** (la última, descargada por el instalador del SDK), en Linux x64, con una página local servida por loopback; las tres pruebas pasan con ambas. Verificado: el handshake responde `2024-11-05` y se acepta; el servidor expone **exactamente las 37 herramientas** que el SDK mapea (31 por defecto + 6 opt-in) con los **mismos parámetros y obligatorios** (comparados esquema a esquema; solo `path`/`create_dirs` de `screenshot`/`pdf` son locales del SDK); `navigate`, `snapshot`, `markdown`, `links`, `count`, `search`, `extract` (`campo[]` recoge todas las coincidencias, `selector@attr` lee un atributo, y `selector[]` **no** es sintaxis válida), `evaluate`, `detect_forms`, `fill`, `click` (navega) y `tab_list`; UTF-8 intacto (ñ, acentos); **`screenshot` devuelve un PNG real (1280×720)** y `pdf` un PDF válido guardado en el sandbox. **Ese PDF es solo una imagen**: una única página con la captura JPEG incrustada, sin capa de texto, sin paginar y sin estilos (poppler tampoco encuentra texto); `office_read_pdf` lo lee como una página sin texto y lo advierte (`no extractable text… scanned images`). Para un PDF buscable y con estilo se lee la página con `obscura_markdown` y se escribe con `office_write_pdf`; cookies y `storage_state`; el bloqueo SSRF llega como error de herramienta (`OBSCURA_ERROR`) y la sesión sigue viva; y `allowed_hosts` rechaza antes de llamar al navegador.
- **Instalador** (`tools/obscura_install.rs`, 7 pruebas sin red con un servidor de releases local y fixtures `.tar.gz`/`.zip`): instalación atómica con permisos `0755`, versión registrada, `update` sin cambios si ya es la última y con cambios si hay otra, zip de Windows, y **nada se instala** si el hash no coincide (mismo tamaño, otro contenido), si la release no publica digest, si el archivo no trae el binario, si falta el asset, si la etiqueta no es una versión, si la descarga da 404 o si la plataforma no tiene build; fuera de GitHub/HTTPS se rechaza. Con `HIVE_TEST_NETWORK=1` una prueba **descarga de verdad** la última release (v0.2.7, 72 MB, SHA-256 `11b53d27…`), comprueba `obscura --version`, y navega y exporta con ella.
- **Sin verificar:** `--stealth`, `--proxy`, `--user-agent`, la cancelación y los timeouts sobre un sitio lento real, el instalador en macOS/Windows/aarch64 (solo se ejecutó la descarga real en Linux x64; los otros assets se eligen por nombre y el `.zip` solo se probó con un fixture) y la actualización entre dos releases reales (se probó con un servidor local). Obscura no es un motor de renderizado completo: para texto y formularios es la herramienta; las capturas salen de su propio renderizado y su PDF es una captura.
