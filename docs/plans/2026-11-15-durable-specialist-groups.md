# Grupos durables de especialistas

Continuación del plan de paridad autorizado, con APIs nuevas y proyectos excluidos. El núcleo Rust conserva la autoridad sobre membresía, scheduling, presupuesto, cancelación, resultados y pruebas. Los bindings solo ejecutan callbacks/proveedores de la aplicación.

Se eligen grupos cerrados creados con todos sus hijos en una transacción, frente al registro incremental de TS: evita grupos abiertos abandonados entre escrituras y permite que la síntesis dependa de una membresía fija. Cada grupo usa kinds propios para no reclamar trabajos de otro grupo en el mismo pool. Las sesiones se derivan del ámbito y la tarea; Agent valida el especialista/sesión al ejecutar desde el contexto de cola.

Los hijos usan el objetivo existente a la profundidad indicada. El coordinador recibe un trabajo único con profundidad menos uno, guardado bajo el mismo CAS de cola que su referencia en el grupo. Un resultado incierto no cuenta como terminal. Fallos/cancelaciones confirmados se incluyen en la síntesis. No se recortan resultados para hacer caber el prompt: un exceso de política falla sin publicar un trabajo parcial.

La comprobación de aceptación es optativa y explícita. El verificador durable recibe una referencia nativa al resultado completado y su fence/hash. La prueba por tarea copia un veredicto ya finalizado del ledger; no acepta un booleano proporcionado al publicar. Un resultado aprobado exige evidencia no vacía. La prueba queda inmutable y puede recuperarse del ledger tras una caída anterior a su publicación. No se añade un juez de modelo automático.

La cancelación del grupo y sus trabajos comparte transacción. No se libera un lane en ejecución antes de su confirmación; la incertidumbre conserva reconciliación explícita. Las operaciones de consulta no ejecutan trabajo al abrir.

Validación: atomicidad/rollback, CAS entre handles, deduplicación, ámbitos, presupuesto raíz agotado, incertidumbre, fallos, cancelación, límites de payload, pruebas incorrectas/pendientes/sin evidencia, agentes deterministas Node/Python, salida abrupta Node y recuperación Python con readback Node, y comparación seleccionada con TS. Gateway, bus/narración, scheduler y goal runner siguen siendo componentes pendientes del plan mayor.
