# Evidencia de distribución: MSRV y avisos

Se valida Rust 1.95 con su toolchain real y target temporal separado; no se cambia el compilador predeterminado. Check cubre workspace y all-targets en Linux x64; pruebas de handles durables y schema final ejercitan contratos con ese compilador. CI recibe un job MSRV separado. Esto no certifica aún las otras plataformas.

Se genera un inventario determinista desde cargo metadata --locked --offline y las fuentes descargadas por Cargo. Incluye conservadoramente todas las dependencias resueltas del workspace y todos los targets, no pretende identificar qué código está enlazado en un binario concreto. Se conservan las expresiones declaradas sin elegir/reinterpretar ramas. Se copian avisos empaquetados recursivos, incluidos componentes vendorizados, y suplementos con commit/hash fijados cuando faltan en el crate.

Los documentos se deduplican por SHA-256. El catálogo conserva paquete/versión/fuente/checksum, rutas y procedencia de cada texto. Los suplementos se verifican contra la versión/checksum/VCS del paquete, sin consultas remotas durante generación o checks. La generación no modifica cachés de Cargo ni proyectos originales. No se inventan copyright holders ni licencias ausentes.

El inventario y agregado muestran gaps explícitos. --check detecta Cargo.lock/archivos modificados o salidas desactualizadas; --require-complete rechaza gaps mecánicos y revisiones manuales pendientes. Tener un fichero LICENSE no demuestra cumplimiento completo: la revisión de código/assets vendorizados y la correspondencia de licencias sigue explícita.

Se añade el agregado a los paquetes npm, bindings y wheel de preview con SHA de evidencia. Un paquete npm público no puede generarse con gaps; private:true continúa mientras falten validaciones. No se publica en este paso. Las pruebas verifican generación offline, checksums/procedencia, detección de gaps/lock desactualizado y contenido de los paquetes. Los workflows preparan la evidencia, sin declarar ejecutada la matriz remota.

## Avisos de submódulos

Los suplementos normales siguen exigiendo el commit VCS exacto del crate. Para el código vendorizado de tiktoken-rs se añade una cadena explícita, sin aceptar URLs arbitrarias de commits distintos. La alternativa de copiar un MIT genérico pierde procedencia; mantener todos los avisos solo como investigación impide que los consumidores reciban el aviso ya verificable. Se elige incorporar el documento verificado y conservar la revisión de assets abierta.

`via_submodule` liga el checksum del crate y su VCS al árbol GitHub fijado, `.gitmodules`, el gitlink del hijo y el blob del archivo vendorizado empaquetado. El árbol/configuración se guardan con SHA-256, el documento conserva sus bytes originales y el catálogo publica ambos commits. La generación sigue offline. Cambios de padre, hijo, repositorio, fuente o evidencia fallan; un árbol truncado se rechaza. El aviso hijo no satisface la ausencia de aviso del wrapper. Esto documenta procedencia, sin certificar licencias de vocabularios ni activar publicación.
