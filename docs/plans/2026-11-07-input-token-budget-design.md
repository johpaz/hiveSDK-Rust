# Diseño: presupuesto BPE local de entrada

Continuación del plan autorizado: añadir conteo antes de inferir sin pagar consultas auxiliares.

Se consideró adivinar codificación por nombre del modelo, consultar al proveedor o elegir explícitamente una codificación local. Los alias hacen frágil la primera opción; la segunda añade red y posibles costos. Se elige un perfil explícito persistido con el modelo, inicialmente cl100k_base u o200k_base, con máximo y margen configurables.

El objeto tokenizado es el JSON neutral compartido, con un reporte llamado token_estimate. No se afirma que reproduzca la plantilla privada del vendor. El uso real permanece independiente. La codificación y vocabularios de tiktoken-rs se fijan a 0.12.1 y se inicializan una sola vez por proceso.

El límite complementa el de bytes. Conserva la selección de turnos completos, sistema, turno activo, historial íntegro y checkpoints. BPE se recalcula sobre candidatos completos, ya que sus conteos no son aditivos sobre fragmentos. Si no cabe, context_limit antes de reservar o despachar; la preparación local también está sujeta al deadline. No hay fallback, inferencia de resumen o reintento.

Validación: dos codificaciones, UTF-8/escapes y literales especiales, frontera exacta, margen, consumo real independiente, paridad completa/streaming, selección y reanudación durable, campos antiguos y perfiles inválidos. El framing por proveedor, acumulación de entrada y ventana combinada siguen pendientes.
