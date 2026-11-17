# Políticas y hooks de herramientas

Control de qué llamadas a herramientas se ejecutan, en el SDK y en el gateway. Hay tres piezas que se combinan: una **política declarativa** (datos, evaluada en Rust, igual en npm, Python y gateway), **hooks** del host (código propio) y **aprobación humana**.

## Política declarativa

```javascript
const agent = new Agent({ model, provider, tools, toolPolicy: {
  default: 'allow',
  rules: [
    { tools: ['fs_write'], decision: 'allow', when: [{ arg: '/path', op: 'prefix', value: 'reports/' }] },
    { tools: ['fs_*'], decision: 'deny', reason: 'solo se escribe en reports/' },
    { tools: ['command_run'], decision: 'ask', reason: 'ejecuta programas' },
  ],
}});
```

Python: `Agent(..., tool_policy={...})`. Las reglas se prueban en orden y **gana la primera** cuyo patrón de herramienta (`*` comodín) y condiciones (todas) se cumplen; si ninguna, `default`. Sin política (o con `{}`) todo se permite: no cambia nada. Decisiones: `allow`, `deny`, `ask` (necesita una persona).

Condiciones: `arg` es un JSON Pointer (`/path`, `/options/0`) y `op` uno de `equals`, `not_equals`, `prefix`, `suffix`, `contains`, `one_of`, `exists`. **Un argumento ausente (o `null`) nunca cumple la condición**, así que una regla que restringe por ruta no se esquiva omitiéndola: después de ella conviene una regla `deny`/`ask` general. Sin regex a propósito (sin coste imprevisible). Límites: 256 reglas, 64 patrones y 16 condiciones por regla; una política inválida se rechaza **al construir el Agent** o al guardarla, nunca a mitad de un turno. La evaluación es pura: no llama a ningún modelo ni tiene efectos.

## Hooks (SDK)

```javascript
hooks: {
  beforeTool: ({ call, verdict, signal }) => ({ decision: 'ask', reason: 'revisar' }),   // solo puede endurecer
  approve:    async ({ call, reason, signal }) => await pedirAUnaPersona(call),          // true | false | {approved, reason}
  afterTool:  ({ call, result }) => ({ result: redactar(result) }),                      // sustituye lo que ve el modelo
}
```

Python: `hooks={'before_tool': ..., 'approve': ..., 'after_tool': ...}` con un único dict de contexto (`call`, `verdict`, `result`, `cancel_event`); pueden ser síncronos o `async`.

- **El más estricto gana** (`deny` > `ask` > `allow`) entre política y `beforeTool`; un hook **nunca puede aflojar** lo que la política decidió.
- **`ask` sin hook `approve` se deniega** (`APPROVAL_UNAVAILABLE`). Un hook que lanza o responde algo ilegible **deniega** (`POLICY_HOOK_FAILED`).
- Una llamada denegada **no se ejecuta**: el modelo recibe un error `POLICY_DENIED: <razón>` como cualquier fallo de herramienta, y el turno sigue.
- `afterTool` corre cuando la herramienta ya se ejecutó; si falla, **se retiene la salida** (`POLICY_HOOK_FAILED: … output was withheld`) en lugar de pasarla sin filtrar. Solo puede reemplazar el resultado.
- **Antes del claim:** la decisión y la espera de aprobación ocurren **antes** de reclamar la operación durable. Si el proceso muere esperando a una persona no queda nada reclamado ni incierto: al reanudar se vuelve a preguntar y la herramienta corre una sola vez. Los hooks de aprobación deben tolerar que se les pregunte de nuevo.
- Cancelar la ejecución cancela la espera de aprobación (`signal` / `cancel_event`).
- Un hijo delegado (`context.delegate`) es otro Agent con sus propias opciones: la política no se hereda sola.

## Gateway

El administrador guarda **una política para todo el gateway** en HiveDB (`toolPolicy/default`):

- `GET /api/tool-policy` · `PUT /api/tool-policy` con `{"policy": {...}, "approval_timeout_ms": 120000}` (1 s–1 h; se valida antes de guardar) · `DELETE /api/tool-policy`. Solo con el token de administrador.
- Se aplica a cada llamada a herramienta de un turno (hoy `task_delegate*` y los `a2ui_*` del panel) **antes de reclamarla**.
- **La misma pregunta, dentro del chat, como A2UI.** Junto al `approval_request` el gateway dibuja una superficie A2UI efímera `hive-approval-{approvalId}` en el hilo (`a2ui:createSurface` + `updateComponents` + `updateDataModel`, con `threadId`): qué herramienta, el motivo, los argumentos (texto plano, 600 caracteres) y los botones **Aprobar** y **Denegar**. Cada botón manda una acción `hive_approval` con `{approvalId, approved}`. **El gateway la resuelve él mismo** y nunca llega al modelo como prompt; solo cuenta si la superficie y el id coinciden, el botón y el sentido concuerdan, hay una aprobación pendiente de ese usuario y es la primera respuesta (si no, `error`). El prefijo `hive-approval-` está **reservado**: el modelo no puede crear, cambiar ni borrar una superficie así (`a2ui_*` la rechaza), de modo que un botón de ese tipo siempre viene del gateway; y la acción `hive_approval` sobre cualquier otra superficie se rechaza y tampoco llega al modelo. Al contestar (por A2UI, por `approval` o por REST), vencer o cancelar el turno, el gateway manda `a2ui:deleteSurface`. No se guarda: no reaparece al reconectar.
- `ask`: (el escritorio Rune lo muestra como una tarjeta Aprobar/Denegar sobre cualquier pantalla: [`hive-desktop-rune`](../../hive-desktop-rune/README.md)) el gateway envía por el WebSocket de **quien tiene el turno** `{"type":"approval_request","approvalId","threadId","tool","arguments","reason","expiresInMs"}` y espera. Se responde con `{"type":"approval","approvalId","approved":true|false}` por el mismo WebSocket o con `POST /api/approvals/{id}` `{"approved":…}`. Solo puede responder **el usuario del turno** (o el administrador); para cualquier otro el id «no existe». Sin respuesta en el plazo se deniega (`APPROVAL_TIMEOUT`); si nadie está conectado, al instante (`APPROVAL_UNAVAILABLE`); cancelar el turno limpia la espera. Una aprobación se responde una sola vez.
- **Falla cerrada:** si la política guardada no se puede leer, se bloquean las herramientas (`POLICY_UNAVAILABLE`) en vez de abrirse.
- **Canales (Telegram, Discord, Slack, WhatsApp Cloud y WhatsApp QR): la pregunta llega al propio chat como texto.** El gateway escribe «Hive quiere ejecutar «X». …(motivo y argumentos, 400 caracteres)… Responde «sí» para aprobarla o «no» para denegarla. Si no respondes en N s se denegará.» y el **siguiente mensaje de ese mismo chat** la contesta. Solo vale un sí o un no **exactos** (`sí`, `si`, `s`, `yes`, `ok`, `aprobar`… / `no`, `n`, `denegar`, `rechazar`, `cancelar`…, sin importar mayúsculas ni un punto final); una frase que contenga uno («sí, pero antes dime…») **no** cuenta y se responde con un recordatorio. La respuesta la resuelve el gateway, **nunca llega al modelo**, y solo el chat permitido al que se preguntó puede contestarla (otra sala, o un extraño, no). Una pregunta que venció se descarta y el mensaje siguiente vuelve a ser un mensaje normal. Un turno que espera no frena la lectura de los mensajes siguientes: en Telegram cada chat tiene su propio trabajador (sus mensajes siguen en orden) y la respuesta se lee en el sondeo. Se aplica la misma política que en el chat web.
- Las tareas programadas (cron) siguen sin nadie a quien preguntar: un `ask` allí se deniega al vencer el plazo.

### Política por usuario

`PUT /api/tool-policy?userId=u2` (y `GET`/`DELETE`, solo administrador) fija una política **propia de ese usuario que sustituye a la global para él**; sin ella se aplica la global. Se valida igual antes de guardarse y un usuario desconocido es un 404. Un usuario con token propio **no puede** leer ni cambiar políticas (403): aflojar la suya sería saltarse al administrador. Con `userId` ausente se opera sobre la global, como antes.

### Registro de auditoría

Con una política en vigor, **cada decisión sobre una llamada a herramienta queda registrada** (`toolAudit`): `allow`, `deny`, `ask_approved`, `ask_refused`, `ask_timeout`, `ask_unavailable` (nadie conectado), `ask_cancelled` y `policy_unreadable`, con usuario, hilo, herramienta, regla que decidió (`rule`), motivo, qué política aplicó (`user` o `default`) y **solo el hash SHA-256 de los argumentos, nunca los argumentos** (pueden contener secretos). Escribir el registro nunca cambia la decisión.

`GET /api/tool-audit?limit=50&before=<id>&userId=` devuelve las entradas **de la más nueva a la más antigua** con un cursor `next`. El administrador ve las de todos (y puede filtrar con `userId`); **un usuario solo ve las suyas**, pida lo que pida. Los ids son `tiempo-secuencia-azar`, de modo que entradas del mismo milisegundo conservan su orden de llegada. Se conservan las **5000 más recientes** (se poda cada 200 escrituras). Sin política configurada no se audita nada (no hay decisiones que registrar).

## Validación

Rust: 4 pruebas del evaluador (`policy.rs`: patrones, condiciones, argumento ausente, validación) y, en el gateway, 6 (denegada no se ejecuta y el modelo lo sabe, `ask` aprobado/rechazado con quién puede responder, caducidad, cancelación, nadie conectado, política ilegible, API de administración y aprobaciones REST). npm (5) y Python (5): política, aprobación, el más estricto, fallos de hook, redacción, espera cancelable y el caso **durable** (denegación registrada y reanudación tras cancelar una aprobación sin dejar nada reclamado).

**Alcance honesto:** probado con proveedores y aprobadores de guion; el escritorio Rune tiene la interfaz de aprobación (probada con un WebSocket real y su UI). **Validación real (2026-11-16):** sobre una copia aislada del perfil del usuario (`--prepare-test-home`), con el proveedor y el modelo reales del coordinador y una política `ask` para `a2ui_create_surface`: el modelo pidió la herramienta, llegó `approval_request` con la superficie A2UI en el hilo correcto, **Aprobar** la ejecutó (el modelo respondió «listo»), **Denegar** devolvió `POLICY_DENIED: approval refused` y el modelo lo explicó, y la auditoría registró `ask_approved` y `ask_refused`. Esa prueba destapó un fallo que las pruebas con servidores falsos no veían (el motor rechazaba toda respuesta con razonamiento) y ya está corregido y con prueba de motor. No se probó el escritorio y el gateway juntos, ni las aprobaciones por texto en canales con cuentas reales. La política del gateway cubre solo las herramientas que el gateway ejecuta hoy (`task_delegate*` y los `a2ui_*` del panel); el registro no tiene interfaz en el escritorio y se consulta por API; una política no se hereda en los hijos delegados del SDK.
