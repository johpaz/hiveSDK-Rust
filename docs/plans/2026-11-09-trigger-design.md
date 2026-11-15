# Triggers fuera del ciclo Agent

La implementación usará un registro explícito del objetivo, desactivado al crear por defecto. Los callbacks no se serializan. Registrar o reabrir una definición no crea otro objetivo, reinicia saldo ni lanza un timer automáticamente.

Fuentes iniciales: intervalo con ancla UTC, fecha UTC única y evento externo con clave de deduplicación. Esto cubre recurrencia y activación proactiva sin introducir consultas LLM para decidir cuándo despertar. El contrato usa timestamps UTC portables; cron con zonas/DST no se simula mediante intervalos.

`tick` procesa a lo sumo una ocurrencia vencida por trigger. Para intervalos atrasados, coalesce al último vencimiento y reporta cuántos se omitieron; no ejecuta todo el atraso. `emit` procesa un evento explícito. Un watcher opt-in puede esperar hasta el siguiente vencimiento, separado del ciclo Agent y cancelable. No hay polling con modelos.

El claim consume un intento raíz y conserva ID/entrada/ocurrencia antes del callback. Repetir un evento confirmado devuelve su recibo. Una ocurrencia incierta bloquea nuevos despachos del trigger hasta reconciliación explícita; no se recupera automáticamente repitiendo efectos. En modo durable, estado de calendario y claim/contador se confirman juntos en HiveDB. El resultado se confirma sin devolver presupuesto ni olvidar incertidumbre.

El contexto puede ejecutar un Agent heredando objetivo, profundidad y cancelación, como delegación. Cualquier callback programático o modelo sigue siendo opt-in; la aplicación puede decidir no inferir según los hechos del evento. El ámbito espera a los agentes iniciados y los cancela/junta al fallar. Alcanzar un límite raíz detiene el watcher, sin nuevos intentos en un bucle de errores.

La desactivación impide nuevos callbacks. Cancelar una ejecución activa no garantiza rollback externo y conserva el claim si no existe resultado conocido. Resolver requiere revisar el efecto y detener al ejecutor original. Los watchers no sobreviven por sí solos al cierre del proceso: la aplicación los reconstruye y activa explícitamente, mientras el ledger y calendario durable conservan sus identidades.

Aceptación: inactividad por defecto; ningún saldo nuevo por disparo; deduplicación, coalescencia, deadline, cancelación, cuota/profundidad, crash y resolución; paridad entre lenguajes; ningún modelo de pago en las pruebas.
