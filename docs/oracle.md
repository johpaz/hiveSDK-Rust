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

## No implementado (sigue pendiente)

- El **planificador** de la referencia (`jev-planner.ts`): construir las preguntas del bucle (terminar o continuar, paralelismo de herramientas, poda de historial y resultados, verificación de resultados y de respuesta, correcciones máximas). El host arma estado y preguntas y decide qué hacer con las respuestas.
- Resolución automática del oráculo ("auto": clave de HiveAgents, luego OpenRouter) desde el almacén de secretos o el entorno: el host pasa `endpoint` y `api_key`.
- Banderas `share` (qué parte del turno viaja): como el host construye `state`, decide él qué incluye.
- Publicación de eventos al canvas y persistencia de contadores de ahorro; el estado del oráculo (cooldown, rachas) vive en la instancia, así que cada inquilino o proceso necesita la suya y no sobrevive a un reinicio.

## Evidencia

`crates/hive-core/tests/oracle.rs` (11 pruebas con servidor local: configuración, protocolo y ambas grafías, validación estricta, cooldown y recuperación, 401, timeout, respuesta enorme, cancelación, desconfianza, y carga al Objective incluyendo límites, uso ausente/asumido, reserva retenida y cooldown sin reserva) y una prueba por binding con servidor local. **No se ejecutó contra OpenRouter ni contra un modelo Kev/Jev real.**
