# Resiliencia del proveedor

Reintentos explícitos y un circuit breaker para las llamadas HTTP a los proveedores (OpenAI-compatible, Anthropic, Gemini y los presets). **Está apagado salvo que se configure** con una clave `resilience`:

```javascript
new OpenAICompatible({ apiKey, resilience: {
  retry: { max_attempts: 3, base_delay_ms: 500, max_delay_ms: 30_000 },
  circuit_breaker: { failure_threshold: 5, cooldown_ms: 30_000 },
}});
```

Python: `OpenAICompatible(..., resilience={...})`, igual en `PresetProvider`, `Anthropic` y `Gemini`. No aplica a `HiveAgents` (gestiona su propio backend y no se reintenta): configurarlo allí es un error.

## Qué se reintenta, y qué nunca

La regla de la matriz es **no repetir operaciones inciertas**. Una llamada a un modelo cuesta dinero y puede haberse procesado aunque no llegue la respuesta, así que solo se repite lo que **demostrablemente no se procesó**:

| Situación | ¿Se reintenta? |
|---|---|
| La conexión nunca se abrió (rechazada, DNS, red) | Sí |
| HTTP **429** (límite de tasa) o **503** (no disponible) | Sí |
| Timeout, conexión cortada a mitad, respuesta parcial, stream interrumpido | **No** (incierto) |
| 500, 502, 504 y cualquier otro 5xx | **No** (pudo procesarse) |
| 4xx (400, 401, 403, 404…) | **No** (es un error de la petición) |

Los reintentos ocurren **dentro de una sola llamada lógica**: no cuentan como otra llamada de modelo ni reservan presupuesto de nuevo (no hubo uso que cobrar), y quedan **dentro del timeout de la llamada y del deadline del turno**, de modo que ninguna espera los supera; la cancelación los interrumpe. `max_attempts` (1–5, incluida la primera) acota el total.

**Espera:** retroceso exponencial desde `base_delay_ms` con tope `max_delay_ms` y *equal jitter* (la mitad fija más una parte aleatoria). Si el servidor manda `Retry-After` en segundos, se espera al menos eso; si pide **más** que `max_delay_ms`, la llamada termina enseguida con el 429/503 en vez de esperar. (La forma con fecha HTTP de `Retry-After` se ignora.)

## Circuit breaker

Cuenta fallos consecutivos del endpoint del proveedor: fallos de conexión, timeouts, 429 y cualquier 5xx. Un 4xx o una respuesta correcta demuestra que el endpoint responde y lo reinicia. Al llegar a `failure_threshold` se **abre**: las llamadas fallan al instante con `PROVIDER_CIRCUIT_OPEN` **sin enviar nada** (un fallo conocido, no incierto). Pasado `cooldown_ms` deja pasar **una sola llamada de sondeo**: si responde, se cierra; si falla, se reabre. Un sondeo cuya llamada se cancela caduca tras un cooldown, así que el breaker no puede quedarse atascado. El estado es por instancia de proveedor, en memoria.

## Validación

`tests/resilience.rs` (9 pruebas con un servidor TCP guionizado), `packages/npm/test/resilience.test.mjs` y `packages/python/tests/test_resilience.py`: 429→503→éxito con 3 peticiones; el límite de intentos devuelve el último rechazo; **500, 502, 504, 400, 401, conexión cortada y timeout se envían exactamente una vez** aunque haya reintentos; fallo de conexión reintentado con esperas; `Retry-After` respetado y, si excede el tope, sin esperar; reintentos acotados por el timeout y cancelables; el breaker se abre sin tráfico, se cierra con un sondeo correcto, se reabre si el sondeo falla, no lo disparan los 4xx y un sondeo cancelado no lo atasca; configuración inválida rechazada.

**Alcance honesto:** probado contra servidores locales guionizados, no contra proveedores reales (que pueden devolver 429/503 con cuerpos y cabeceras propios). No hay métricas ni evento visible de «se reintentó N veces». El breaker no se comparte entre procesos ni entre instancias.
