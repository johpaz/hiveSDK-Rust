# Objetivos por turno y delegación durable del gateway

Continuación del plan autorizado de paridad, sin proyectos ni procesos auxiliares. Se conserva el coordinador y su conversación actual; los especialistas usan el grupo/pool nativo.

Se consideraron tres rutas: conservar un presupuesto externo en memoria, abrir una nueva sesión por turno importando historial, o guardar un objetivo por turno en el checkpoint de la conversación existente. Se elige la tercera: evita recuentos separados, conserva el historial nativo y permite reclamar/liquidar permisos atómicamente. Un campo opcional de RunCheckpoint identifica el objetivo del turno de una sesión sin raíz fija. Los registros usan schema 6; las raíces permanentes no pueden ser reemplazadas y un turno activo no puede iniciar otro ámbito.

El gateway abre una raíz nueva únicamente para un turno nuevo; begin_run falla si hay un turno anterior interrumpido. No reanuda ni repite operaciones inciertas. El pool reclama los trabajos hijos bajo esa misma raíz, con sesiones derivadas por el núcleo. Una cola por objetivo aísla la capacidad de turnos inciertos. La política conserva la cuota de modelos y cuenta aparte el claim del trabajo además de la herramienta del padre.

La llamada individual mantiene su API y pasa por un grupo de un hijo. Una herramienta de lote explícito agrega paralelismo, con un máximo ligado al catálogo visible y techo 64. El coordinador recibe los resultados terminales completos y continúa desde su propio checkpoint; no se dispara además una sesión/trabajo independiente de síntesis. Los fallos conocidos llegan como tales; la incertidumbre bloquea la continuación y exige reconciliación. Los resultados sin comprobación se etiquetan unchecked.

La cancelación se registra en el grupo y la raíz, y espera los hijos. El adaptador reutiliza la narración durable existente, con claves independientes para tareas paralelas. Se validan otra vez usuario, conversación, delegación y especialista antes de ejecutar.

Validación: pruebas del núcleo sobre scope, CAS, historial, recuperación y reservas; fixtures del gateway con dos especialistas sincronizados, fallos antes de inferir y proceso terminado después de un efecto; regresión de cuota/narración/streaming en memoria, formatos, Clippy y bindings. Quedan fuera la migración de sesiones anteriores, UI de reconciliación, scheduler/bus generales y aceptación automática.
