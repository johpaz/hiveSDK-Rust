# Verificación explícita de objetivos

Se elige `Objective.verify(key, value, callback, options)` como operación explícita fuera del ciclo Agent. Frente a un hook automático al terminar, evita pagar revisiones que la aplicación no pidió. Frente a otro ciclo de corrección, confirma un veredicto sin generar reintentos o reflexión. El resultado de Agent no se redefine como éxito de un objetivo externo.

La clave identifica una comprobación y la versión del criterio. Rust valida entrada/opciones antes del claim y el veredicto antes de confirmarlo. Un claim consume un intento del presupuesto raíz, con ID de ejecución. Misma clave/entrada/opciones confirmadas devuelve el resultado guardado; cambiar la entrada o criterio exige otra clave. Una operación pendiente no se repite automáticamente. El snapshot permite inspeccionarla y una resolución explícita aporta un veredicto conocido con el ID original.

Callbacks pueden comprobar recibos, tests o hechos externos. El contexto permite delegar a un agente revisor con el mismo objetivo, profundidad y cancelación. Esa ruta requiere opt-in `modelReview`, con referencia de evaluación y justificación; se registra en el ledger. No prueba por sí misma que una evaluación externa sea válida. El SDK no es un sandbox para callbacks que llamen directamente a otros servicios.

El modo durable confirma claim/contador y resultado/ledger en transacciones HiveDB. El cierre del ámbito espera a revisores iniciados y cancela/junta hijos al fallar. Confirmar un resultado conocido tras cancelación no libera intentos ni autoriza otra inferencia. No se reinicia el objetivo.

Pruebas: cero llamadas auxiliares por defecto; caché, claves incompatibles y veredictos estrictos; presupuesto de revisión compartido; cancelación, resultado tardío, caída real y reconciliación; paridad Node/Python. Una evaluación con fixtures reportará costo en llamadas/tokens y coincidencia con hechos conocidos, sin atribuir calidad de producción a modelos simulados.
