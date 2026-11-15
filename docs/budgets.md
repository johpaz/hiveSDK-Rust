# Reservas de salida y deadline del turno

Los límites están implementados en Rust y se aplican igual en npm y Python. No se hace una llamada auxiliar para calcularlos ni para explicar que se agotaron.

```javascript
const agent = new Agent({
  ...options,
  policy: {
    max_output_tokens: 2048,
    max_total_output_tokens: 8192,
    max_run_ms: 60_000,
  },
});
```

Python usa el mismo dict de policy. Los tres campos aceptan un entero positivo o null/None y están desactivados por defecto. Configurar solo salida agregada permite reservar todo lo restante en una llamada; añadir un cap por llamada lo reduce.

## Antes de inferir

El motor prepara `ModelRequest.max_output_tokens` con el menor cap aplicable y guarda `stats.reserved_output_tokens` cuando existe un presupuesto agregado. Una comprobación repetida no duplica la reserva. Solo hay una operación activa por turno; CAS impide que un actor obsoleto reclame esa reserva otra vez.

El adaptador compatible envía `max_completion_tokens`. La [documentación oficial](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create) lo define como límite de salida visible y razonamiento; `max_tokens` es el parámetro anterior y no sirve para todos los modelos. Elegir este último explícitamente con `outputTokenParameter: 'max_tokens'` o `output_token_parameter='max_tokens'` si el endpoint lo requiere. No se prueban ambos haciendo dos solicitudes.

Los proveedores personalizados reciben el contrato neutral y deben aplicar ese límite a su API. El deadline no se manda en el body del vendor: el transporte lo usa para limitar el tiempo local de la solicitud.

## Reconciliación

Ejemplo: salida total 500, cap por llamada 300. Se reservan 300 antes de la primera llamada. Si el uso real es 200, se liquida esa reserva y quedan 300; la siguiente puede reservar 300. No se estima el gasto restando caracteres del texto visible.

Uso conocido suma tokens reales y devuelve a cero la reserva activa. Si falta el uso y hay presupuesto agregado, el turno se detiene con `usage_unavailable` y retiene la reserva. Si el proveedor declara más salida de la permitida, se registra ese número real y se detiene con `output_limit`.

Una respuesta final puede completar exactamente en el límite. Si no queda salida y el modelo pide tools, se detiene antes del callback: no se inicia una acción cuya continuación ya no se puede financiar. Streams incompletos, timeout o cancelación sin reply confirmado dejan una operación incierta y su reserva retenida. Se aplican las reglas de [reconciliación de checkpoints](checkpoints.md), sin reintento automático.

## Deadline global

`max_run_ms` se convierte una sola vez en `deadline_unix_ms` al iniciar. Se persiste con el turno e incluye las pausas y reinicios de proceso. Reanudar conserva el timestamp original. No ampliar el reloj al recuperar un checkpoint.

El motor comprueba el deadline antes de despachar y el transporte HTTP lo aplica aunque su timeout de solicitud sea mayor. Los timers anfitriones propagan cancelación a callbacks cooperativos. El sistema depende de un reloj Unix confiable; no pretende resistir manipulación del reloj.

Un checkpoint limpio vencido termina como `{status:'stopped', stop_reason:'deadline_exceeded'}` sin llamar al proveedor. Si HTTP/modelo termina incierto al vencer, se lanza `DEADLINE_EXCEEDED` y se conserva su reserva; Python expone `DeadlineExceeded`, subclase de TimeoutError. Si una tool devuelve un resultado conocido tarde, se guarda antes de parar y no se pide otra síntesis al modelo.

Callbacks que bloquean o ignoran señales pueden retrasar el cierre local; no se matan threads por la fuerza. La siguiente llamada del harness sigue prohibida después del deadline. Una respuesta terminal ya confirmable no requiere más tiempo para otra inferencia: se puede finalizar ese checkpoint sin renovar su reloj.

## Qué cubre el presupuesto

Controla salida por turno del harness cuando el proveedor honra el parámetro y reporta uso fiable. No es un máximo monetario: la entrada, cachés, precios, tools externas y llamadas que el código de la aplicación haga por su cuenta no quedan limitadas por ese número.

`max_total_tokens` de LoopPolicy conserva su significado anterior: suma entrada+salida reportadas y detiene después de recibir el uso. El [presupuesto BPE local de entrada](tokens.md) admite framing JSON explícito, entrada acumulada por turno y ventana combinada con cap reservado; no sustituye uso real. Su `input_token_budget.max_total_tokens` es un límite separado de entrada con carga conservadora. `Objective` comparte llamadas, tools y salida entre delegados, sin reiniciar esos saldos por turno. Los registros antiguos se leen con los campos nuevos por defecto; cambiar la policy de una sesión exige una sesión nueva.
