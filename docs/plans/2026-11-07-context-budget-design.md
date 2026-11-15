# Diseño: contexto acotado en el motor compartido

Continuación del plan autorizado. Objetivo: acotar entrada en sesiones largas sin llamadas auxiliares dentro del loop.

Alternativas: un tokenizer fijo no representa todos los modelos; un contador por proveedor necesita un contrato posterior para costos y consultas auxiliares. Esta entrega elige bytes JSON explícitos y mantiene pendiente el conteo de tokens.

Rust prepara solicitudes desde el historial completo. La policy opt-in detiene o retira turnos antiguos completos, conservando sistema y turno activo. No reescribe el historial. Selección determinista al reanudar y conteo mediante un writer sin buffer completo. Para retirar mensajes descuenta sus tamaños y comas sin serializar de nuevo cada candidato.

Entrada obligatoria excesiva produce `context_limit` sin reserva ni despacho. Un resultado de tool conocido se guarda antes de parar. Metadata se expone a npm/Python y se excluye del body vendor.

Validación: frontera exacta UTF-8/escapes, schemas, sistema y turno activo inamovibles, lotes sin tools huérfanas, resultado grande durable, preflight idempotente, registros antiguos y reanudación entre lenguajes. No promete ventana de tokens ni máximo monetario.
