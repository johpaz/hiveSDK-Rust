# APIs públicas asíncronas de almacenamiento

Se añaden factories open para SessionStore, Agent y Objective, además de variantes Async en npm y _async en Python para ámbitos, inspección, recuperación, resolución, cierre y cambio de plan. Las APIs síncronas existentes conservan sus firmas. Se eligen alternativas explícitas frente a cambiar retornos existentes a Promises o devolver objetos todavía no inicializados. El objeto de una factory solo se devuelve después de completar la apertura/carga y sus validaciones.

Node usará workers Rust para abrir/cerrar el store, abrir ámbitos/agentes y despachar comandos. Python compartirá un helper que envía I/O al executor, protege y espera el worker incluso con cancelaciones repetidas. Si una apertura crea un recurso y su waiter cancela, se cierra ese recurso en un worker antes de informar cancelación. No se cancela a mitad una transacción ni se devuelve un saldo ya reservado.

Inspección y aperturas derivadas retienen el store hasta terminar. Recuperación, resolución de operaciones de sesión y cierre usan un guard exclusivo, adquirido antes del primer await: no arrancan agentes, aperturas u otras operaciones mientras está pendiente esa intervención. Los guards síncronos comprueban también ese estado. Objective ofrece alternativas asíncronas para inspección, cancelación y reconciliación manual, con exclusión por instancia durante reconciliación y control de callbacks activos. No se transforma automáticamente la lógica de los callbacks de usuario.

Agent.open valida proveedores/tools/opciones antes de cargar, conserva el handle durable en Rust y obtiene metadata en el worker. setContextPlanAsync bloquea ejecuciones concurrentes del mismo agente y conserva CAS. Un resultado conocido tras cancelar se mantiene durable; el objeto existente debe reflejarlo antes de quedar libre o exigir reapertura según la operación.

Esta entrega añade alternativas públicas; todavía hay checks, inspecciones y control de Objective/Trigger síncronos dentro de sus flujos existentes. Migrar esas rutas y medir latencia de toda la aplicación siguen pendientes. Las consultas explícitas que exportan historiales grandes también tienen costo de serialización en el lenguaje anfitrión.

Pruebas: factories y paridad de alternativas, uso de Node 24/Python 3.10, guards adquiridos antes de await, cierre/recuperación frente a nuevas ejecuciones, cancelaciones repetidas con join, limpieza de recursos creados tarde, CAS de planes, errores tipados y regresión de APIs síncronas.
