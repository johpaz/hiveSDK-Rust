# Pruebas externas

Esto es lo que **no se pudo probar dentro del repositorio** y qué necesita cada prueba. Todo lo demás tiene pruebas automáticas (`cargo test --workspace --all-features --locked`, npm y Python). Cada punto dice cómo ejecutarlo, qué debe verse y qué devolver.

**Regla para quien prueba:** informa del resultado *exacto*, incluido el texto de los errores. Un «no funcionó» sin el mensaje no sirve; un «funcionó» sin decir en qué sistema, tampoco. **No envíes claves, tokens ni capturas de pantalla con datos personales.**

## Estado de cada cosa

| Área | Estado | Qué se hizo ya | Qué falta |
|---|---|---|---|
| Computer use, Linux X11 | probado | servidor X real (Xwayland anidado) | otro entorno de escritorio |
| Computer use, Linux Wayland | probado contra un portal **falso** | formato de mensajes, consentimiento, token | el portal real de GNOME/KDE |
| Computer use, **Windows** | **teórico** | compila y pasa clippy para `x86_64-pc-windows-msvc` | todo: nunca se ejecutó |
| Computer use, **macOS** | **teórico** | compila y pasa clippy para `aarch64-apple-darwin` | todo: nunca se ejecutó |
| Bucle de computer use con un modelo real | sin probar | gateway y motor con modelo y escritorio simulados | un modelo con visión manejando una pantalla |
| Slack, Telegram, Discord, WhatsApp | solo servidores falsos | protocolo según su documentación | cuentas reales |
| Oráculo | solo servidor falso | protocolo, presupuesto, cooldown | un oráculo real y medir si mejora las respuestas |
| Ollama nativo | probado con un Ollama real local | texto, streaming, herramienta | Ollama alojado, imágenes |
| Office (Word, Excel, PowerPoint) | probado con LibreOffice | diseño, campos, formatos | Microsoft Office, Google Docs, Keynote/Numbers |
| NVIDIA, Groq | parcial | listados y texto | `thinking`, audio nano-omni, STT/TTS de Groq |
| CI de 6 plataformas, publicación | sin ejecutar | workflows escritos | ejecutarlos |

## 0. Ejecutar lo automático en otra máquina

```bash
python3 scripts/external-tests.py list
python3 scripts/external-tests.py run rust-suite bindings        # lo básico, en cualquier sistema
python3 scripts/external-tests.py run libreoffice                # si hay LibreOffice y pdftotext
python3 scripts/external-tests.py run x11 --x11-display :9       # solo con un servidor X DESECHABLE
python3 scripts/external-tests.py run ollama --ollama-model qwen3.5:4b   # un modelo LOCAL con herramientas
```

Genera `external-report.json` (plataforma, versiones, qué corrió y cómo acabó, y las últimas líneas de cada salida; nunca variables de entorno ni claves). Un área que no pudo ejecutarse figura como `skipped` con su motivo, no como éxito. **Devuelve ese archivo.**

## 1. Computer use en Windows y macOS (teórico)

Requisitos: Rust (el del repositorio, `rust-toolchain.toml`). El código está en `crates/hive-core/src/tools/computer/win.rs` y `mac.rs`.

1. **Compila primero** en el sistema real: `cargo build -p hive-agent-core --features computer-use --examples`. Si falla, el error es la prueba más valiosa: envíalo entero.
2. **Captura (no toca nada):**
   `cargo run -p hive-agent-core --features computer-use --example computer_demo -- screenshot shot.png`
   Imprime el backend y los tamaños; abre `shot.png` en la carpeta temporal que indica. Debe ser tu pantalla real.
   - **macOS:** la primera vez pedirá **Grabación de pantalla** (Ajustes del Sistema → Privacidad y seguridad). Concédelo a la aplicación desde la que ejecutas (la terminal) y **reinicia esa aplicación**. Sin el permiso, el sistema devuelve una imagen vacía sin avisar; el backend lo comprueba antes y lo dice.
   - **Windows:** captura todo el escritorio virtual (todos los monitores). Una pantalla de UAC o de bloqueo no se puede capturar y debe dar un error claro.
3. **Entrada (mueve tu ratón y escribe: ten un documento vacío abierto y enfocado):** añade `--allow-input`.
   - `... -- --allow-input move 400 300` → el puntero debe ir al punto equivalente de la captura (las coordenadas son de la captura reducida a 1280).
   - `... -- --allow-input click 400 300`
   - `... -- --allow-input type "hola"` y `... -- --allow-input key ctrl+a`
   - **macOS:** necesita **Accesibilidad** para la aplicación que ejecuta; sin él los eventos se descartan en silencio (el backend lo comprueba al conectar).
   - **Windows:** una ventana que corre como administrador ignora la entrada de un proceso sin elevar.
4. **Qué comprobar y anotar:** ¿el clic cae donde debe con 1, 2 monitores y pantallas HiDPI/Retina? ¿`type` escribe tildes, `ñ` y emojis? ¿`ctrl+c`/`cmd+c`, flechas, F5? (En macOS, `ctrl+...` es la tecla Control; el atajo normal es `super+...`.) ¿Se queda alguna tecla o botón «pegado» tras un error?
5. **Multi-monitor:** Windows usa el escritorio virtual completo; macOS, solo la pantalla principal; X11, toda la raíz; en Wayland, **la pantalla que la persona elige en el diálogo del portal**.

**Devuelve:** sistema y versión, resultado de cada paso, texto de los errores.

## 2. Computer use en Linux Wayland real (portal de GNOME/KDE)

1. `cargo run -p hive-agent-core --features computer-use --example computer_demo -- --backend portal screenshot shot.png` → la captura sale por el portal `Screenshot` (puede pedir permiso la primera vez).
2. Con entrada: `... -- --backend portal --allow-input move 400 300` → el escritorio muestra el **diálogo de consentimiento** de control remoto; acéptalo. Anota si el puntero llega al punto correcto y si el diálogo vuelve a salir en cada ejecución (con `restore_token_path` no debería: eso se prueba desde el gateway).
3. Rechaza el diálogo una vez: debe salir `the person declined…` y no hacerse nada.

## 3. El bucle completo con un modelo real (gateway)

Con un modelo de **visión** configurado en HiveDB (capacidad `vision`):

1. Activa (administrador): `PUT /api/computer` con `{"enabled":true,"allow_input":true,"max_actions":30}`; comprueba `GET /api/computer` → `model_has_vision: true`.
2. Pide algo inofensivo y acotado en el chat («abre el bloc de notas y escribe hola», con un documento vacío preparado).
3. Debe: capturar la pantalla, pedir **una aprobación** la primera vez que va a usar ratón o teclado (y no más durante 10 minutos), y actuar. Anota si **ve** la captura (si describe lo que hay en pantalla) y cuántas acciones necesita.
4. `POST /api/computer/stop` en mitad de una tarea: debe cortar la acción en curso y bloquear lo siguiente; `resume` la reanuda.
5. Prueba también la denegación (rechaza la aprobación): no debe tocar nada.

**Devuelve:** modelo usado, si vio la pantalla, número de acciones, si la parada fue inmediata, y cualquier acción inesperada.

## 4. Canales con cuentas reales

Cada canal exige una **lista de permitidos** (sin ella no arranca) y responde solo a esos chats. Crea cuentas de prueba, nunca uses las personales.

- **Telegram:** crea un bot con @BotFather; `allowed_chat_ids` con tu chat. Prueba texto, **foto con pie**, foto sin texto, un PDF (debe avisar «Solo puedo leer imágenes») y una aprobación por texto («sí»/«no»).
- **Discord:** bot con el intent de contenido de mensajes; `allowed_channel_ids`. Prueba texto, imagen adjunta, PDF adjunto y mención de otro canal no permitido (no debe responder).
- **Slack, Socket Mode:** `bot_token` con `chat:write`, `files:read` (para imágenes), `app_token` con `connections:write`; prueba hilos, `/hive …`, imagen y los botones Aprobar/Denegar.
- **Slack, webhook:** `mode: "webhook"` + `signing_secret`; la URL pública (túnel HTTPS) es `https://…/webhook/slack/{id del canal}` para eventos, comandos e interactividad. Comprueba el desafío al guardar la URL, un mensaje, una imagen y un botón.
- **WhatsApp Cloud:** `access_token`, `phone_number_id`, `app_secret`, `verify_token`; webhook `…/webhook/whatsapp/{id}`. Prueba texto, **imagen** y la ventana de 24 h.
- **WhatsApp QR (`--features whatsapp-qr`):** vincular con un teléfono de prueba, reconexión y mensajes. Antes de distribuir, **revisar la licencia de `wacore-libsignal`** (ver `docs/whatsapp-qr.md`).

En todos: ¿responde solo a lo permitido?, ¿una imagen llega al modelo (el modelo debe ser de visión)?, ¿los errores aparecen sin mostrar ningún secreto?

## 4b. Probar el oráculo real

```bash
HIVE_ORACLE_KEY=... cargo run -p hive-agent-core --example oracle_probe -- \
  --endpoint https://openrouter.ai/api/v1/systemone --model typesafe/jev-1.13
```
Envía dos casos (una respuesta respaldada y otra que afirma algo que la evidencia no muestra) y dice qué haría el revisor. **Esperado:** el primero se acepta, el segundo se revisa. Devuelve la salida (la clave no se imprime). En el gateway: `PUT /api/oracle` con `endpoint`, `model` y `api_key`; después, mide con varias tareas reales si las correcciones mejoran la respuesta (el SDK no lo ha medido).

## 4c. Probar el OCR real

En el escritorio: Ajustes → **OCR**. Activa, elige una carpeta con un PDF escaneado y una foto de un documento, elige el proveedor (Mistral con su clave en Proveedores, o un modelo de visión) y **Guarda**. En el chat pide: «lee `escaneo.pdf` y dime el total». **Esperado:** pide aprobación una vez; la respuesta cita el método y el proveedor; un PDF con texto se lee sin pedir aprobación (no sale del equipo). Comprueba también que pedir un archivo fuera de la carpeta falla. Envía el texto leído frente al original: la fidelidad real no está medida.

## 5. Proveedores

- **NVIDIA `thinking`:** probar con un modelo que lo admita y comprobar que el razonamiento no se reenvía roto entre turnos.
- **NVIDIA nano-omni (audio):** repetir una petición con audio.
- **Groq:** STT y TTS.
- **Ollama alojado e imágenes:** `Ollama({ baseUrl: 'https://ollama.com/api', apiKey })` y un modelo de visión con una imagen.
- **Ventanas de contexto:** en `models`, rellenar `context_window` de los modelos que ningún listado trae; **confirmar con NVIDIA la de Nemotron 3.5 Bolt (se guardó 65 536 por fuentes que discrepan)**.

## 6. Office en Microsoft Office y otros

```bash
node scripts/office-samples.mjs ./muestras
```
Abre cada archivo y comprueba lo que dice su propio texto: Word (encabezado a la derecha y «Página N de M» correcto en **todas** las páginas), Excel (fechas como fechas, moneda, porcentaje, hora, anchos, primera fila fija, filtros, la celda `=1+1` como **texto**), PowerPoint (notas del orador en la vista de notas), PDF (con estilo). Anota si Office avisa de que el archivo está dañado o «necesita reparación»: es lo más importante.

## 7. Publicación

- **CI:** ejecutar `ci.yml` y `native.yml` (6 plataformas). En Windows y macOS, `native.yml` **compila los backends teóricos de computer use por primera vez de verdad**: si falla ahí, es la prueba de la sección 1.
- **Avisos de terceros:** `python3 scripts/third-party-notices.py --check --require-complete` falla mientras haya paquetes pendientes de **revisión legal humana**: quedan 3 (`rustls-platform-verifier-android` y dos `winapi-*-gnu`; `htmlescape` y `tiktoken-rs` se resolvieron el 2026-11-17, ver `docs/third-party-review.md`).
- **Dependencia Git:** `hivedb-core` está fijado por commit, lo que bloquea `cargo publish`: hay que publicarlo en el registro o empaquetarlo.
- **Credenciales:** npm y PyPI, solo para quien publica.

## Cómo reportar

Un archivo por área con: sistema y versión, versión del repositorio (`git rev-parse HEAD`), pasos ejecutados, resultado esperado frente a obtenido, y el texto íntegro de cualquier error. Para las áreas del script, `external-report.json`.
