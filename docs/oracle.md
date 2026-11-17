# Oráculo de decisiones opcional (Jev / Kev)

Un oráculo responde preguntas cerradas sobre un estado que describe el host: elegir una opción entre varias (`choice`) o dar la probabilidad de un enunciado (`noul`). Es **solo consultivo** y está apagado salvo que el host cree un `Oracle`; el SDK no lo consulta por su cuenta. Jev (OpenRouter), Kev (laboratorio HiveAgents) o un modelo propio comparten el mismo protocolo.

```js
const oracle = new Oracle({ endpoint, model: 'typesafe/jev-1.13', api_key });
const reply = await oracle.ask(
  { objective: 'resumir el informe', toolResults: [...] },
  { next: { type: 'choice', instructions: '¿Qué sigue?', criteria: { finish: 'ya responde', continue: 'falta evidencia' } },
    sure: { type: 'noul', instructions: '¿La respuesta está respaldada?' } },
  { objective, depth: 0 });   // objective opcional, pero recomendado
if (reply.outcome === 'decision') use(reply.answers); else classicPath(reply.reason);
```

```python
reply = await Oracle({'endpoint': url, 'model': 'kev'}).ask(state, questions, objective=objective)
```

## Protocolo

`POST endpoint` con `{model, state, questions}` y `Authorization: Bearer` si hay clave. Respuesta: `{answers, usage?}`. Se aceptan las dos grafías de respuesta —`{choice, confidence}` / `{noul}` (OpenRouter) y `{choice, probabilities}` / `{probability}` (llama.cpp System One, donde la confianza de la opción es su propia probabilidad)— y luego se valida con rigor: la opción debe ser una clave de `criteria`, y confianza y `noul` deben ser números finitos en [0, 1]; una respuesta ausente o malformada invalida toda la respuesta. Una pregunta `choice` necesita al menos dos criterios, hay un máximo de 32 preguntas y 1 MiB de respuesta, y el endpoint es http(s) sin credenciales ni redirecciones.

## Falla en abierto, sin lanzar

Un servidor caído, un HTTP no 2xx, un timeout (3 s por defecto), un JSON inválido o una respuesta que rompe el contrato devuelven `{outcome:"fallback", reason, sent}`; el host sigue por la ruta clásica. Tres fallos seguidos —o un 401/403 de inmediato— abren un *cooldown* (`cooldown_ms`, 60 s) durante el cual no se envía ni se reserva nada. Cancelar una llamada lanza `CANCELLED` y no cuenta como fallo. `status()` informa `ready`/`fallback`, `cooling_down`, último error y decisiones servidas; los mensajes de error nunca incluyen la URL ni la clave.

Desconfianza: cuando el runtime descubre que el oráculo se equivocó (dijo "terminar" y la evidencia no lo respaldaba), `recordOverruled(razón)` lo registra; `distrust_strikes` (3) seguidas lo apartan `distrust_ms` (5 min) y devuelve `true`. `recordAgreement()` reinicia la racha y `reset()` limpia fallos, cooldown y rachas.

## Presupuesto

Cada llamada es una **llamada auxiliar de modelo** y se carga al Objective raíz (local o durable):

1. Antes de enviar se reserva con `max_output_tokens` (64 por defecto); si el Objective no admite más llamadas o salida, falla con `OBJECTIVE_LIMIT` —un presupuesto agotado no es un fallo del oráculo y no se envía nada—.
2. Si responde, se liquida el uso notificado (`usage.input_tokens/output_tokens`).
3. Si el servidor **no** informa uso: con `assumed_input_tokens` se liquida esa asignación más `max_output_tokens`; sin ella se liquida como desconocido y el Objective se detiene con `usage_unavailable` (nada medido crea capacidad gastable).
4. Una petición enviada que no produjo decisión (fallo, timeout, cancelación) **conserva su reserva**, igual que cualquier llamada de modelo sin confirmar. Durante un cooldown no se reserva.

`cost_usd` es el coste que informa el servidor, `0` si `self_hosted`, y `null` si se desconoce; no se estima ninguna tarifa. En Rust, `Oracle::ask_charged(&objective, depth, …)` hace lo mismo; los wrappers JS/Python lo hacen con `objective`.

## Revisión de la respuesta final (`oracle::review`)

Antes de que el modelo entregue su respuesta, el host puede pedirle al oráculo dos preguntas cerradas sobre ella: `next` («¿debe parar aquí?», opciones `finish` / `continue`) y `supported` («¿lo que afirma está respaldado por lo que devolvieron las herramientas?», una probabilidad). `review::questions(&ReviewInput {request, answer, evidence})` arma el estado y las preguntas (todo recortado: 2 000 caracteres de la petición, 4 000 de la respuesta, las 12 últimas evidencias de 300) y `review::judge(&reply, &config)` decide: **`Revise` solo si el oráculo dice `continue` con confianza ≥ `min_confidence` (0.7) o si `supported` < `support_threshold` (0.5)**; todo lo demás —incluido un oráculo caído, en cooldown o que respondió mal— es `Accept`. `ReviewConfig.max_corrections` (1–3, por defecto 1) limita cuántas veces el host puede devolver la respuesta al modelo, y `correction_prompt(razón)` es el mensaje de corrección. El host llama a `record_overruled` si el modelo, tras la corrección, da la misma respuesta (el consejo no hacía falta) y a `record_agreement` si la cambia.

### En el gateway

Apagado por defecto; **solo el coordinador** (profundidad 0). Ajustes de administrador en `GET/PUT /api/oracle`: `{enabled, endpoint, model, self_hosted, timeout_ms, max_corrections, min_confidence, support_threshold}` más una `api_key` opcional que se guarda como **secreto** (`oracle:api_key`) y **nunca se devuelve** (solo `has_api_key`). Un ajuste desconocido, fuera de rango o una dirección inválida es un 400 y no cambia nada; activarlo exige `endpoint` y `model`.

- Cuando el coordinador va a dar su respuesta, el gateway le pregunta al oráculo con la petición, la respuesta y lo que devolvieron las herramientas MCP, de escritorio y los especialistas (sin imágenes). Si dice que revise, el modelo recibe el motivo y corrige **antes de que la persona vea nada**: solo se entrega y se guarda la respuesta final. Tras las `max_corrections` correcciones se acepta lo que haya, sin volver a preguntar.
- **Presupuesto:** cada consulta es una llamada auxiliar del turno, **reservada en su Objective antes de enviarla y liquidada con el uso informado** (sin uso se carga una asignación de 2 000 tokens de entrada en vez de detener el turno). El presupuesto del turno crece en una vuelta por corrección más la llamada del oráculo. Un turno sin presupuesto se salta la revisión, no la respuesta.
- **Falla en abierto:** caído, en cooldown, con respuesta inválida, cancelado o sin presupuesto → se entrega la respuesta tal cual. Tres fallos seguidos abren el cooldown y mientras dura no se envía ni se reserva nada. Si el modelo repite la misma respuesta tras tres correcciones, el oráculo se aparta (`distrust`).
- Probado con un oráculo falso: apagado por defecto y validación, clave nunca devuelta ni guardada con los ajustes, solo administradores, una corrección (la persona ve solo la corregida; el modelo ve el motivo y su borrador; 3 llamadas cargadas: 2 del modelo y 1 del oráculo), un sí o un oráculo caído no retrasan nada, cooldown tras tres fallos, desconfianza tras tres consejos inútiles, la evidencia de una herramienta MCP y el cálculo del presupuesto. **No se probó contra OpenRouter ni contra un modelo Jev/Kev real**, ni se midió si las correcciones mejoran las respuestas.

## No implementado (sigue pendiente)

- Del **planificador** de la referencia (`jev-planner.ts`) solo existe la **revisión de la respuesta final** (abajo). **No se implementaron a propósito** el paralelismo de herramientas, la poda de historial y de resultados dirigida por el oráculo y la verificación de cada resultado de herramienta: el gateway ejecuta una herramienta cada vez por diseño (reclamar, ejecutar y confirmar), la poda ya la hace la política de contexto, y sin la referencia delante serían reglas inventadas sobre un oráculo que, además, aún no se ha medido contra un modelo real.
- Resolución automática del oráculo ("auto": clave de HiveAgents, luego OpenRouter) desde el almacén de secretos o el entorno: el host pasa `endpoint` y `api_key`.
- Banderas `share` (qué parte del turno viaja): como el host construye `state`, decide él qué incluye.
- Publicación de eventos al canvas y persistencia de contadores de ahorro; el estado del oráculo (cooldown, rachas) vive en la instancia, así que cada inquilino o proceso necesita la suya y no sobrevive a un reinicio.

## Evidencia

`crates/hive-core/tests/oracle.rs` (11 pruebas con servidor local: configuración, protocolo y ambas grafías, validación estricta, cooldown y recuperación, 401, timeout, respuesta enorme, cancelación, desconfianza, y carga al Objective incluyendo límites, uso ausente/asumido, reserva retenida y cooldown sin reserva) y una prueba por binding con servidor local. **No se ejecutó contra OpenRouter ni contra un modelo Kev/Jev real.**

## Pestaña «Oráculo» (escritorio)
Ajustes → **Oráculo** edita `GET/PUT /api/oracle`: activar, servicio (dirección, modelo, clave), servidor propio, correcciones máximas, umbrales y tiempo de espera. La clave se escribe, viaja solo si se escribió una nueva y no se vuelve a mostrar (solo si existe). Probado contra un servidor falso; el oráculo real no se ha medido.
