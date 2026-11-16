# Continuación: secretos y herramientas nativas

Se mantiene el alcance aprobado: paridad funcional progresiva, APIs propias, motor Rust sin procesos auxiliares y exclusión de proyectos. Este bloque cierra los cambios existentes de vault y herramientas comunes; no habilita publicación.

El vault vive sobre el SessionStore y separa metadatos de revelación explícita. AES-GCM autentica ámbito, revisiones y tombstones; ancla y registros se actualizan mediante CAS atómico. La rotación usa la misma transacción para todas las filas, con reintento completo ante concurrencia. El host conserva y aprovisiona claves fuera de HiveDB. Los bindings pasan claves como bytes, mantienen vivo el store y esperan operaciones pendientes durante cancelación. Las referencias de catálogo no activan resolución automática de credenciales.

Las herramientas se agrupan en un feature opcional `tools`: seis operaciones de archivos, ejecución directa de programas permitidos y GET/búsqueda web configurables. El Agent conserva claims, presupuesto, ledger e incertidumbre. La raíz canonicalizada protege rutas normales pero no constituye aislamiento del proceso ni previene cambios concurrentes del árbol; los binarios permitidos conservan sus capacidades del sistema operativo.

Se acotan lecturas, reemplazos, recorridos y salida. Glob usa programación dinámica para evitar explosión combinatoria. Los procesos tienen timeout, cancelación y tareas de pipes que se abortan al terminar. La web fija las direcciones DNS validadas, desactiva proxies implícitos y redirecciones, y aplica cancelación/timeout al cuerpo completo. Los endpoints de búsqueda son configuración de confianza.

Aceptación: pruebas Rust/Node/Python, consumo TypeScript strict y reapertura Node → Python → Node tras salida abrupta. Los contratos que necesitan sockets quedan en CI; EPERM local se registra como limitación, nunca como prueba aprobada. La matriz sigue parcial para operaciones TS restantes, resolución de referencias, HTTP genérico y extracción HTML.
