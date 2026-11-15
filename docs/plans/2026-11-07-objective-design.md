# Presupuesto de objetivo y delegación dentro del proceso

El presupuesto de cada Engine sigue siendo local. Un `Objective` explícito añade un límite compartido entre agentes: llamadas al modelo, intentos de tool (incluidos rechazos), salida reservada, deadline absoluto y profundidad. No se crea ningún agente ni llamada de revisión automáticamente.

Se elige un contador Rust protegido por mutex frente a copiar políticas a los hijos (multiplica presupuesto) o contadores exclusivos de cada binding (duplican semántica y permiten carreras). Cada despacho reserva antes de ejecutar. Respuestas conocidas reconcilian la reserva una sola vez; fallos conservan reservas y llamadas consumidas. Falta de uso con presupuesto de salida bloquea nuevas operaciones. No hay reset del objetivo al empezar otro turno.

Una tool delega mediante su contexto a un Agent proporcionado por la aplicación. Se heredan objetivo, profundidad y cancelación; se rechaza cambiar el objetivo. La ejecución padre espera a los hijos iniciados durante el callback antes de continuar. La delegación es opt-in y no se serializa como metadata MCP.

Primera entrega: objetivo dentro del proceso, sin importación de snapshots ni reanudación durable. Se rechaza combinarlo con SessionStore, también en hijos, antes de cualquier efecto: prometer persistencia con dos commits independientes permitiría perder consumo tras un crash. La fase de delegación completa queda pendiente de coordinación durable.

Pruebas: concurrencia real sobre reservas, límites antes del despacho, reducción del cap, uso ausente/excesivo, liquidación duplicada, deadline/cancelación terminal, profundidad y paridad npm/Python con hijos, rechazos, errores y ausencia de otra inferencia tras agotamiento. No se atribuye ahorro de calidad o costo sin evaluación.

## Entrega durable — 8 de noviembre

El modo dentro del proceso se conserva. La segunda entrega guarda el ledger del objetivo en HiveDB y vincula la configuración Engine a su ID. El claim de modelo/tool y la reserva/contador global se confirman en el mismo col_batch con CAS; la respuesta, uso real y liquidación del permiso también. Los rechazos de schema consumen el contador raíz en la transacción del rechazo. Un guard compartido por los ámbitos del mismo handle serializa estas transacciones breves; las llamadas externas siguen corriendo concurrentemente. No se añaden reintentos de modelos ni tools.

RunCheckpoint conserva objective_depth y Operation conserva objective_permit para modelos. La reanudación no reduce profundidad ni puede separar la sesión de su objetivo. resolveModel liquida el permiso y confirma el resultado una sola vez. Un crash mantiene claims y reservas, sin repetir consultas. Pruebas directas verifican rollback de la reserva si falla el CAS de sesión, concurrencia de reservas/liquidaciones y caída real de procesos en ambos bindings.
