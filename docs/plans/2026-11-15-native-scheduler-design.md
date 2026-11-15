# Scheduler embebido sobre la cola nativa

Continuación del plan autorizado, con APIs nuevas, gateway opcional y proyectos excluidos. Se consideran ampliar el ledger de triggers de Objective, trasladar el cron del gateway sin cambiar su almacenamiento, o publicar ocurrencias dentro de la transacción de la cola. Se elige la tercera para el scheduler embebido; el gateway comparte el calendario de ese núcleo. El ledger de triggers conserva su API y no se añade un segundo cargo por trigger al job.

El feature scheduler incluye harness y depende opcionalmente de las versiones de chrono/chrono-tz ya fijadas en el workspace. Las definiciones son inmutables e idempotentes por tenant/cola/ID y empiezan pausadas. Fechas, intervalos y cron de cinco/seis campos calculan la siguiente ocurrencia en Rust. Se admiten zonas IANA, segundos, aliases, rangos/listas/pasos, DOM/DOW OR/AND y un horizonte de ocho años.

El tick guarda juntos definición actualizada, trabajo y clave idempotente mediante CAS de cola. Solo publica una ocurrencia por observación; skip con gracia o fire_once controla fechas perdidas. No avanza frente a cuota insuficiente, payload inválido o root rechazado. Un job no terminal bloquea la próxima publicación; una ejecución incierta exige reconciliar su efecto. Los límites de publicaciones y del objetivo no se reinician por reapertura/pausa.

El sondeo se inicia explícitamente y solo publica; los workers se dirigen por separado. El núcleo ofrece un loop Tokio; los wrappers conducen los mismos comandos nativos con sus runtimes anfitriones, sin duplicar calendario, misfires ni transacciones. Al cancelar esperan las operaciones de almacenamiento y no borran trabajos ya publicados.

El gateway reutiliza el calendario y cambia la clasificación de interrupciones/errores posteriores al despacho a uncertain, que bloquea repetición automática y manual. Conserva cronJobs/taskRuns; no migra filas al nuevo almacenamiento. Su administración completa y la integración de triggers cron en Objective quedan pendientes.

Validación: carreras entre handles, rollback, cuotas, presupuesto sin renovación, fechas perdidas, no solapamiento, scopes/paginación, calendario/DST/bisiestos, productores cancelables, bindings y caída real Node → Python → Node. Se compara con la referencia TS fijada. Nueve casos coinciden; se documenta y prueba por separado la corrección del indicador de días restringidos para */2, que la referencia TS ignora. No se declara paridad total ni se habilita publicación.
