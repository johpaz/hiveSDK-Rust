# Handles del motor en memoria

Los agentes sin SessionStore conservan el estado de conversación dentro de Rust, tanto en npm como en Python. Los callbacks siguen ejecutándose en su lenguaje anfitrión. Cada agente posee un objeto NativeEngine; no hay registro global de IDs ni liberación manual. El mutex se libera antes de devolver acciones al wrapper y nunca abarca un callback.

El puente anterior enviaba el motor serializado con cada comando y devolvía el estado completo junto a la acción. El handle recibe start, reply, tool_result, reject o tick sin estado, y devuelve la acción con el deadline necesario para el wrapper. Una acción model contiene su contexto seleccionado porque el proveedor personalizado necesita leerlo. Consultar messages exporta el historial confirmado de forma explícita; las consultas de fuentes y búsqueda devuelven solo su resultado.

## Confirmación y errores

Al iniciar un turno se clona el motor confirmado una vez para trabajar en Rust. Las transiciones mutan ese motor de trabajo. Un commit solo acepta complete o stopped y reemplaza el confirmado. Un abort elimina el trabajo; es idempotente y se ejecuta al salir del wrapper, incluso por error, cancelación o cierre del iterador.

Así messages durante un callback conserva el historial confirmado anterior. Una respuesta todavía observada por onEvent no se publica si el observador falla. Un resultado terminal confirmado queda disponible, también después de close del agente. Cada consulta devuelve datos independientes. Se conserva el comportamiento previo de herramientas con efectos externos: descartar el historial no deshace un efecto realizado ni reembolsa el presupuesto raíz. La aplicación decide si inicia una ejecución nueva.

Los schemas, presupuestos, contexto, razones de parada y uso siguen perteneciendo al mismo motor Rust. El handle no restaura snapshots proporcionados por el anfitrión; sus comandos rechazan campos de estado y opciones desconocidas. Las consultas de contexto usan el estado confirmado y cambiar un plan durante un turno falla.

## Medición local

Ejecutar `python3 scripts/build-dev.py` y `node scripts/measure-engine-bridge.mjs`. El script compara las acciones de ambos caminos en cada transición y genera [el reporte](../evals/engine-bridge.json). Suma bytes JSON UTF-8 enviados y recibidos durante la misma tarea, incluyendo commit/abort del handle. La preparación del historial y una lectura explícita de messages se contabilizan fuera de esa suma.

| Historial | Tools | Contexto máximo | Puente anterior, bytes | Handle, bytes |
| --- | ---: | ---: | ---: | ---: |
| 10 turnos, 1 KiB por mensaje | 3 | Sin límite | 624416 | 89177 |
| 100 turnos, 1 KiB por mensaje | 3 | Sin límite | 6004617 | 857777 |
| 100 turnos, 1 KiB por mensaje | 12 | Sin límite | 19599613 | 2796947 |
| 100 turnos, 8 KiB por mensaje | 12 | 64 KiB | 87758436 | 664856 |

El contexto transmitido dentro de las acciones de modelo está incluido. Ninguna solicitud al modelo cambia; el ahorro medido es de serialización local y no de tokens ni facturación. Los tiempos se imprimen como observaciones informativas, sin umbrales ni promesas de rendimiento. La construcción del contexto dentro de Rust y la copia al iniciar cada turno siguen teniendo costo.

## Alcance pendiente

Los agentes con SessionStore ahora usan un [handle durable y workers para los checkpoints](durable-handles.md), con su propia medición y [alternativas públicas asíncronas](async-storage.md). Los controles internos durables de Objective/Trigger también usan workers, con [medición local bajo concurrencia](objective-runtime.md); no se certifica latencia de producción. Las operaciones de este handle en memoria son síncronas; Python libera el GIL, pero eso no convierte una llamada directa en I/O asíncrono del event loop.

Las pruebas verifican equivalencia de acciones, prohibición de commit parcial y doble start, rollback tras una tool conocida, cancelación, abandono de streaming, getters defensivos y consultas de contexto. Las suites existentes cubren además salida estructurada, presupuestos, delegación y resúmenes con este camino.
