# Computer use: pantalla, puntero y teclado

Herramientas `computer_*` para que un modelo vea el escritorio y lo maneje. Controlar el escritorio actúa
sobre **todas** las aplicaciones y cuentas abiertas, así que el diseño gira en torno a la autorización.
Feature Cargo `computer-use` (activada en los paquetes npm y Python). Linux (probado), Windows y macOS (**teóricos**: ver abajo).

## Autorización

- **Apagado por defecto.** Sin `computer` en la configuración de `NativeTools` no se anuncia ninguna herramienta.
- **Dos permisos explícitos.** `allow_screenshot` (por defecto `true`) y `allow_input` (por defecto **`false`**). Una herramienta de un permiso no concedido ni se anuncia ni se puede ejecutar aunque se la llame por nombre.
- **Presupuesto.** `max_actions` (200) acciones hasta que el host llame a `computerReset()` / `computer_reset()`. Se cobra al empezar la acción: una acción que falla a medias pudo tener efectos.
- **Parada de emergencia del host.** `computerStop()` / `computer_stop()` interrumpe la acción en curso, suelta lo que esté pulsado y rechaza todo lo siguiente hasta `computerResume()`. Solo el host puede reanudar; el modelo no tiene herramienta para ello.
- **Nada se repite.** Una acción de entrada interrumpida (tiempo agotado, cancelación o parada) se informa como `COMPUTER_ACTION_UNCERTAIN`: pudo haberse hecho en parte. Se sueltan teclas y botones y no se reintenta.
- **Aprobación por acción.** La pide la política de herramientas del host (por ejemplo `ask` sobre `computer_click`, `computer_type` y `computer_key`); para el motor son herramientas normales llamadas `computer_*`. En el gateway, ver más abajo.

```js
const tools = new NativeTools({ root, computer: { allow_input: true, max_actions: 100, restore_token_path: '/home/yo/.hive/portal-token' } });
```

## Herramientas

| Herramienta | Permiso | Qué hace |
|---|---|---|
| `computer_screenshot` | captura | Captura la pantalla. `region` {x,y,width,height} (píxeles de la captura anterior) devuelve esa zona con más detalle; `path` la guarda dentro de la raíz del sandbox |
| `computer_mouse_move` | entrada | Mueve el puntero a (x, y) |
| `computer_click` | entrada | Clic (izquierdo, medio, derecho; 1 a 3 veces) en (x, y) o donde esté el puntero |
| `computer_drag` | entrada | Pulsa en el origen, mueve y suelta en el destino |
| `computer_scroll` | entrada | Rueda en muescas (`dy` positivo baja, `dx` positivo va a la derecha) |
| `computer_type` | entrada | Escribe texto (hasta `max_text_chars`) en la ventana con foco |
| `computer_key` | entrada | Tecla o combinación: `Return`, `ctrl+c`, `alt+Tab`, `ctrl+shift+t`, `F5`… |
| `computer_wait` | — | Espera hasta 10 s |

**Coordenadas.** Son las de la captura que recibió el modelo: la pantalla reducida para que su lado mayor no
pase de `max_dimension` (1280 por defecto, nunca se amplía). Así una captura y los clics que le siguen siempre
coinciden, también en pantallas HiDPI (la captura se lleva al espacio del puntero antes de reducirla). Un punto
fuera de la captura se rechaza.

**Captura inline.** PNG primero (texto y bordes exactos); si no cabe en `max_output_bytes`, JPEG más pequeño
y, si aún no cabe, con menos tamaño. Si ni así, se pide `path`. El resultado es JSON con `data` en base64,
`mime_type`, `width`, `height` y `screen` (el tamaño del espacio de coordenadas). Llevar esa imagen al modelo
como imagen (y no como texto) es cosa del host, igual que con `obscura_screenshot`.

## Backends

- **X11** (`x11rb`): `GetImage` para capturar y la extensión XTEST para la entrada. Resuelve la distribución del teclado, el Shift de las mayúsculas y los caracteres que ninguna tecla produce (toma prestado un keycode libre). Ve el servidor al que se conecta: en una sesión Wayland eso es XWayland, que solo contiene las aplicaciones X11, por lo que ahí se usa el backend de portales.
- **Portales xdg** (`zbus`, Wayland/GNOME/KDE): `RemoteDesktop` + `ScreenCast` para la entrada y `Screenshot` para capturar. **El escritorio pide permiso a la persona** al empezar la sesión; si lo rechaza, es un error claro y no se reintenta. Con `restore_token_path` el consentimiento se recuerda (archivo con permisos `0600`) y el siguiente arranque no vuelve a preguntar. La captura no es interactiva y el archivo temporal que crea el portal se borra tras leerlo.
- `backend: "auto"` (por defecto) usa portales si hay `WAYLAND_DISPLAY` y X11 si no.

## Que el modelo vea la pantalla: `image_tools`

Un resultado de herramienta es texto, así que una captura en base64 no le sirve a un modelo. La política del
bucle tiene `image_tools: ['computer_screenshot']` (y `max_tool_images`, por defecto 2): el motor saca la imagen
del resultado y se la muestra al modelo como imagen en un mensaje de usuario justo después de **todos** los
resultados del lote (los proveedores exigen ese orden). Detalles:

- Solo las herramientas declaradas pueden hacerlo, y solo con `mime_type: image/*` y `data` en base64 válido; cualquier otro resultado sigue siendo texto, de modo que una herramienta MCP no puede colar imágenes.
- El resultado de texto conserva sus campos y dice `image_attached: true`; el base64 no se repite en él.
- Solo se conservan las `max_tool_images` capturas más recientes; las anteriores se sustituyen por una nota breve, para no reenviar cientos de KB en cada turno.
- Si hay un presupuesto de tokens de entrada sin `attachment_tokens`, la imagen no se adjunta y el resultado dice `image_omitted` (la ejecución sigue).
- Probado en el cuerpo de petición de los tres adaptadores (OpenAI-compatible, Anthropic y Gemini): el resultado de la herramienta va primero y la imagen después.
- Las herramientas `computer_*` se anuncian con `allow_repeated_results`: un clic devuelve siempre `{"ok":true}` y una pantalla quieta se ve igual dos veces.

## En el gateway de Hive

Desactivado por defecto; solo el coordinador (profundidad 0) y solo si su modelo ve imágenes.

- **Ajustes** (administrador): `GET/PUT /api/computer` con `{enabled, allow_input, backend, max_dimension, max_actions, session_minutes}`. `enabled` y `allow_input` son interruptores separados; un ajuste desconocido o fuera de rango se rechaza con 400 y no cambia nada. `GET` incluye `model_has_vision`.
- **Aprobación una vez por conversación:** la primera acción de puntero o teclado pide aprobación (WebSocket y superficie A2UI, igual que las demás); si se acepta, vale para esa conversación y esa persona durante `session_minutes` (10 por defecto). Mirar (`computer_screenshot`, `computer_wait`) no pide nada. Una regla `deny` de la política de herramientas sigue ganando, y una regla `ask` queda satisfecha por la aprobación de la sesión. Un rechazo no concede nada: la siguiente acción vuelve a preguntar.
- **Parada:** `POST /api/computer/stop` interrumpe la acción en curso, anula todas las aprobaciones y bloquea todo hasta `POST /api/computer/resume`; `POST /api/computer/reset` rellena el presupuesto de acciones. Solo con el token de administrador.
- El presupuesto de herramientas y de llamadas al modelo del turno crece en `max_actions`, y el consentimiento del portal se recuerda en `<home>/computer-portal-token`.
- Cada decisión queda en el registro de auditoría (`ask_approved`, `allow`, `deny`…) sin los argumentos.

## Qué está probado y qué no

- Lógica común (permisos, presupuesto, parada, mapeo de coordenadas, teclas, capturas grandes, interrupciones): pruebas unitarias con un escritorio simulado.
- **X11 real:** `crates/hive-core/tests/computer_x11.rs` contra un servidor X desechable (se activa con `HIVE_TEST_X11_DISPLAY`, por ejemplo un `Xwayland :9` anidado; sin la variable no hace nada). Comprueba captura, posición del puntero, clic, rueda, `a`, `B`, `€` y `ctrl+a` recibidos por una ventana real.
- **Portales:** `crates/hive-core/tests/computer_portal.rs` contra un portal falso en un `dbus-daemon` privado (se omite si no hay `dbus-daemon`). Comprueba el orden de las llamadas, el mapeo al flujo, los códigos de botón, la rueda, las teclas, el token de restauración, la captura no interactiva y el rechazo del diálogo.
- **Motor y gateway:** pruebas del motor (imagen tras el resultado, orden en un lote, retención, límites, estado persistido), de los tres adaptadores y del gateway con un escritorio simulado: apagado por defecto, requisito de visión, interruptores, una sola aprobación por conversación, rechazo, regla `deny`, parada/reanudación, caducidad y aislamiento por conversación y persona.
- **Pendiente:** el portal real de GNOME/KDE con una persona aceptando el diálogo; un bucle completo con un modelo real viendo y manejando un escritorio; macOS (Quartz) y Windows (Win32); varios monitores (hoy el espacio del puntero es el flujo o la raíz X que se comparte); atajos globales de parada (en Wayland una aplicación no puede capturar una tecla global, por eso la parada es una llamada del host).

## Windows y macOS (teórico)

Hay backends para ambos sistemas (`win.rs`: `SendInput` y GDI sobre el escritorio virtual; `mac.rs`: eventos de Quartz y `CGDisplay` sobre la pantalla principal), escritos desde la documentación de cada API y **comprobados solo de tipos y con clippy para `x86_64-pc-windows-msvc` y `aarch64-apple-darwin`; nunca se han ejecutado**. No hay ninguna prueba de comportamiento y es probable que aparezcan diferencias (distribuciones de teclado, escalado, permisos). macOS exige los permisos de **Grabación de pantalla** (capturas) y **Accesibilidad** (ratón y teclado) para la aplicación anfitriona, y sin ellos descarta los eventos o devuelve una imagen vacía sin error: el backend lo comprueba antes y lo dice. Windows ignora la entrada hacia ventanas con más privilegios (UIPI) y no puede capturar el escritorio seguro (UAC, bloqueo). Cómo probarlos: `docs/external-testing.md` (ejemplo `computer_demo`).

**Varios monitores:** X11 usa toda la raíz (todos los monitores); Windows, el escritorio virtual; macOS, solo la pantalla principal; en Wayland la persona elige la pantalla en el diálogo del portal y solo se usa esa.

## Pestaña «Control del equipo» (escritorio)
Ajustes → **Control del equipo** edita `GET/PUT /api/computer`: ver la pantalla y, aparte, mover el ratón y escribir; backend (auto/x11/portal), tamaño de captura, acciones por sesión y vigencia de la aprobación. El botón **PARAR YA** (`POST /api/computer/stop`) está siempre visible, y tras parar aparece «Reanudar»; «Reiniciar contador» pone a cero las acciones. Avisa si el modelo del coordinador no ve imágenes. Probado contra un servidor falso (pruebas de interfaz del escritorio); no con una sesión real.
