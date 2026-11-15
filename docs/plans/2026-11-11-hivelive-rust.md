# HiveLive Rust

Se conserva la escena Rune/BIA y el protocolo del Hive original (`gateway/realtime`,
`agent/realtime-providers/gemini-live.ts`). El gateway nativo incorpora `/realtime`
autenticado: PCM16 mono 16 kHz de entrada, PCM16 mono 24 kHz de salida, texto, JPEG,
transcripciones, interrupción y herramientas puente. La llamada queda vinculada al
hilo seleccionado y se impide cambiar de conversación mientras esté abierta.

Proveedor, modelo realtime habilitado, clave y tarifas proceden de HiveDB. Se prioriza
el modelo activo y luego el preferido del original si existe en el catálogo. No hay
fallback de credenciales por entorno ni selección de un modelo no registrado.

`consultar_a_bee` acusa recibo y ejecuta un turno real del SDK en segundo plano;
estado y cancelación consultan el mismo runtime. No anuncia capacidades externas aún
no migradas. Narraciones y resultados se agrupan antes de inyectarlos a la voz,
esperando que termine de hablar. Los IDs de herramientas se deduplican y las
notificaciones no se reinterpretan como pedidos nuevos.

Se persisten transcripciones casuales con `source: realtime_chat`, separadas de los
pedidos ya guardados por el coordinador. El consumo se liquida por respuesta completada
con los contadores reportados; solicitudes interrumpidas pueden quedar sin medición.
Las tarifas mostradas salen del catálogo, no de constantes de la UI.

Se espera `setupComplete` antes de abrir el micrófono. Solo se reanuda con un handle
vigente marcado como resumible; nunca se reproducen audio, texto ni herramientas
anteriores. Audio y vídeo se limitan en tamaño y las colas son acotadas. El cierre
libera dispositivos sin mantener el candado global de voz durante callbacks.

Referencia del protocolo verificada en la documentación oficial:
[Live WebSockets](https://ai.google.dev/api/live),
[cliente oficial Go](https://github.com/googleapis/go-genai/blob/main/live.go).
Las pruebas usan sockets en memoria, PCM sintético y hosts de trabajo deterministas;
no activan dispositivos ni llaman a modelos de pago.

## Validación de esta entrega

- Gateway: 31 pruebas aprobadas, incluidas cinco de HiveLive (configuración/aislamiento,
  autenticación, medios y uso, deduplicación del puente y reconexión sin repetir historia).
  Se excluyeron tres pruebas que necesitan escuchar en TCP, no permitido en este entorno.
- `cargo clippy -p hive-gateway-rust --all-targets --offline -- -D warnings`: aprobado.
- Rune: 13 pruebas del estado Live, parser de eventos y prueba de la vista HiveLive aprobadas.
- Binarios debug de `hive-gateway-rust` y `hive-desktop-rune` compilados sin errores.
- Pendiente: llamada real con Gemini, micrófono, altavoz, cámara y captura de pantalla.
  No se modificó el perfil original ni se gastaron tokens en estas verificaciones.

## Corrección del arranque de Gemini Live

La configuración inicial enviaba `speechConfig.languageCode`. La documentación oficial
indica que los modelos de audio nativo no admiten ese campo:
[voz e idioma](https://aistudio.google.com/docs/live-api/capabilities#change-voice-and-language).
Se conserva la preferencia regional en la instrucción de BIA, sin enviarla como parámetro
de síntesis. Además, el manejo anterior ocultaba todos los frames Close y errores del
transporte detrás de un mensaje genérico. Se conserva el código y motivo del proveedor,
con la clave eliminada, para distinguir rechazos de modelo, permisos y configuración.
No se dispone del frame de cierre de la llamada reportada: no se atribuye su motivo
exacto sin una nueva conexión desde el escritorio.

Validación de la corrección: las dos regresiones fallaban antes del cambio y pasan
después. Suite del gateway: 32 aprobadas, tres pruebas TCP excluidas por el entorno.
`cargo fmt --all --check` aprobado. No se realizó una llamada a Gemini real.

## Mensajes JSON en frames binarios

El error «frame inesperado durante el inicio» proviene del rechazo de `Message::Binary`
en el parser de Gemini: texto, ping, pong y cierre ya tenían ramas explícitas.
El cliente oficial acepta JSON tanto desde texto como desde Blob/ArrayBuffer:
[handleWebSocketMessage](https://github.com/googleapis/js-genai/blob/main/src/live.ts).
Se unifica esa decodificación al iniciar y durante el streaming. Los frames binarios
entrantes desde Rune siguen siendo PCM; solo los frames del proveedor son JSON.
La regresión recorre la misma sesión completa en ambos formatos, con confirmación
inicial, audio, transcripciones, medición, interrupción y persistencia.

Validación del parser: la prueba binaria reprodujo exactamente «frame inesperado durante
el inicio» antes de corregirlo. Después pasan las 33 pruebas del gateway ejecutables
sin escuchar en TCP (tres excluidas). Formato aprobado; gateway debug recompilado.
La llamada real con dispositivos sigue pendiente.
