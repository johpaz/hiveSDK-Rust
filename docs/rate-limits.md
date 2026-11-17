# Límites de tasa del gateway

Protegen lo que cuesta dinero (cada mensaje de chat es una llamada de modelo), la ruta pública del webhook de WhatsApp y los tokens frente a la fuerza bruta. **Están apagados hasta que el administrador los configura**, así que por defecto nada cambia.

```
PUT /api/rate-limits
{ "api_per_minute": 120, "chat_per_minute": 20, "channel_per_minute": 6,
  "webhook_per_minute": 120, "auth_failures_per_minute": 30 }
```

`GET /api/rate-limits` los muestra, `DELETE` los quita todos y `PUT` **reemplaza el conjunto entero** (un campo ausente o `null` es «sin límite»). Cada valor es un entero de 1 a 100000 por minuto; campos desconocidos o valores fuera de rango son un 400 y no se guarda nada. Solo con el token de administrador (un usuario no puede subirse su propio límite: 403). Los cambios **valen en la petición siguiente**.

| Límite | Qué cuenta | Clave (cubo independiente) |
|---|---|---|
| `api_per_minute` | Peticiones REST autenticadas | Usuario |
| `chat_per_minute` | Mensajes del chat web (WebSocket) | Usuario |
| `channel_per_minute` | Mensajes de Telegram, Discord, Slack y WhatsApp | Chat (`canal:chat`) |
| `webhook_per_minute` | Peticiones al webhook público de WhatsApp | Canal |
| `auth_failures_per_minute` | Peticiones sin token o con uno falso | Todas juntas |
| `auth_failures_per_minute` | Intentos de login con contraseña incorrecta (`/api/auth/login`) | Cuenta (el correo; también los desconocidos) y, además, suman al cubo común |

**Cómo funciona:** un cubo de fichas por clave que admite una **ráfaga** de hasta el límite y se rellena de forma uniforme (6 por minuto = una cada 10 s). Las claves son independientes: un usuario o un chat ruidoso no gasta la cuota de otro, y el administrador tiene la suya. Es en memoria (se reinicia con el proceso) y acotada: los cubos inactivos se descartan.

**Qué pasa al pasarse:**
- **REST y webhook:** respuesta **429** con `{"error":"Demasiadas peticiones…","retryAfterMs":N}`.
- **Chat web:** el mensaje se rechaza antes de ejecutarse («Estás enviando mensajes muy rápido; espera un momento»), sin llamada de modelo.
- **Canales de mensajería:** el chat recibe esa misma frase y el mensaje **no llega al modelo**; no se crea conversación ni se paga nada. Los demás chats no se ven afectados.
- **Fallos de autenticación:** tras unos cuantos tokens falsos o ausentes, esas peticiones reciben 429 en vez de 401, lo que frena a quien adivina. **Una petición con token válido nunca usa ese cubo**, así que adivinar no deja fuera a los usuarios legítimos. (No hay dirección IP del cliente —el gateway escucha en localhost detrás de un proxy—, por eso el cubo de fallos es global.)
- El webhook se cuenta **antes de leer nada**, también el tráfico sin firma, para que inundar esa ruta pública cueste poco. Un `/health` no se cuenta.

## Valores sugeridos (no son por defecto)

`api_per_minute` 120, `chat_per_minute` 20, `channel_per_minute` 6, `webhook_per_minute` 120, `auth_failures_per_minute` 30. Un `channel_per_minute` bajo es lo que más protege el gasto de un bot al que escribe cualquiera de la lista de permitidos.

## Validación

`ratelimit.rs` (ráfaga y relleno uniforme con un reloj inyectado, claves independientes y cambio de límite al instante, memoria acotada, validación de la configuración) y pruebas del gateway: sin configurar no se limita nada (200 peticiones); la API es solo de administrador y se valida; REST (5 pasan, la 6.ª y 7.ª reciben 429 con `retryAfterMs`, el administrador no se ve afectado y quitar el límite libera al instante); 3 tokens falsos y luego 429 mientras los tokens válidos siguen pasando; en Telegram, un chat con límite 2 recibe dos respuestas y una negativa amable, **el modelo se llamó 3 veces y no 4**, y otro chat sigue respondiendo; chat web por WebSocket real y webhook (el 3.º GET da 429, otro canal no se ve afectado, el tráfico sin firma también cuenta).

**Alcance honesto:** el estado es **por proceso** y en memoria (no compartido entre varios gateways ni persistente), no hay límites por IP, ni sobre el tamaño ni el coste en tokens de cada mensaje (solo su número), y el tope de presupuesto del modelo sigue siendo el de los objetivos. No se probó bajo carga.

**Login por contraseña.** Los fallos de `/api/auth/login` usan el mismo límite `auth_failures_per_minute`, **por cuenta**: tras agotarlo se responde 429 con `retryAfterMs` *sin comprobar la contraseña*, y el cubo se rellena de forma uniforme. Los aciertos no cuentan nunca. El login solo consulta el cubo de su cuenta, no el común, de modo que quien adivine tokens no puede bloquear un inicio de sesión legítimo. Los correos desconocidos reciben el mismo trato (no revelan si la cuenta existe).

**Por IP: no aplica.** El gateway solo escucha en loopback (`127.0.0.1`), así que toda conexión tiene la misma IP y un límite por IP no distinguiría a nadie; si se coloca detrás de un proxy, el límite debe hacerse allí. **Compromiso:** un proceso local puede agotar el cubo de una cuenta y hacer que su dueño espere hasta un minuto.

