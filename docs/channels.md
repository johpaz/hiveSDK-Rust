# Canales nativos del gateway

El gateway conecta un canal por cada fila `channels` activa cuya configuración (secreta, `channel:{id}:config`) está completa; los canales se arrancan, reinician o paran solos cuando cambian la fila o el secreto (o al llamar a `POST /api/channels/{id}/reconnect`, que lo aplica al instante). **Hay adaptadores de Telegram, Discord, Slack y WhatsApp Cloud**; WhatsApp QR sigue pendiente (ver abajo).

## Reglas comunes (`channels.rs`)

Las comparten los tres canales, porque un bot lo alcanza cualquiera que lo encuentre:

- **Lista de permitidos obligatoria.** Solo se responde a los chats/canales listados; un canal sin lista **no arranca** (y nunca abre conexión) y su estado dice por qué. Todo lo demás se descarta sin respuesta y sin llegar al modelo, igual que los mensajes de otros bots y del propio bot.
- **A lo sumo una vez.** Ningún mensaje de usuario se ejecuta dos veces (con sus herramientas): Telegram guarda el desplazamiento antes de tratar los mensajes, Slack confirma cada sobre antes de tratarlo y descarta los ya vistos, y Discord no reanuda sesiones (lo enviado mientras la conexión estaba caída se pierde, no se repite).
- **Los secretos nunca aparecen** en estados, errores ni registros; los errores de red se informan sin URL. **El texto de error no llega al chat**: el chat solo recibe «No pude completar la respuesta. Inténtalo de nuevo.»
- **Los turnos corren como el dueño del canal** (`channels.user_id`, o el propietario) por el mismo controlador durable que el chat web: valen el presupuesto, el aislamiento entre usuarios y la política de herramientas (sin nadie a quien pedir aprobación, un `ask` se deniega al vencer el plazo). La conversación es `{usuario}/{canal}/{chat}`; el historial y la lista del chat web **no** las muestran (solo ven hilos `webchat`).
- Un mensaje mientras hay un turno activo en ese chat recibe «Sigo trabajando en tu mensaje anterior»; respuestas largas se parten (máx. 8 mensajes), preferentemente en saltos de línea y sin cortar caracteres.
- Reintentos con retroceso (2 s hasta 30 s) ante fallos de red; los fallos de autenticación son **fatales**: se muestran en el estado y no se reintentan hasta que cambie la configuración. Estado en vivo: `{status: connecting|connected|error|disconnected, error}`.
- URLs configurables (`api_base`, `gateway_url`) solo seguras (https/wss; http/ws solo hacia localhost) y sin credenciales.

## Aprobaciones por texto

Cuando la política pide aprobación (`ask`) durante un turno de un canal, el chat recibe la pregunta como texto y su siguiente «sí»/«no» exacto la resuelve (ver [policy.md](policy.md)). Probado de extremo a extremo con Telegram falso (pregunta con motivo y argumentos, recordatorio ante una frase, un extraño que no decide, aprobar y denegar, limpieza) y con la lógica común (`ask_agent`) para Discord, Slack, WhatsApp Cloud y WhatsApp QR, cuyos adaptadores solo pasan su función de envío.

## Tareas programadas hacia los canales

Una tarea programada corre sin nadie delante, así que su resultado quedaba en una conversación que nadie lee. Con `channel` en la definición de la tarea, **el resultado se entrega además a los canales activos** para que la ejecución no se pierda:

| `channel` | Destino |
|---|---|
| `all` | **Todos los canales activos** del dueño de la tarea (Telegram, Discord, Slack y WhatsApp) |
| `telegram` · `discord` · `slack` · `whatsapp` · `whatsapp_qr` | Solo los canales activos de ese tipo |
| `auto` · `webchat` · ausente | Solo la conversación de la tarea (como antes) |

- **Qué cuenta como «activo»:** la fila `channels` del **mismo dueño** de la tarea, `enabled` y `active`, con configuración utilizable. Un canal deshabilitado, o de otro usuario, **nunca recibe nada**.
- **A quién se escribe:** a cada destino de su lista de permitidos (chats de Telegram, canales de Discord/Slack, números de WhatsApp), **hasta 50 por canal y entrega**. Los mismos destinos que ya pueden hablar con el agente; Discord y Slack con las menciones neutralizadas.
- **Qué se envía:** `⏰ <nombre de la tarea>` y la respuesta del agente. **Si la ejecución falla o queda incierta, también se avisa** («La tarea programada «X» no se pudo completar. Revisa su historial antes de repetirla.»), sin texto de error interno.
- **Un canal que falla no detiene a los demás, y nada se reintenta** (un reintento podría entregar dos veces). La ejecución se **guarda siempre** (resultado, conversación e historial); `taskRuns` incluye `delivery: {target, sent: [{channel, type, destinations}], failed: [{channel, type, error}]}`, sin secretos. Sin ningún canal activo del tipo pedido la ejecución se completa y el informe lo dice.
- Se elige al **crear o editar** la tarea (`POST/PATCH /api/cron`, que valida el valor).
- **WhatsApp:** fuera de la ventana de 24 h de atención al cliente solo admite plantillas aprobadas, así que un envío iniciado por una tarea puede ser rechazado; se registra como fallo de ese canal. No hay plantillas.

## Telegram

Un bot por *long polling* (`getUpdates` / `sendMessage`). Configuración (se guarda como secreto, nunca en la fila):

```json
{ "bot_token": "123456:ABC…", "allowed_chat_ids": [42, -1001234567890], "poll_timeout_s": 25, "api_base": "https://api.telegram.org" }
```

`bot_token` y `allowed_chat_ids` son obligatorios; `poll_timeout_s` 1–50 (25 por defecto); `api_base` solo para puentes o pruebas (https, o http solo hacia localhost, sin credenciales).

**Reglas de seguridad, porque un bot lo alcanza cualquiera que lo encuentre:**

- **Solo se responde a los chats de `allowed_chat_ids`.** La lista es obligatoria: un canal sin ella **no arranca** y su estado dice por qué. Los demás chats —y los mensajes de otros bots— se descartan sin respuesta y sin llegar al modelo.
- **A lo sumo una vez.** El desplazamiento de `getUpdates` se guarda **antes** de tratar los mensajes (`channelState/{canal}`): un fallo puede perder un mensaje, pero **nunca ejecuta dos veces** el mismo mensaje del usuario (y sus herramientas). Al reiniciar se continúa desde el desplazamiento guardado.
- **El token nunca aparece** en estados, errores ni registros: los errores de red se informan sin su URL y un 401 se muestra como «Telegram rechazó el bot_token».
- **El texto de error no se envía al chat.** Si el modelo o el gateway fallan, el chat recibe «No pude completar la respuesta. Inténtalo de nuevo.»; el detalle queda para el operador.
- Reintentos de red con retroceso (2 s hasta 30 s); un 409 (otro proceso usando el bot) se muestra como estado de error.

**Cómo responde:** cada chat permitido es una conversación `{usuario}/telegram/{chat}` del dueño del canal (`channels.user_id`, o el propietario), con canal `telegram` (el historial y la lista del chat web **no** las muestran: solo ven hilos `webchat`); el turno corre por el mismo controlador durable que el chat web, así que **valen el presupuesto, el aislamiento entre usuarios y la política de herramientas**. Los mensajes de un chat se atienden de uno en uno (si llega otro mientras hay un turno activo, se responde «Sigo trabajando en tu mensaje anterior»). Respuestas largas se parten en mensajes de hasta 4096 caracteres (máx. 8), preferentemente en saltos de línea, sin cortar caracteres. Sin nadie conectado a quien pedir aprobación, un `ask` de la política de herramientas se deniega al vencer el plazo.

**Estado:** `GET /api/channels/…/status` devuelve `{status: connecting|connected|error|disconnected, error}` en vivo.

### Validación

`telegram.rs` (validación de la configuración con 11 casos inválidos, troceado de respuestas) y, con un **servidor falso de la API de Telegram** más el proveedor falso del modelo, tres pruebas del gateway: solo el chat permitido llega al modelo y recibe la respuesta (un extraño y un bot no); el hilo queda bajo el usuario con canal `telegram`; el desplazamiento avanza y se guarda; un segundo mensaje es un segundo turno; deshabilitar el canal detiene el sondeo; un canal sin lista de permitidos nunca sondea; un fallo del modelo da una disculpa genérica sin 429 ni token; se retoma desde el desplazamiento guardado; y un token rechazado se informa sin mostrarlo.

**Alcance honesto:** probado **solo contra un servidor falso**, nunca contra Telegram real ni con una cuenta de bot; el comportamiento real (límites de tasa de Telegram, formato de mensajes, mensajes editados, medios, grupos con menciones, comandos) no está verificado. Solo texto: fotos, audio y documentos se ignoran. No hay webhook (solo long polling). Un solo bot por proceso: si dos procesos sondean el mismo bot, Telegram responde 409.

## Discord

Un bot en la pasarela WebSocket (`wss://gateway.discord.gg`). Configuración:

```json
{ "bot_token": "…", "allowed_channel_ids": ["123456789012345678"] }
```

Los ids van **como texto** (son demasiado grandes para números JSON). Funciona con canales de servidor y mensajes directos. Hay que activar el **intent privilegiado «Message Content»** del bot en el portal de desarrolladores; sin él Discord manda el contenido vacío en canales de servidor y el bot no tiene nada que contestar (el cierre 4014 se muestra como un error con esa indicación).

Conexión: Hello → Identify (intents `GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT`) → latidos con el último número de secuencia; **si el servidor no confirma un latido, la conexión se considera zombi y se reemplaza**; también se reconecta si Discord lo pide (op 7/9) o la conexión se pierde. Los cierres **4004** (token rechazado), **4013/4014** (intents) y 4010–4012 son fatales. Solo mensajes normales de personas (no de bots, webhooks, ni a sí mismo, ni mensajes de sistema). **Las respuestas llevan `allowed_mentions: {parse: []}`: nada de lo que escriba el modelo puede mencionar a `@everyone`, un rol o una persona.** Un 429 al responder se espera una vez (hasta 5 s). Mensajes de hasta 2000 caracteres.

## Slack

Socket Mode: la app abre ella misma el WebSocket (no hace falta exponer ningún webhook). Configuración:

```json
{ "bot_token": "xoxb-…", "app_token": "xapp-…", "allowed_channel_ids": ["D0123ABCD", "C0456EFGH"] }
```

`app_token` (con `connections:write`) abre la conexión (`apps.connections.open`); `bot_token` envía (`chat.postMessage`). Cada sobre se **confirma antes de tratarlo**, también los que se ignoran, y los `event_id` ya vistos se descartan, de modo que una reentrega no ejecuta dos veces el mismo mensaje. Solo mensajes de personas (`message` sin subtipo: se ignoran bots, ediciones, borrados y otros eventos). Slack pide reconectar con regularidad (`disconnect`): se hace sin más. Los errores de autenticación (`invalid_auth`, `token_revoked`, `account_inactive`, `missing_scope`…) son fatales; el resto se reintenta. **Los marcadores que notifican (`<!channel>`, `<!here>`, `<@usuario>`, `<#canal>`) se neutralizan antes de enviar**, así que una respuesta no puede avisar a nadie; los enlaces siguen funcionando.

### Validación (los tres)

Por canal: validación de la configuración (11, 11 y 9 casos inválidos) y pruebas del gateway contra **servidores falsos del protocolo** más el proveedor falso del modelo. Telegram: ver arriba. Discord (WebSocket + REST falsos): identify con token e intents, latidos, solo el canal permitido llega al modelo y se contesta con `allowed_mentions` vacío (extraños, otros bots, él mismo, mensajes de sistema y contenido vacío no), hilo bajo el usuario con canal `discord` y 404 en el historial web, reconexión tras una conexión perdida, reemplazo de una conexión sin confirmación de latido, un 429 reintentado una vez (llega una sola respuesta), cierres 4004 y 4014 fatales y sin reintento, y sin lista de permitidos nunca se conecta. Slack (Socket Mode + Web API falsos): token de app para abrir y de bot para enviar, **los 7 sobres confirmados** (incluidos los ignorados), reentrega descartada, solo el DM permitido se contesta, reconexión a petición, `invalid_auth` y `token_revoked` sin reintento ni token en el estado, un error transitorio reintentado y recuperado, y sin lista nunca se conecta.

**Entrega de tareas programadas** (arriba): probada contra los cuatro servidores falsos a la vez, con un canal deshabilitado y otro de otro usuario que nunca reciben nada, el aviso de fallo sin internals, un canal inalcanzable sin impedir a los demás, y sin canales activos.

**Alcance honesto:** **solo contra servidores falsos**, nunca contra Discord, Slack ni Telegram reales ni con cuentas de bot; el comportamiento real (límites de tasa, formatos, hilos de Slack, comandos, medios, reanudación de sesión de Discord, reintentos de entrega) no está verificado. Solo texto. Slack responde en el canal (no en hilos) y Discord no escribe «está escribiendo…».


### Hilos, comandos con barra y botones de aprobación

- **Hilos.** Un mensaje escrito dentro de un hilo se contesta **en ese hilo** (`thread_ts`); uno fuera de hilo, en el canal. El identificador del hilo solo se reenvía si es una marca de tiempo de Slack (dígitos y punto, máx. 32); cualquier otra cosa se ignora. La conversación sigue siendo la del canal.
- **Comando con barra** (`/hive <pregunta>`, el que configures en la app de Slack con Socket Mode). El sobre `slash_commands` se **confirma antes de procesarlo**, se descartan los repetidos por `envelope_id`, y la respuesta va por su `response_url` (`response_type: in_channel`, en trozos). Solo canales de la lista de permitidos (un extraño no recibe respuesta alguna, como con los mensajes); un comando vacío recibe, solo para quien lo escribió, una pista de uso. **Solo se publica en `https://hooks.slack.com/…`** (o en esta máquina, para pruebas): un `response_url` de otro anfitrión se ignora sin tocar la red ni el modelo.
- **Botones de aprobación.** Si una política pide aprobar una herramienta (ver [policy.md](policy.md)), la pregunta llega con **Aprobar** y **Denegar** (Block Kit; el texto va como `plain_text`, nunca como marcado, y los botones solo llevan el id de la aprobación). El sobre `interactive` se confirma y se resuelve en el gateway: solo vale el botón de **la pregunta que ese canal tiene pendiente**, desde un canal permitido; uno antiguo, ajeno o repetido no decide nada. Al decidir se **reemplaza el mensaje** (`replace_original`) por «Aprobado. Continúo.» / «Denegado.» para que no se pulse dos veces. Escribir «sí»/«no» sigue funcionando. Igual que con el texto, **cualquiera que esté en un canal permitido puede pulsarlos**: el canal es el límite de confianza.

**Validación:** Slack falso con Socket Mode, `response_url` y botones: hilos (con y sin `thread_ts`, e identificador inválido), comando (respuesta, repetido, vacío, extraño, `response_url` ajeno, sin url) con todos los sobres confirmados, y botones (aprobar y denegar, botón antiguo/de otro canal/acción desconocida, reemplazo del mensaje y segunda pulsación). **No probado:** con una app de Slack real, ni los límites de tamaño de Block Kit más allá del recorte a 2900 caracteres.

## WhatsApp Cloud

La API oficial de WhatsApp Business, por **webhook**: Meta llama a `GET/POST /webhook/whatsapp/{id del canal}`. Fila `channels` de tipo `whatsapp` y configuración secreta:

```json
{ "access_token": "EAAB…", "phone_number_id": "1055…", "app_secret": "…", "verify_token": "…", "allowed_numbers": ["+57 300 111 2222"] }
```

Los cuatro primeros y `allowed_numbers` son obligatorios (los números se normalizan a dígitos con prefijo de país). Esa URL en Meta, con el `verify_token`, completa la suscripción; las respuestas salen por `POST {api_base}/{phone_number_id}/messages`.

**El gateway escucha solo en localhost**, así que el operador debe exponer **esa única ruta** por HTTPS (proxy inverso o túnel); no hay que abrir nada más. Es la **única ruta atendida antes del token bearer** y por eso lleva su propia protección:

- **Toda petición POST debe ir firmada.** `X-Hub-Signature-256` se verifica como HMAC-SHA256 del cuerpo exacto con el `app_secret`, en tiempo constante; sin firma o con una incorrecta (secreto distinto, cuerpo alterado, formato roto) es un **401 y el cuerpo ni se interpreta**. Por eso `app_secret` es obligatorio: sin él se aceptarían webhooks sin firmar.
- La verificación (`hub.verify_token`) también se compara en tiempo constante y devuelve el `hub.challenge` **tal cual, como texto plano**.
- Un canal **desconocido, deshabilitado, de otro tipo o mal configurado es un 404 liso**: Meta (o un atacante) no averigua nada.
- Solo se contesta **texto de los números permitidos** y de este `phone_number_id`; imágenes, audio, recibos de entrega y texto vacío se ignoran.
- **A lo sumo una vez.** WhatsApp reenvía un webhook que cree fallido, así que los ids de mensaje se **guardan** (los últimos 200, `channelState/whatsapp:{id}`) **antes** de ejecutar nada y un id ya visto se descarta; si no se pudo guardar, no se ejecuta. Se responde 200 enseguida y el turno corre después.
- Estado: `listening` si la configuración es utilizable (es un webhook, no hay conexión que vigilar) o el error de configuración, nunca un secreto.

**Validación** (HTTP real contra el gateway, Graph API falsa y modelo falso): verificación de Meta con el challenge exacto en texto plano y 403 con token, modo o challenge incorrectos; el resto del gateway sigue exigiendo el bearer con el webhook público; mensaje firmado de un número permitido contestado a ese número con el token de acceso; redelivery descartada y ids persistidos; firma falsa, ausente, rota y de otro cuerpo (401); extraño, otro teléfono, imagen, texto en blanco y recibos sin llegar al modelo; canal desconocido, de Telegram, deshabilitado, y sin números o sin secreto (404); estado sin secretos. **Alcance honesto:** solo contra una Graph API falsa; no se probó con Meta real, ni la ventana de 24 h de WhatsApp, plantillas, medios, grupos, ni el límite de tasa. La deduplicación persiste, pero no se probó reiniciando un proceso real. Solo texto.

## Pendientes (sin adaptador)

**WhatsApp QR** (vinculación con un dispositivo y sesiones persistentes) requiere un spike previo de la biblioteca de vinculación (emparejamiento y reconexión reales) antes de añadir la dependencia, y una cuenta real; WhatsApp Cloud no lo sustituye. Sigue las mismas reglas y no se enviará ningún mensaje real sin que lo autorices.

### Slack: imágenes y modo webhook

**Imágenes.** Un mensaje con archivos (`subtype: file_share`) de un canal permitido se contesta, y sus **imágenes** (png, jpeg, gif, webp, hasta 4 por mensaje y 5 MB cada una) llegan al modelo como adjuntos. Necesita el permiso `files:read` del bot. Cada archivo se descarga con el token del bot, pero **solo desde los servidores de Slack (https) o el host de `api_base`**, sin seguir redirecciones, y se comprueba que los **bytes** son realmente esa imagen (una página de inicio de sesión HTML o un fichero disfrazado se descartan). Un PDF u otro tipo no se descarga. Un mensaje solo con una imagen se contesta como «Mira la imagen adjunta.»; uno con archivos y sin nada utilizable ni texto recibe un aviso y no llama al modelo. **Si el modelo no ve imágenes** (no tiene la capacidad `vision` en HiveDB) no se le envían, y se le avisa de que la persona adjuntó N. El mecanismo (`ask_agent_with`, y la descarga y verificación en `channels::fetch_image`) es común a todos los canales (ver abajo).

**Modo webhook (Events API por HTTP).** Para quien no quiere Socket Mode:

```json
{ "mode": "webhook", "bot_token": "xoxb-…", "signing_secret": "…", "allowed_channel_ids": ["D0123ABCD"] }
```

Slack llama a `POST /webhook/slack/{id del canal}` (eventos en JSON; comandos con barra e interactividad con formulario), en una dirección HTTPS pública que pones delante del gateway. Es la **segunda ruta pública**, así que se protege como la de WhatsApp: **toda petición debe ir firmada** (`X-Slack-Signature`, `v0=` + HMAC-SHA256 de `v0:{timestamp}:{cuerpo}` con el `signing_secret`, en tiempo constante) y con `X-Slack-Request-Timestamp` a **menos de 5 minutos** de ahora (un reenvío antiguo es un 401); sin firma, con firma rota, de otro cuerpo, de otro secreto o de otra versión es un **401 y el cuerpo ni se interpreta**. Un canal desconocido, deshabilitado o mal configurado (sin `signing_secret`, por ejemplo) es un 404 liso. El desafío `url_verification` se contesta con el `challenge`. Los eventos se **guardan por su `event_id` antes de ejecutar nada** (los últimos 200, `channelState/slack:{id}`), así que Slack reenviando un evento, o un reinicio, no lo ejecuta dos veces; un evento sin id no se ejecuta. Se responde 200 enseguida y el turno corre después; las respuestas siguen saliendo con `chat.postMessage` y el token del bot. El estado es `listening` (no hay conexión que vigilar) y nunca incluye secretos. El resto del gateway sigue exigiendo el bearer.

**Validación:** servidores falsos de Slack (API, archivos, Socket Mode) y del modelo; el webhook por **HTTP real** contra el gateway. Cubre una imagen real entre archivos de otro host, HTML, bytes falsos, PDF y tamaño declarado enorme (solo se pide la buena y el token solo viaja al host de Slack), el borde sin visión, el desafío, seis variantes de firma inválida, la repetición, la persistencia del id, extraños, bots y ediciones, un comando con barra y 404/405. Los botones de Aprobar/Denegar también se probaron **por webhook**: pulsaciones firmadas como formulario (`payload=`), sin firma o con firma de otro secreto (401), y las de otra pregunta, otro canal u otra acción no deciden nada. **No se probó contra Slack real**, ni la descarga de archivos privados con los permisos reales. Enviar archivos desde el agente a Slack (subidas) no está implementado.

### Imágenes en Telegram, Discord y WhatsApp

Los cuatro canales aceptan **imágenes de entrada** con las mismas reglas (`channels::fetch_image` y `trusted_file_url`): como mucho 4 por mensaje y 5 MB cada una; solo png, jpeg, gif y webp; **los bytes deciden el tipo** (una página HTML o un archivo disfrazado se descartan, aunque el remitente diga otra cosa); **no se siguen redirecciones**; el token solo viaja a donde debe. Llegan al modelo solo si tiene la capacidad `vision` en HiveDB (si no, se le avisa de que había N imágenes); un mensaje con archivos sin nada utilizable ni texto recibe «Solo puedo leer imágenes…» y no llama al modelo; una imagen sola se contesta como «Mira la imagen adjunta.».

- **Telegram:** foto (se usa el tamaño más grande que cabe) o imagen enviada como documento, con su pie de foto como texto. Se pide `getFile` y se descarga de `…/file/bot<token>/<ruta>`; la ruta debe ser relativa y de caracteres simples (nada que salga del servidor de archivos).
- **Discord:** adjuntos del mensaje; solo se descargan de los hosts de Discord (`discordapp.com`, `discordapp.net`, `discord.com`) por https o del host de `api_base`, y **sin credencial** (sus archivos son públicos). Una URL de otro sitio nunca se pide.
- **WhatsApp Cloud:** mensajes de tipo `image` (con su `caption`); el id de medio (de caracteres seguros) da una URL temporal por la Graph API y ambas peticiones llevan el token de acceso; la URL debe ser de Meta (`fbsbx.com`, `facebook.com`, `whatsapp.net`, `fbcdn.net`) o del host de `api_base`. **Cambio de comportamiento:** antes una imagen se ignoraba; ahora se procesa o se avisa de que no se pudo usar.

**Validación:** servidores falsos con archivos propios y el modelo de visión falso: la imagen real entre otras de otro host, HTML, bytes falsos, PDF y tamaños enormes; el borde sin visión; sin texto; chats o canales no permitidos que no llegan al modelo; y el token solo donde corresponde. **Ninguno se probó con cuentas reales**; los formatos de Telegram, Discord y Meta son los de su documentación, no observados.
