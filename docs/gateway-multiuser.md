# Gateway multiusuario

Antes el gateway tenía un único token y actuaba siempre como «el primer usuario»: quien tuviera el token podía abrir la sesión de cualquier usuario (`/ws?session=<id>`). Ahora distingue **quién** pide cada cosa.

## Identidades

| Token | Actúa como | Alcance |
|---|---|---|
| El compartido (`HIVE_AUTH_TOKEN` / `.auth_token`) | El primer usuario (propietario) | **Administrador**: todo lo que ya podía, sin cambios |
| Uno por usuario (`hvu_…`) | Solo ese usuario | Solo su superficie propia (abajo) |

Sin credenciales configuradas (ningún usuario con correo y contraseña) el gateway sigue abierto y actúa como propietario, igual que antes.

**Tokens por usuario** (solo el administrador): `POST /api/users/{id}/token` devuelve `{"token":"hvu_…"}` **una sola vez**; en la base solo se guarda su SHA-256 (`users.api_token_hash`). Emitir otro **sustituye** el anterior; `DELETE /api/users/{id}/token` lo revoca. La comprobación compara el hash de todos los usuarios en tiempo constante y sin cortar en el primero. Ninguna respuesta incluye hashes de tokens ni de contraseñas.

## Inicio de sesión con contraseña

`POST /api/auth/login` `{email, password}` devuelve `{authToken, userId}`:

- **El propietario** (el primer usuario, que es el administrador) recibe el token compartido, como siempre: el escritorio lo usa así.
- **Cualquier otro usuario con contraseña** recibe un **token propio de sesión** (`hvu_…`), nunca el del administrador, que solo alcanza lo suyo (ver abajo). Antes todos los inicios de sesión devolvían el token de administrador.
- Un inicio de sesión en otro dispositivo **no cierra** los anteriores: cada usuario puede tener hasta **5** tokens de sesión a la vez y se descarta el más antiguo. Solo se guarda el hash (`users.session_token_hashes`). Una contraseña incorrecta no crea nada y cuenta para el [límite de fallos](rate-limits.md).
- Emitir un token nuevo (`POST /api/users/{id}/token`) o revocarlo (`DELETE`) **termina también todas las sesiones** iniciadas con contraseña.

## Qué puede un token de usuario

Su conversación y chat (`/api/conversations`, `/api/chat/history`, `/ws` **solo de su sesión**), sus tareas programadas (`/api/cron*`, incluido crear y editar), sus notas y superficies del panel, sus agentes (`GET /api/agents`, `/api/office`), su perfil (`GET /api/users`) y responder a sus aprobaciones (`/api/approvals/{id}`).

**Todo lo demás exige el token de administrador y responde 403** (no se «acota» en silencio): proveedores, canales, herramientas, skills, MCP, ética, ajustes, otros usuarios y tokens, peticiones HTTP salientes, voz/realtime, reuniones, panel de control y la política de herramientas. Con un token de usuario, `/ws` con `session=<otro>`, `/realtime` y `/meeting-stream` se rechazan (401). Pedir un recurso ajeno (conversación, tarea, nota, superficie) responde 404, igual que uno que no existe.

## Aislamiento de datos

- **Store y colas por usuario.** Cada usuario tiene su propio `SessionStore` (el tenant es su id; el del propietario sigue siendo el original) y, dentro, su cola `gateway.cron`: los schedules, jobs, eventos y objetivos de uno no existen para otro, ni siquiera con el mismo id.
- **Turnos como el dueño de la conversación**, no como «el primero»: la selección de agente/proveedor, el store, el objetivo y la delegación usan el usuario de la conversación.
- **Tareas programadas** (`cronJobs`): el dueño es su `user_id`; las filas sin dueño (compartidas, heredadas) pertenecen al propietario y solo él las ve, programa y ejecuta. El scheduler corre **por usuario** (workers y cola propios, los usuarios nuevos se incorporan solos).

## Administración de tareas (kernel compartido)

`POST /api/cron` crea una tarea y `PATCH /api/cron/{id}` edita su definición (`name`, `task`, `task_type`, `interval_sec`, `cron_expression`, `timezone`, `dom_and_dow`, `fire_at`, `start_at`, `stop_at`, `max_runs`, `channel`). **Se valida antes de guardar**: un intervalo de cero, un cron imposible, una zona desconocida, un `one_shot` sin fecha, una herramienta externa sin adaptador o un canal sin adaptador son un 400 con el motivo, no una tarea que nunca corre. El dueño, el estado y los contadores no se eligen ni se editan (se cambian con pausar/reanudar/ejecutar). Editar cuándo dispara crea un schedule nuevo y retira el anterior; las ejecuciones hechas siguen en el historial. `GET /api/cron/{id}/schedule` muestra lo que el kernel tiene de esa tarea (schedule, activo, próxima fecha, ejecuciones publicadas, trabajo en curso). Las tareas en estado `error`/`completed` siguen requiriendo `resume` tras corregirlas.

## Validación

`server/tests.rs` (nuevas): tokens (identidad, una sola vez, sustitución, revocación, solo hash guardado, solo administrador, modo abierto intacto), superficie restringida (15 rutas que exigen administrador, perfil y agentes propios, sin filtraciones), aislamiento de conversaciones, historial, tareas, notas y superficies entre dos usuarios, stores y colas separados (el mismo id de objetivo y de tarea en dos usuarios no colisiona), y WebSocket real (sesión propia sí; ajena, realtime, reuniones, token falso o ausente y token revocado, no; el administrador sigue pudiendo). `scheduler.rs`: crear/editar/inspeccionar y 16 definiciones inválidas rechazadas.

**Límites de tasa** por usuario, chat y canal, opt-in: ver [rate-limits.md](rate-limits.md).

**Alcance honesto:** probado con dos usuarios sembrados y un servidor local; no con el escritorio real ni con el volumen de muchos usuarios (cada usuario con tareas arranca su propio scheduler y workers). La política de herramientas puede ser global o propia de cada usuario, y cada decisión se audita ([policy.md](policy.md)). Las reuniones y la voz siguen siendo solo del administrador. No hay inicio de sesión por usuario con contraseña que entregue su token: el administrador lo emite y lo entrega.
