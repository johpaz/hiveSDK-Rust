# WhatsApp por QR (no oficial)

Canal de WhatsApp vinculado a un teléfono como «dispositivo vinculado», igual que hace Baileys en el Hive TS. Tipo de canal `whatsapp_qr`; la API oficial de Meta sigue siendo el tipo `whatsapp` ([channels.md](channels.md)).

> **Riesgo.** Es un cliente **no oficial** de WhatsApp Web (crate [`whatsapp-rust`](https://github.com/jlucaso1/whatsapp-rust) 0.7, MIT). Usarlo puede incumplir los términos de WhatsApp y llevar a **suspender el número**. Úsalo solo con un número que puedas perder; para producción, la Cloud API.

## Activarlo

Está **desactivado por defecto**: se compila solo con la feature `whatsapp-qr` del gateway (`cargo build -p hive-gateway-rust --features whatsapp-qr`). Sin ella un canal `whatsapp_qr` queda en estado `error` con el motivo, y no envía ni recibe nada. Añade ~80 crates (cliente, protocolo Signal, SQLite embebido).

Crear el canal (`POST /api/channels`, tipo `whatsapp_qr`) con configuración secreta:

```json
{ "allowed_numbers": ["573001112233", "14155550123"] }
```

`allowed_numbers` es **obligatorio** (números internacionales, 6 a 15 dígitos, sin espacios; se admite un `+` inicial; máximo 1000). Sin él el canal no arranca.

## Vincular

1. Activa el canal. Su estado (`GET /api/channels/{id}/status`) pasa a `qr` con `{"status":"qr","qr":"<texto del QR>","expires_in_s":N}`.
2. El cliente del propietario dibuja ese texto como QR. En el teléfono: *Ajustes → Dispositivos vinculados → Vincular un dispositivo*. El QR dura unos veinte segundos y se renueva solo.
3. Al vincularse el estado pasa a `connected` y el QR desaparece. Las claves del dispositivo quedan en `<home>/whatsapp-qr/<id del canal>.db` (directorio `0700`) y **no hay que volver a escanear** tras reiniciar.

Si desvinculas el dispositivo desde el teléfono, el canal borra esas claves, avisa en el estado y genera un QR nuevo. Desactivar el canal desconecta de forma ordenada.

## Reglas (las comunes de los canales)

- **Solo se responde a los números de `allowed_numbers`**, y se comparan por **número de teléfono**: un remitente que WhatsApp identifica solo con un LID (sin número visible) **se descarta**, nunca se adivina.
- **Nunca** se responde a grupos, a mensajes propios, a mensajes que no sean texto ni a los de **más de dos minutos** de antigüedad (la cola que llega al reconectar). Cada id de mensaje se atiende **a lo sumo una vez** (los últimos 4096 en memoria; los desconocidos no la llenan).
- Cada turno se ejecuta como el propietario del canal por el driver durable (presupuesto, aislamiento entre usuarios, política de herramientas y límite `channel_per_minute` de [rate-limits.md](rate-limits.md)). Lo que no puede aprobar nadie se deniega al vencer la aprobación.
- Los errores al chat son genéricos; el QR y las claves nunca van a logs ni a errores (el QR solo está en el estado del canal, al que accede su propietario).
- **Entrega de tareas.** Cuenta en `delivery` de las tareas programadas: `channel:"whatsapp_qr"` o `"all"` envía el resultado a cada número permitido por el cliente conectado; si el canal no está conectado, se informa como fallo sin reintentar.

## Qué está probado y qué no

**Probado (Rust, sin red):** validación de la configuración (lista obligatoria, formatos, límite), nombre de archivo del enlace sin escapes de directorio, el filtro de mensajes (permitidos, grupos, propios, sin número, no texto, antiguos, duplicados, memoria acotada, extraños que no la llenan), permisos `0700` del directorio de claves, envío sin conexión, y que el gateway compila con la feature y sin ella.

**NO probado: el protocolo real.** Vincular un QR, reconectar, recibir y enviar mensajes y el comportamiento con LID exigen un teléfono y un número reales y no se han ejecutado. La parte que habla con WhatsApp es la del crate; el adaptador solo la usa. **Antes de fiarse hay que probarlo con tu número:** vincular, escribir desde un número permitido y uno que no lo es, reiniciar el gateway sin reescanear, desvincular desde el teléfono.

## Avisos de terceros

El crate trae `wacore-libsignal` (implementación del protocolo Signal declarada MIT por su autor). Antes de **distribuir** un binario con esta feature, revisa su origen y regenera `third_party/inventory.json` (`scripts/third-party-notices.py`, necesita red). Los binarios publicados no deben activarla sin esa revisión.
