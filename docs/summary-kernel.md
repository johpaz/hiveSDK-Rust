# Kernel de resúmenes presupuestados

El kernel Rust registra generación explícita de resúmenes bajo `Objective`. Los [bindings npm/Python](summary.md) ejecutan los callbacks y propagan el ámbito por delegación/checkpoints. La búsqueda local y los planes con resúmenes proporcionados están disponibles en [memoria](memory.md). El kernel por sí solo no inicia modelos ni callbacks.

`SummaryInput` contiene la fuente capturada y sus mensajes completos. El kernel comprueba índices, hash y límites antes de consumir presupuesto: entrada JSON de hasta 1 MiB, clave de 1–256 bytes y turnos user cerrados. `begin_summary` registra un ID de ejecución y consume un intento raíz, compartido con tools, triggers y verificaciones. No concede saldo adicional de modelos ni salida.

Una clave confirmada con la misma fuente devuelve un recibo, sin repetir trabajo. Fuente distinta con esa clave se rechaza; una clave pendiente devuelve `SummaryUncertain`. `finish_summary` exige el ID vigente y texto no vacío de hasta 64 KiB UTF-8. Un ID antiguo o una segunda resolución no sobrescriben el resultado. El recibo contiene `summary` y su costo; si su fuente cubre el prefijo requerido, se puede instalar como dato del plan. No instala automáticamente el plan ni certifica la calidad del texto.

## Costo atribuible

Un driver Rust debe reservar cada llamada auxiliar con `reserve_summary_model(key, depth, cap)`, transmitir cap/deadline al proveedor y liquidar el permiso con `settle_model`. Las tools y rechazos del ámbito usan `count_summary_tool`. Profundidad mínima 1, bajo el mismo máximo raíz; cerrar el resumen impide nuevos efectos de ese ámbito. Estas rutas consumen las cuotas globales normales. Llamar directamente a un proveedor fuera del driver no queda convertido por el SDK en trabajo medido.

Cada permiso se conserva en su resumen. `SummaryCost` informa llamadas de modelo, intentos de tools (incluido el claim inicial), entrada/salida reportadas, llamadas pendientes, llamadas confirmadas sin medición y salida todavía reservada. No se calcula como diferencia de snapshots globales: el trabajo concurrente de otros resúmenes o del worker no se atribuye a este. `Objective.snapshot().summary_costs` muestra estos costos incluso mientras el resumen sigue pendiente.

Una respuesta desconocida mantiene su reserva. Uso ausente se muestra explícitamente, conserva salida reservada y detiene el objetivo. Un resultado conocido se puede guardar después de cancelar o vencer el deadline, sin reiniciarlos. Si después se reconcilia uso conocido de un permiso pendiente, la consulta del recibo refleja ese costo actualizado. Antes de resolver manualmente hay que detener al ejecutor anterior y comprobar su resultado.

## Persistencia y estado de entrega

Los comandos internos compartidos son `summary_begin`, `summary_finish`, `summary_check`, `summary_model` y `summary_tool`. El documento raíz usa formato 4 y lee 1–3 conservando sus saldos. Los lectores que desconocen formato 4 fallan antes de efectos. La validación comprueba la pertenencia exclusiva de permisos, reservas/settlements, contadores globales y entradas con hashes válidos. Los planes de memoria mantienen formato de sesión 4; los checkpoints atribuidos a un resumen usan formato 5 y conservan clave/profundidad. Las sesiones sin esas funciones conservan los formatos previos.

Las pruebas Rust cubren resúmenes programáticos, caché, fuente incompatible, resolución obsoleta, costos separados, cuota/profundidad, salida incierta, uso ausente y liquidación tardía. Los bindings añaden callbacks, cancelación/join, delegación anidada y reapertura de checkpoints con el saldo restante. `tests/summary-parity.py` comprueba Node → Python → Node sin repetir el callback ni tools conocidas. La evaluación de fixtures y sus límites están en `docs/validation.md`.
