# Vistas completas del escritorio y predeterminado de HiveLive

## Modelo predeterminado y compatibilidad

`App::open` aplica una migración atómica una sola vez: registra/habilita
`gemini-3.8-live`, lo marca activo y desmarca los otros modelos realtime de Gemini.
La selección queda en la colección canonical `models`, con un recibo en `config`
(`hive_rust_gemini_38_live_default_v1`). No cambia el proveedor ni su clave ni el
modelo del chat textual. Después de esta migración, las selecciones explícitas del
usuario se conservan entre reinicios. La telemetría en reposo muestra la selección
actual, en lugar del modelo de la última llamada.

El gateway publica `desktopContract: 3` y la capacidad `settings`, además de las
capacidades de cada superficie. Rune no reutiliza gateways anteriores que solo
implementaban chat/dashboard aunque su protocolo WebSocket siguiera siendo 2.
Es necesario reiniciar ambos binarios para aplicar la migración al perfil real.
Las pruebas solo modificaron perfiles temporales.

## Superficies y datos

- Agentes: listado con nombres de campo compatibles con Rune, creación de workers,
  edición, selección de proveedor/modelo LLM, prompt y límites. Los modelos realtime
  y STT no se ofrecen como modelos de los agentes de texto.
- Providers: catálogo, modelos, activación, guardado/borrado y comprobación de claves.
  Los secretos se cifran como en Hive, con la clave maestra existente; las lecturas
  solo devuelven presencia y máscara, sin descifrar todas las claves al abrir la vista.
- Canales: alta, configuración cifrada, activación y estado real desconectado.
  Conectar Telegram/Discord/WhatsApp/Slack todavía requiere sus adaptadores de ejecución;
  la acción responde con el límite en lugar de simular un QR o una conexión.
- Ajustes: herramientas, skills, MCP (lectura sin afirmar conexiones activas), ética,
  perfil, pantalla/audio y actualizaciones. Herramientas configuradas sin ejecutor
  se distinguen de las ejecutables. Actualizaciones no afirma una versión nueva
  si el gateway solo conoce la versión local.
- API Client: HTTP/HTTPS nativo con cabeceras, parámetros, cuerpo, tiempos de espera,
  redirecciones desactivadas y respuesta limitada a 4 MiB.
- Notas: lectura del scratchpad canonical, limitado a conversaciones del usuario.
- Reuniones: historial y socket autenticado, STT compatible con audio/transcriptions
  usando modelo/proveedor/clave de HiveDB, persistencia de segmentos y exportación
  de la transcripción a Markdown dentro del workspace. No simula grabación si falta
  STT ni permite una segunda captura alterar la sesión activa.
- Cron Jobs: listado, historial, pausa/reanudación, cola manual y scheduler nativo.
  Soporta intervalos, one_shot y cron numérico de cinco campos con zonas IANA.
  Respeta ventanas y máximo de ejecuciones, reclama intentos antes de ejecutar y
  omite ocurrencias antiguas. Un intento incierto tras un reinicio no se reproduce.
  Las tareas textuales usan el coordinador y dejan resultados en un hilo de Webchat.
  Tareas con herramientas, agentes específicos o canales externos sin adaptador
  registran un fallo explícito antes de ejecutar operaciones no soportadas.
- Panel interactivo: herramientas nativas a2ui_create_surface/update_components/
  update_data_model/delete_surface; adjacencia y bindings compatibles con Rune.
  Estado por usuario, escritura y recibo de operación atómicos, recuperación al
  suscribirse al lienzo y acciones de formulario convertidas en turnos reales del SDK.

Añadir herramientas cambia la configuración del coordinador. Las sesiones SDK
anteriores conservan sus checkpoints: si aparece SESSION_CONFIG_MISMATCH se crea
una conversación nueva, sin borrar ni reiniciar operaciones inciertas.

## Validación

45 pruebas del gateway aprobadas, con tres pruebas TCP excluidas por el entorno.
Incluyen migración persistida, lectura/escritura de ajustes, confidencialidad y auth,
A2UI ejecutado por el driver durable, reuniones mediante sockets en memoria y cron
con reloj controlado y fallos reales de acciones no soportadas. No hubo llamadas a
modelos de pago ni uso de micrófono/cámara. La navegación nativa de todas las vistas
solicitadas y submenús, HiveLive, su estado y la compatibilidad del gateway pasaron.
Las pruebas de STT real, HTTP remoto y conexiones de dispositivos siguen pendientes.

Cierre: `cargo fmt --all --check`, Clippy del gateway con `-D warnings` y compilación
de ambos binarios debug aprobados. La actualización del perfil real se aplica al
iniciar el gateway nuevo; no se ha escrito directamente en ~/.hive desde las pruebas.
