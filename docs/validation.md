# Validación inicial — 7 de noviembre de 2026

Entorno probado: Linux x64, Rust 1.99.1, Node 26.13.0, Python 3.10 y Python 3.15. No se han probado macOS, Windows, ARM64 ni instalación desde registros públicos.

| Verificación | Resultado |
|---|---|
| `cargo check --workspace` | Correcto; dependencias faltantes descargadas durante preparación. |
| `cargo test --offline` | 13 pruebas del motor correctas. |
| `python3 scripts/build-dev.py` | Ambos bindings nativos compilados y copiados a sus paquetes locales. |
| `node --test packages/npm/test/*.test.mjs` | 7 pruebas correctas. |
| `PYTHONPATH=packages/python/python python3 -m unittest discover -s packages/python/tests -v` | 7 pruebas correctas en Python 3.15. |
| Mismo comando con `python3.10` | 7 pruebas correctas, usando el mismo binario abi3. |
| `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | Correcto, sin advertencias. |
| `cargo fmt --all -- --check` | Correcto. |

Las pruebas HTTP de npm y Python requieren permiso para abrir un puerto efímero en localhost. La primera corrida restringida falló por bloqueo del sandbox; las corridas autorizadas pasaron. No se saltaron esas pruebas ni se sustituyó el transporte nativo por un mock.

Se verificaron respuesta directa, ciclo con tool, historial, errores de tool, fallo del modelo, ejecución concurrente, HTTP nativo, uso ausente, límites de llamadas/tokens y repetición. Los presupuestos y repetición se prueban también directamente en Rust, incluida la excepción para polling y la parada dentro de un lote.

Sin llamadas a modelos de pago. Estas pruebas verifican comportamiento y paridad básica; no prueban calidad de respuestas, reducción real de costos, tolerancia a crashes ni compatibilidad de los paquetes finales. La evaluación de consumo y calidad descrita en `agent-loop.md` sigue pendiente.

## Integración HiveDB

- `cargo check --workspace --locked --offline`: correcto con el crate remoto fijado de HiveDB.
- `cargo test --locked --offline`: 21 pruebas, incluidas 8 de persistencia y 13 del loop.
- `python3 scripts/build-dev.py`: ambos bindings recompilados con HiveDB enlazado directamente.
- Suite npm: 12 pruebas correctas.
- Suite Python: 12 pruebas correctas tanto en Python 3.15 como en Python 3.10, con el mismo binario abi3.
- `PYTHONPATH=packages/python/python python3 tests/session-parity.py`: sesión creada en Node, continuada en Python y reabierta en Node correctamente.
- Clippy sin advertencias y formato Rust comprobado.

Las pruebas nuevas verifican reapertura, separación de identificadores, conflictos anteriores a cualquier llamada del proveedor, configuración incompatible, recuperación explícita y persistencia de paradas de política. Simulan una interrupción cerrando/reabriendo el handle sin confirmar; no incluyen inyección de fallos de disco ni garantía de tolerancia a cortes de energía.

## Checkpoints dentro del turno

- Rust: 29 pruebas correctas (13 del motor, 8 de sesiones y 8 de checkpoints).
- npm: 15 pruebas correctas.
- Python: 15 pruebas correctas, ejecutadas en Python 3.10 y 3.15.
- La prueba cruzada comprueba historial Node → Python → Node y un checkpoint después de una tool creado en Node y reanudado en Python sin repetirla.
- Compilación de ambos bindings, Clippy y formato Rust verificados.

Las suites de npm y Python incluyen terminación real del proceso hijo dentro de una tool, después de escribir un recibo externo de prueba. Al reabrir, la operación queda incierta y la reanudación se bloquea. Tras aportar el resultado verificado, el contador del callback confirma cero repeticiones.

También se verifica resolución de una respuesta de modelo sin otra consulta, conservación de uso y detector de repetición al reanudar, fencing por revisión, lectura de registros del formato anterior y conservación del ledger de turnos abandonados. Estos escenarios no prueban exactamente una ejecución en sistemas externos, cortes de energía ni recuperación ante corrupción.

## JSON Schema y feedback de argumentos

- Rust: 40 pruebas correctas, incluyendo 10 de validación y una de actualización de checkpoint antiguo con argumentos inválidos.
- npm: 20 pruebas correctas.
- Python: 20 pruebas correctas en Python 3.10 y 3.15.
- Paridad de historial y checkpoints entre npm/Python: correcta.
- Ambos bindings recompilados, Clippy sin advertencias y formato Rust correcto.

Los casos nuevos verifican schemas inválidos al registrar; tipos, required, límites, arrays, formatos, referencias incluidas, combinadores y condicionales; rechazo de referencias HTTP/file sin recuperación externa; ausencia de coerción/defaults; feedback acotado; corrección por el modelo sin ejecutar la llamada inválida; y persistencia del rechazo en el ledger.

También se prueban parada por rechazos repetidos, consumo del presupuesto de intentos dentro de un lote y revalidación de una tool pendiente guardada por una versión anterior. Las pruebas no certifican todos los casos de la especificación ni miden rendimiento/aislamiento frente a schemas maliciosos. No usan llamadas a modelos de pago.

## Transporte asíncrono, streaming y cancelación

- Rust: 56 pruebas correctas; incluye 9 de parser/ensamblado SSE y 7 de transporte/configuración.
- npm: 30 pruebas correctas.
- Python: 30 pruebas correctas en Python 3.10 y 3.15.
- Historial y checkpoints Node/Python: paridad correcta.
- Ambos bindings recompilados; Clippy y formato Rust correctos.
- Declaraciones npm comprobadas con TypeScript estricto y sin emisión, usando el compilador ya instalado del SDK original.

Los servidores locales verifican que llegan deltas antes de liberar la respuesta final, que las tools fragmentadas se ensamblan y ejecutan solo tras terminar, y que cada respuesta suma su uso una vez. Se verifica reutilización de conexión para respuestas JSON, cierre del socket antes de headers, timeout durante SSE y cancelación con backpressure/iterador pausado.

También se comprueba cierre al devolver/cerrar el iterador, ausencia de despacho ante una señal ya cancelada, rechazo de EOF prematuro sin ejecutar tools, reanudación streaming con uso acumulado sin repetir una tool y guardado de un resultado conocido antes de detener la continuación. Una tool síncrona Python mantiene el agente ocupado hasta terminar; no se prueba ni ofrece terminación forzosa de threads.

Todo usa HTTP/SSE local y proveedores deterministas, sin solicitudes a modelos de pago. La prueba de cierre de socket acredita cancelación del transporte local; no demuestra que un servicio remoto deje de generar/facturar ni garantiza un máximo monetario por turno.

## Reservas de salida y deadline global

- Rust: 68 pruebas correctas, incluidas 9 de presupuestos y 10 de transporte.
- npm: 36 pruebas correctas.
- Python: 36 pruebas correctas en Python 3.10 y 3.15, usando el mismo binding abi3.
- Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos.

Se comprueban reserva antes del despacho, reducción del cap siguiente según uso real, retención de reservas ante uso desconocido y reconciliación una sola vez tras una interrupción. HTTP JSON y SSE reciben el parámetro moderno o legacy elegido explícitamente, sin una segunda solicitud de fallback.

Los casos de tiempo verifican cierre del socket por deadline global, ausencia de conexión si ya venció, comprobación después de hooks y antes de reclamar una operación, conservación del timestamp al reanudar y guardado de una tool tardía sin otra inferencia. Las pruebas usan HTTP local y callbacks deterministas, sin modelos de pago. El límite controla salida reportada; el conteo previo de entrada y un techo monetario no están implementados. Ver [contrato de presupuestos](budgets.md).

## Contexto acotado en bytes JSON

- Rust: 76 pruebas correctas, incluidas 8 de contexto.
- npm: 40 pruebas correctas.
- Python: 40 pruebas correctas en Python 3.10 y 3.15 con el mismo binding abi3.
- Paridad cruzada: historial Node → Python → Node, checkpoint tras tool y checkpoint con contexto acotado Node → Python correctos.
- Ambos bindings recompilados; Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos.

Se comprueban frontera exacta de bytes UTF-8 y escapes JSON, inclusión de schemas, sistema y turno activo inamovibles, selección de los turnos más recientes y retiro de lotes de tools completos. También resultado grande guardado antes de detener, reserva cero cuando no se despacha, comprobaciones repetidas sin mutación, lectura de campos ausentes en registros anteriores y reconstrucción al reabrir HiveDB manteniendo todo el historial.

Los casos de integración prueban respuestas completas y streaming con proveedores propios. El límite mide el contrato JSON neutral, no tokens ni tamaño HTTP. No se hicieron llamadas a modelos de pago ni se midieron ahorro o calidad de respuestas. Ver [alcance del contexto](context.md).

## Presupuesto BPE local de entrada

- Rust: 84 pruebas correctas, incluidas 8 de tokenización y presupuesto de entrada.
- npm: 44 pruebas correctas.
- Python: 44 pruebas correctas en Python 3.10 y 3.15 con el mismo binding abi3.
- Paridad Node/Python: historial, checkpoints de tools y reporte/selección de contexto con perfil BPE persistido correctos.
- Ambos bindings recompilados; Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos. Tipos públicos de tokenización importados también en Python 3.10.

Las pruebas comparan los conteos de ambas codificaciones con encode_ordinary del tokenizer sobre el mismo sobre neutral. Incluyen Unicode, escapes, literales que parecen tokens especiales, frontera exacta, margen, perfiles inválidos y ausencia de adivinación por alias. Comprueban el límite BPE solo y combinado con bytes, preflight idempotente, reanudación durable, configuración incompatible y lectura de campos ausentes en registros anteriores.

Los bindings verifican respuestas completas y streaming, cero despacho y reserva ante entrada excesiva, historial íntegro al reabrir y guardado de resultados grandes de tools antes de detener. Uso real reportado y llamadas sin medición permanecen independientes de la estimación. Los vocabularios se compilan con el crate; las pruebas se ejecutan sin recuperación de tokenizers ni llamadas a modelos de pago.

Esta validación no certifica equivalencia con plantillas o conteos facturados por servicios remotos, ventanas combinadas, ahorro de producción ni rendimiento con historiales grandes. Ver [contrato del conteo](tokens.md).

## Anthropic Messages y transporte compartido

- Rust: 99 pruebas correctas, incluidas 12 de protocolo Anthropic y 3 de transporte HTTP nativo Anthropic.
- npm: 50 pruebas correctas, incluidas 6 de integración Anthropic.
- Python: 50 pruebas correctas en Python 3.10 y 3.15, incluidas las mismas 6 de integración.
- Ambos bindings recompilados. Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos.
- Paridad de historial, checkpoints de tools y contexto/BPE entre Node/Python correcta.

Se verifican ruta Messages, headers de autenticación/versión, default de max_tokens y prioridad del cap neutral; system, schemas, agrupación de resultados de tools e indicador de error; texto/bloques JSON y SSE, fragmentación UTF-8, argumentos parciales y sus índices neutrales; input ordinario más caché y snapshots de uso acumulado sin duplicar consumo confirmado.

Los servidores locales comprueban llegada de deltas antes de liberar el resto de la respuesta, reutilización de conexión entre JSON/SSE, cierre por cancelación y deadline global antes de headers, y fallo por EOF sin message_stop. Las integraciones prueban ausencia de ejecución de tools ante ese EOF, reserva/claim inciertos conservados, reanudación tras una tool sin repetirla ni reiniciar consumo/cap y cero reintentos ante truncación, pausa, refusal y HTTP 429.

La regresión detectó un cambio accidental en el texto del diagnóstico OpenAI por EOF. Se restauró su referencia a [DONE], se dejó message_stop para Anthropic y se repitieron las suites completas correctamente. No se cambiaron las expectativas de las pruebas antiguas para aceptar la regresión.

Todo se validó con proveedores deterministas y HTTP/SSE local, sin claves reales ni llamadas de pago. No se certifican modelos remotos, compatibilidad cloud ni las capacidades excluidas del [contrato Anthropic](anthropic.md).

## Gemini y estado firmado entre lenguajes

- Rust: 115 pruebas correctas, incluidas 12 de Gemini y 4 de su transporte.
- npm: 56 pruebas correctas, incluidas 6 de Gemini.
- Python: 56 pruebas correctas en Python 3.10 y 3.15, usando el mismo binding abi3.
- Ambos bindings recompilados; Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos.
- Paridad: checkpoints firmados Node → Python → Node conservan partes originales, consumo y cap restante sin repetir la tool. También pasan los casos anteriores de historial, tools y contexto/BPE.

Se comprueban rutas GenerateContent/SSE, key en header, IDs de modelo sin inyección de ruta, schemas JSON y caps. Las pruebas conservan firmas en texto, functionCall, texto vacío y partes de solo firma, ocultan texto thought del contenido visible y rechazan incoherencias de estado antes de cambiar el motor. IDs remotos y locales mantienen su asociación; resultados sin ID remoto no inventan uno.

Se verifican uso acumulado, caché sin doble suma, candidatos+thoughts, tool-use prompt y uso ausente/inválido. Streaming exige STOP y fin HTTP/SSE limpio; EOF sin STOP o dentro de un evento final no produce reply. Se comprueban UTF-8 fragmentado, conexión reutilizada, cierre por cancelación/deadline, rechazo de firmas por otro protocolo antes de conectar, almacenamiento/reapertura y recorte de turnos completos conservando el estado activo.

Las integraciones incluyen JSON y SSE, reanudación JSON→SSE, cero tools ante respuesta incompleta, reservas inciertas y cero reintentos ante truncación, seguridad, llamadas malformadas o HTTP 429. La regresión completa conserva las pruebas anteriores de OpenAI y Anthropic.

Todo usa HTTP/SSE local y payloads deterministas, sin claves reales ni modelos de pago. No certifica modelos remotos, Interactions, Vertex o capacidades excluidas del [contrato Gemini](gemini.md).

## MCP stdio y Streamable HTTP

- Rust: 129 pruebas correctas, incluidas 9 de stdio y 5 de HTTP MCP.
- npm: 65 pruebas correctas, incluidas 6 de stdio y 3 de HTTP MCP.
- Python: 65 pruebas correctas en Python 3.10 y 3.15, con el mismo binding abi3.
- Ambos bindings recompilados; Clippy y declaraciones TypeScript estrictas correctos.

Se verifican handshake, revisión explícita, descubrimiento paginado y acotado, schemas inválidos y referencias externas, catálogo duplicado y ciclos de paginación. Stdio prueba entorno filtrado, stderr, ping, solicitudes no soportadas, framing, salida malformada, concurrencia, cancelación, timeout y recogida del hijo directo. HTTP comprueba headers, sesiones, JSON/SSE, ping durante SSE, cancelación, pérdida de respuesta, cambios de sesión, redirects y ausencia de reintentos.

Las integraciones prueban corrección de argumentos antes del despacho, metadata con identidad durable, errores conocidos, claim retenido al perder respuesta y reconciliación manual sin repetir la tool. `ToolOperationUncertain` también evita que callbacks propios conviertan un resultado incierto en feedback seguido por otra inferencia.

Se usan procesos y servidores locales deterministas, sin solicitudes de pago. No se certifica interoperabilidad con todos los servidores MCP, OAuth, protocolo 2026 ni rollback de efectos externos. Ver [alcance MCP](mcp.md).

## Objetivos compartidos y delegación dentro del proceso

- Rust: 136 pruebas correctas, incluidas 7 de objetivo compartido.
- npm: 76 pruebas correctas, incluidas 11 de objetivo/delegación.
- Python: 77 pruebas correctas en Python 3.10 y 3.15; incluye 12 de objetivo/delegación.
- Ambos bindings recompilados; Clippy sin advertencias.

El motor prueba 20 threads compitiendo por un saldo de cinco permisos, reconciliación una sola vez, límites de profundidad/llamadas/tools, saldo sin reset, uso ausente/excesivo, cancelación terminal y deadline absoluto. Las integraciones verifican saldo padre/hijo, reducción de cap entre llamadas, rechazos sin efectos, rechazo de otro objetivo, cancelación de hijos activos, espera de hijos no esperados por el callback, cierre del contexto, streaming y ausencia de continuación después de un fallo.

Python añade cancelación del hijo antes de que empiece su callback de proveedor. La planificación puede conceder su reserva antes de propagar el fallo del padre; se conserva ese permiso y no se interpreta como capacidad libre. Se prueba en ambas versiones Python.

Se rechaza explícitamente mezclar objetivos dentro del proceso con sesiones persistentes. Estas pruebas no certifican recuperación durable del presupuesto raíz, aislamiento de callbacks propios ni ahorro de modelos reales. No se hicieron llamadas de pago. Ver [alcance del objetivo](objectives.md).

## Paquetes instalados y mínimo Node 24 — 8 de noviembre de 2026

- npm: 78 pruebas correctas en Node 24.15.0 y 26.13.0, incluyendo dos casos de resolución/manifiestos de plataformas.
- Declaraciones TypeScript estrictas correctas; historial, tools, contexto/BPE y checkpoints firmados Node/Python conservan paridad.
- Workflows de regresión y seis plataformas validados con actionlint 1.7.12. No se ejecutaron todavía en GitHub.
- Build optimizado local de ambos bindings correcto. Primer wheel local `cp310-abi3-linux_x86_64` instalado en entornos Python 3.10 y 3.15 con PATH limitado, sin Rust/Bun.
- El primer tarball GNU construido en el host falló al cargar en Debian: requería GLIBC_2.38. Se reconstruyó desde fuentes en `manylinux_2_28_x86_64`, con Rust 1.99.1 y Maturin 1.16.0; símbolos GLIBC hasta 2.28 y wheel auditado `cp310-abi3-manylinux_2_28_x86_64`.
- Tarballs corregidos instalados offline, sin scripts y sin binding de desarrollo, en consumidores locales Node 24.15.0/26.13.0 y contenedor Debian con Node 24.22.2. Tools, HiveDB y objetivo compartido correctos.
- El mismo wheel manylinux pasó instalación y smoke en Python 3.10/3.15 locales aislados y contenedor Debian con Python 3.10, sin compiladores Rust ni Bun en PATH.

El paquete principal npm exige Node >=24 y mantiene `private: true`, al igual que los manifiestos nativos generados. Las pruebas locales suministran el tarball nativo explícitamente; no verifican resolución desde registros públicos. Todavía faltan las otras cinco plataformas, inventario completo de licencias y publicación.

`cargo package -p hive-agent-core --locked --offline --no-verify` confirmó otro pendiente: la dependencia Git de `hivedb-core` carece de requisito de versión publicable. Cargo elimina el origen Git al empaquetar y exige una versión resoluble en crates.io. No se cambió silenciosamente la dependencia ni se publicó otro proyecto para sortearlo.

## Presupuesto raíz durable y coordinación de checkpoints

- Rust: 146 pruebas correctas; se añaden 9 de objetivos durables y una de rollback de transacción.
- npm: 85 pruebas correctas en Node 24 y 26; 7 casos nuevos de objetivo durable.
- Python: 84 pruebas correctas en Python 3.10 y 3.15; los mismos 7 casos de integración durable.
- Ambos bindings recompilados; Clippy sin advertencias, formato Rust y contrato TypeScript estrictos correctos.
- Paridad nueva: objetivo/checkpoint creado en Node, reanudado en Python y leído de nuevo en Node conserva ID, saldo, cap reducido y uso confirmado sin repetir la tool.
- La paridad anterior de historial, tools, contexto/BPE y firmas Gemini sigue pasando. CI incluye ambas verificaciones.

Se prueban reapertura sin otra asignación ni deadline nuevo, separación por tenant, política incompatible, intentos compartidos entre ejecuciones y rechazos, consumo de hermanos, reservas concurrentes sin exceder el saldo y liquidación simultánea de ocho respuestas sin perder uso. Un test directo fuerza un CAS de sesión inválido en el mismo batch de la reserva raíz y verifica rollback completo.

Los bindings prueban padres e hijos persistentes, alias del mismo objetivo, reducción de cap, checkpoint limpio, profundidad conservada al reanudar directamente un hijo, rechazo al quitar el objetivo de una sesión vinculada, cancelación durable y bloqueo de cierre durante ejecuciones activas. Una respuesta sin uso deja reserva retenida y parada durable.

Ambos lenguajes incluyen terminación real del proceso dentro del callback del proveedor. Al reabrir, la reserva y el claim siguen presentes; no hay una segunda consulta. Aportar una respuesta verificada confirma checkpoint y liquidación una sola vez; una resolución repetida falla.

Todo usa callbacks y almacenamiento de prueba, sin modelos de pago. No acredita recuperación ante corrupción, cortes físicos de energía, idempotencia de servicios externos ni ahorro de producción. Los artefactos de distribución construidos en el apartado anterior pertenecen al snapshot previo: deben reconstruirse y volver a probarse con estos cambios antes de una publicación.

## Verificadores explícitos — 9 de noviembre de 2026

- Rust: 155 pruebas correctas; 7 de verificación y 2 adicionales de formatos/versionado.
- npm: 95 pruebas correctas en Node 24 y 26, incluidas 10 de verificación.
- Python: 96 pruebas correctas en Python 3.10 y 3.15, incluidas 12 de verificación.
- Clippy sin advertencias, formato Rust y declaraciones TypeScript estrictas correctos. Workflows comprobados con actionlint.
- Paridad nueva: veredicto confirmado en Node leído desde Python sin callback, entrada Unicode y orden de propiedades conservados; verificación interrumpida/resuelta en Python leída desde Node sin repetir trabajo.

Se comprueban ausencia de verificación automática, caché de éxitos y fallos, claves/entradas incompatibles, límites antes del claim, veredictos estrictos, resolución con ID vigente y rechazo de resoluciones repetidas. Los revisores requieren metadatos de evaluación, heredan cuota y profundidad, y no pueden añadir una llamada cuando se agota el saldo. Se espera a revisores no esperados por el callback, también al cancelarlos.

Ambos bindings prueban terminación real del proceso después de escribir un recibo de prueba dentro del verificador. El claim pendiente se conserva; al reabrir no se ejecuta el callback. Aportar un veredicto comprobado y leerlo de nuevo no añade intentos ni llamadas al modelo.

Python prueba cancelación repetida de un callback síncrono y cancelación durante el cierre del watcher. Esta última detectó un handle que podía quedar BUSY; el cierre ahora espera la liquidación y libera el registro del store incluso si se cancela durante la limpieza. La prueba falla con la implementación anterior y pasa con la corrección en ambas versiones Python.

La evaluación `scripts/evaluate-verifiers.mjs` genera `evals/verifier-contract.json` sin claves ni modelos reales: programático coincide con 3/3 fixtures usando 3 llamadas totales del worker; el revisor simulado coincide con 1/3 usando 6 llamadas totales. Los tokens son sintéticos y no acreditan calidad, facturación o ahorro de un modelo remoto. No se justifica habilitar revisión de producción a partir de este reporte.

La entrega de verificadores incorporó el formato de objetivos 2; el formato 1 se lee sin devolver una reserva pendiente. El kernel de triggers descrito a continuación lo amplía a formato 3. Las sesiones vinculadas al objetivo usan formato 3 para que lectores que desconocen el presupuesto raíz fallen antes de ejecutar. Las sesiones no vinculadas mantienen la compatibilidad anterior. Ver `docs/verification.md`.

## Kernel Rust de triggers — validación inicial

- Rust: 163 pruebas correctas, incluidas 8 nuevas de triggers.
- Bindings recompilados con el ledger de objetivos formato 3; Clippy sin advertencias.
- Regresión existente después de recompilar: 95 npm y 96 Python 3.15 correctas. La paridad de objetivos/checkpoints/verificadores Node → Python → Node sigue pasando.
- El contrato TypeScript sigue comprobándose con el compilador estricto.

Los casos del kernel verifican fuentes inactivas por defecto, intervalo atrasado coalescido en un solo claim sin ráfaga, ancla conservada al registrar de nuevo, reloj de observación que retrocede, fecha única, deduplicación de eventos, claves con otra entrada rechazadas, límites compartidos y cero asignación nueva por ocurrencia. También desactivación sin borrar incertidumbre, IDs de ejecución antiguos, cancelación con resultado conocido tardío y reapertura durable sin repetir el evento pendiente. JSON null confirmado se distingue de una respuesta desconocida.

Esta validación inicial cubrió el kernel y su almacenamiento sin iniciar timers ni consultas LLM. La integración de host se validó posteriormente como se describe a continuación.

## Callbacks y watchers de triggers

Regresión completa: 104 pruebas npm en Node 24.15.0 y 26.13.0; 105 pruebas Python en 3.10.22 y 3.15. Los nuevos casos de triggers (9 por lenguaje) cubren fuentes deshabilitadas sin consumo, entrada aislada del callback, caché de JSON null, intervalos atrasados sin ráfagas, fecha única sin ejecución hasta consumir el iterador, espera hasta la fecha, deadline durante espera, cancelación/desactivación del watcher y liberación del store. También prueban delegación bajo el saldo raíz, rechazo de otro evento mientras hay incertidumbre, resolución con ID vigente, resultado conocido tardío y conservación del calendario al reabrir.

`tests/objective-parity.py` añade Node → Python → Node para triggers: Node confirma un evento y un calendario y termina realmente el proceso dentro de otro callback, tras escribir su efecto de prueba. Python lee el recibo confirmado y el calendario sin reiniciarlos, detecta la ocurrencia pendiente sin ejecutarla y aporta su resultado comprobado. Node lee esa reconciliación sin un nuevo intento. Contadores de modelos y tools se conservan. La prueba ya forma parte del workflow existente.

Se usaron fixtures locales, sin proveedores de pago. Los timers solo se activan al consumir `watch`; no existe un scheduler de fondo al abrir el SDK. Ver `docs/triggers.md`.

## Framing, entrada acumulada y ventana combinada

El motor y sus bindings se recompilaron. Pasan 107 pruebas npm en Node 24.15.0 y 26.13.0, y 108 Python en 3.10.22 y 3.15. Clippy, formato y TypeScript estricto pasan. La suite Rust previa de 167 pruebas y los tres casos adicionales de ventana/reapertura/transporte pasan: 170 casos distintos en total.

Los perfiles se contrastan con BPE sobre el JSON producido por los mismos adaptadores, antes y después de una tool; no con cifras inventadas de uso. Se prueban ventanas en el límite exacto y un token menos, sin reducir el cap de salida. También selección de turnos completos bajo bytes/ventana, conservación de pares tool/resultado activos y detención sin reservar otra llamada.

La carga acumulada se reconcilia con el máximo entre estimación y uso reportado, conserva ambos valores separados y no aumenta por preflight. Uso ausente o exceso real impiden efectos posteriores. Una llamada durable incierta conserva su reserva al reabrir hasta aportar su respuesta conocida. Un perfil incompatible con el adaptador nativo se rechaza sin abrir una conexión, comprobado con un listener local sin solicitudes.

`tests/session-parity.py` ahora crea en Node un presupuesto acumulado con framing y ventana, pausa después de una respuesta conocida, reanuda la tool en Python y comprueba en Node que la parada es `input_limit`: un modelo, una tool y ningún nuevo cobro por la reapertura. Se mantiene el contrato de historial y las firmas Gemini de las pruebas anteriores. Son fixtures de contrato; no prueban ahorro monetario ni calidad de un modelo remoto.

## Recuperación local y planes de contexto con resumen proporcionado

Regresión completa después de recompilar: 181 pruebas Rust, 112 npm en Node 24.15.0/26.13.0 y 113 Python en 3.10.22/3.15. Clippy con advertencias denegadas, formato, TypeScript estricto y actionlint correctos. Continúan pasando las paridades de sesiones y objetivos/checkpoints/verificadores/triggers.

Las 11 pruebas Rust nuevas verifican fuentes de turnos cerrados, hashes obsoletos, consulta léxica acotada por bytes exactos y desempate reciente, proyección sin modificar historial, fuentes recuperadas completas con llamadas/resultados, límites de datos protegidos, planes inválidos, CAS/reapertura y fencing de sesiones formato 4. Se contrasta BPE y bytes contra la solicitud real que contiene resumen y recuperación, incluidos sus IDs y fuentes. Los 5 casos por binding cubren la API pública, copia defensiva del plan, rechazo mientras está ocupado, conflicto de escritor obsoleto y ausencia de inferencia auxiliar.

Los hashes se contrastaron con SHA-256 de los datos en Node y Python. Esto detectó una diferencia de orden de claves entre structs Rust y el puente JSON; el hash usa ahora claves ordenadas por mensaje y normalización de integrales portables. La prueba `tests/memory-parity.py` detectó después un falso SESSION_CONFLICT al reabrir en Node argumentos float `1.0`/`-0.0` escritos por Python. La comparación de snapshots admite esa equivalencia exacta y mantiene el conflicto por valores distintos o pérdida de precisión en enteros grandes. Hay regresión Rust específica para esos límites.

La nueva paridad crea un resumen y recuperación en Node, ejecuta un turno con tool en Python, instala un resumen actualizado y recupera ese turno original desde Node, sin repetir la tool. Fuentes y plan se conservan al abrir de nuevo; el historial sigue completo. Está incorporada al workflow de regresión. No se generan resúmenes con modelos ni se evalúa su calidad en esta entrega: callbacks auxiliares presupuestados y evaluación por tarea siguen en PLAN.md.

## Kernel de generación explícita de resúmenes

188 pruebas Rust correctas, incluidas 7 nuevas del ledger de resúmenes. Ambos bindings recompilados con formato raíz 4: regresión existente correcta, 112 npm en Node 24/26 y 113 Python en 3.10/3.15. Pasan las paridades de memoria y objetivos/verificadores/triggers, además de Clippy con advertencias denegadas y TypeScript estricto.

Se prueban claims sin modelos, caché, fuente incompatible y entrada alterada, claves pendientes sin replay, ID obsoleto y segunda resolución rechazada. Dos resúmenes y una llamada raíz intercaladas conservan costos separados por ID de permiso. Cuota y profundidad son globales, sin saldo nuevo por resumen. También se verifica salida pendiente, medición ausente sin reembolso de reserva, cancelación con resultado conocido tardío y recibo actualizado al liquidar uso conocido posteriormente.

La reapertura durable conserva fuente, ejecución y permisos. El documento raíz formato 4 lee versiones previas sin devolver la reserva existente; el test de migración del formato 1 comprueba el saldo pendiente tras actualizar. La validación rechaza atribuciones de permisos o contadores incompatibles. `summary_costs` muestra costo conocido, no medido y pendiente incluso antes de tener texto final. El kernel no llama modelos: la generación con callbacks y propagación del ámbito por agentes/checkpoints npm/Python siguen pendientes. Ver `docs/summary-kernel.md`.

## Callbacks de resumen y atribución durable

Pasaron 190 pruebas de la suite Rust completa y una nueva prueba de rechazos de schema bajo cuota atribuida: 191 casos distintos. La suite de resúmenes contiene 10 casos. Pasan 122 pruebas npm en Node 24.15.0/26.13.0 y 123 Python en 3.10.22/3.15. Clippy con advertencias denegadas, formato, TypeScript estricto y workflows con actionlint correctos. Ambos bindings están recompilados.

Los callbacks reciben fuente/mensajes inmutables para el ledger, devuelven texto y se consultan por clave sin replay. Se comprueban resolución con ID vigente, costo separado de dos resúmenes concurrentes y un worker ajeno, delegación anidada bajo profundidad global, cancelación/join de hijos no esperados, texto conocido tardío y cierre real del proceso antes de confirmar. Python además prueba cancelación repetida de un callback síncrono, conservando texto y ocupación del store hasta su terminación.

La concurrencia detectó que el cleanup npm podía cancelar otro trabajo que reutilizaba un hijo ya terminado. Una prueba falló con esa implementación y pasa después de usar una señal exclusiva por delegación. Se rechaza adoptar un agente ya ocupado; los callbacks Python también comprueban esa condición y conservan los hijos terminados al cancelar. Hay regresiones en ambos lenguajes para reutilización y trabajo ocupado.

Los checkpoints atribuidos usan formato 5, con clave del resumen y profundidad. Reserva raíz, permiso atribuido y claim se escriben en la misma transacción; el uso conocido se confirma junto a la respuesta. La reapertura conserva el ámbito y las tools conocidas no se repiten. Rechazos de schema consumen la misma cuota; un máximo agotado bloquea la continuación antes de otro modelo. Lectores sin atribución quedan cercados por la versión del documento.

`tests/summary-parity.py` inicia un resumen en Node, pausa su hijo durable después de una tool, reabre y reanuda en Python con cap restante 7, resuelve el resumen y lo consulta desde Node sin otro callback. El resultado conserva dos modelos, dos intentos de tools, entrada reportada 6 y salida 5. El plan se instala explícitamente después de obtener el recibo. Esta paridad y la evaluación siguiente están incorporadas a CI.

## Evaluación de contexto por tarea con fixtures

`node scripts/evaluate-context.mjs` genera `evals/context-contract.json` sin proveedores remotos. Tres tareas comprueban un hecho antiguo, uno reciente y una actualización que sustituye un dato previo. El historial completo permanece intacto en todas las estrategias. Cada tarea incluye cinco llamadas de ingestión histórica, una respuesta y las llamadas auxiliares explícitas.

| Estrategia | Hechos correctos | Modelos | Auxiliares | Entrada sintética | Salida sintética |
| --- | ---: | ---: | ---: | ---: | ---: |
| Contexto completo | 3/3 | 18 | 0 | 2736 | 36 |
| Descarte de turnos | 2/3 | 18 | 0 | 1350 | 36 |
| Resumen explícito | 3/3 | 21 | 3 | 2312 | 42 |
| Recuperación local | 3/3 | 18 | 0 | 1306 | 36 |

Los proveedores de fixture extraen hechos de forma determinista; reportan como entrada el BPE local y fijan salida a 2 por respuesta. No son tokens facturados ni calidad de un modelo remoto. El resumen añade una llamada por tarea, registrada separadamente, y no se activa de forma automática. El reporte no acredita ahorro monetario; la evaluación con modelos remotos sigue abierta en PLAN.md.

## Salida estructurada sin reparación

199 pruebas Rust correctas en la suite completa, incluidas 8 nuevas de salida estructurada. Ambos bindings recompilados; 131 pruebas npm pasan en Node 24.15.0 y 128 Python en 3.10.22. Las 9 nuevas pruebas npm y 5 Python también pasan en Node 26.13.0 y Python 3.15 respectivamente. Clippy con advertencias denegadas, formato Rust, tipos TypeScript estrictos y workflows validados con actionlint 1.7.12.

Los servidores locales de los tres proveedores comprueban los campos nativos y el schema íntegro, JSON y SSE, fragmentos de texto todavía incompletos, una respuesta válida Unicode con espacios preservados y una respuesta incompatible. Una respuesta final inválida consume exactamente una llamada y su uso reportado, conserva el texto en historial y deja reserva cero; no hay reparación. Un HTTP 400 tampoco genera fallback. Mutar la copia del request desde un proveedor personalizado no cambia la política que valida el motor. Las configuraciones malformadas fallan antes de callbacks.

Rust además comprueba restricciones conservadas sin reescritura, referencias externas rechazadas, tools intermedias, límite exacto de bytes y BPE con los cuatro framings, schema protegido dentro de generationConfig y liquidación durable bajo presupuesto raíz. Reabrir una respuesta inválida conserva estado terminal y uso conocido. Los registros previos sin configuración siguen legibles.

`tests/structured-output-parity.py` inicia el turno en Node, confirma una tool y pausa antes de la segunda llamada. Python rechaza una política cambiada y reanuda con el schema original; Node recupera el JSON final íntegro y los contadores de dos modelos, una tool, entrada 7 y salida 5, sin replay. Pasa tanto con Node 24/Python 3.10 como con Node 26/Python 3.15 y está en CI. Toda esta validación usa fixtures locales, sin llamadas ni API keys de modelos reales.

## Handles de agentes en memoria

203 pruebas Rust pasan en la suite completa; las 4 nuevas de handles se vuelven a comprobar tras endurecer la configuración para rechazar snapshots y campos desconocidos. Ambos bindings recompilados. Pasan 135 pruebas npm en Node 24.15.0 y 132 Python en 3.10.22, incluidas cuatro nuevas por lenguaje. Clippy con advertencias denegadas, formato Rust, TypeScript estricto y actionlint correctos. Las paridades de contexto, resumen atribuido y salida estructurada pasan con Node 24/Python 3.10.

El handle conserva acciones equivalentes al puente anterior sin transportar estado en los comandos. Rust comprueba doble start, commit parcial, consultas del confirmado mientras hay trabajo, contexto, parada terminal por schema inválido y rechazo de estado aportado por el anfitrión. Los bindings verifican fallo después de una tool conocida, fallo del observador terminal, cancelación y cierre de streaming. El historial se descarta correctamente, los getters son defensivos y se admite una nueva ejecución explícita sin repetir automáticamente la tool previa. El descarte del historial no deshace efectos externos.

`scripts/measure-engine-bridge.mjs` compara acciones tras cada transición y mide bytes UTF-8 de comandos/respuestas para cuatro fixtures sin proveedores remotos. Incluye commit/abort del handle; separa preparación histórica y getter explícito. Node 24 y 26 producen el mismo reporte `evals/engine-bridge.json`. Con 100 turnos de 1 KiB por mensaje y 12 tools, el intercambio baja de 19599613 a 2796947 bytes. Con historial de 8 KiB por mensaje y contexto de 64 KiB, baja de 87758436 a 664856 bytes. Las acciones de modelo conservan y cuentan su contexto seleccionado; el ahorro no corresponde a tokens ni facturación. Los tiempos son informativos y no se publican como garantía.

La medición está en CI. El camino durable conserva snapshots y operaciones síncronas del store; handles durables, I/O asíncrono y medición de bloqueo del event loop siguen pendientes en PLAN.md. No se declara terminado el requisito de producción por haber optimizado el camino en memoria.

## Handles durables y workers del ciclo

209 pruebas Rust correctas en la suite completa, incluidas 6 nuevas de handles durables. Ambos bindings recompilados. Pasan 139 pruebas npm en Node 24.15.0 y 136 Python en 3.10.22, con cuatro nuevas por binding. Clippy con advertencias denegadas, formato, TypeScript estricto y workflows validados con actionlint correctos. Las cinco paridades de sesión, objetivo, contexto, resumen y salida estructurada pasan con Node 24/Python 3.10.

Rust comprueba frames reducidos sin historial, comandos sin estado/revisión aportados, IDs de ejecución obsoletos, CAS entre handles, operación incierta y resolución sin replay, actualización de planes y cierre del store. Una prueba sostiene el lock del almacenamiento mientras otro thread comienza una transición; una consulta del historial visible termina sin esperar ese lock.

Los bindings controlan las fronteras de begin, claim y advance para cancelar con el worker pendiente. El store y el agente permanecen ocupados hasta terminar. Después de un claim cancelado no se invoca el modelo y el checkpoint mantiene la operación incierta. Después de una respuesta conocida, la cancelación conserva texto, uso liquidado y reserva cero bajo el mismo objetivo raíz; el terminal se reanuda sin otro modelo. Python recibe cancelaciones repetidas del Task y espera el worker antes de liberar guards. También se comprueba historial/contexto cacheado tras cerrar el store y rechazo de nuevas escrituras.

`scripts/measure-durable-bridge.mjs` realiza escrituras CAS reales en dos bases temporales y compara acciones, revisiones e historial con el puente anterior. `evals/durable-bridge.json` registra bytes deterministas con Node 24: 1443146 → 350461 para 20 turnos y 3 tools; 7733209 → 419262 para 60 turnos y 12 tools con contexto de 16 KiB. Incluye begin/claims/advances/commit y contexto de acciones de modelo; excluye preparación histórica y getter explícito. Los timers avanzaron durante el trabajo combinado; sus conteos y tiempos son informativos y quedan fuera del reporte. No acredita una cota de bloqueo, rendimiento de toda la aplicación ni ahorro facturado.

La medición durable está en CI. Los checkpoints del ciclo ya no cruzan el puente como snapshots completos; HiveDB sigue serializándolos dentro de Rust. Apertura, inspect/recover/resolve, Objective y cambios de plan siguen siendo APIs síncronas. Migrarlas y medir latencia bajo carga/concurrencia siguen pendientes en PLAN.md; no se marca terminado el requisito de producción.

## Alternativas públicas asíncronas

209 pruebas Rust correctas, bindings recompilados y Clippy sin advertencias. Pasan 150 pruebas npm en Node 24.15.0 y 148 Python en 3.10.22, incluidas 11 y 12 nuevas respectivamente. TypeScript estricto, formato y workflows con actionlint correctos. Las APIs síncronas existentes conservan su regresión.

Las pruebas cubren factories listas al devolver, ámbitos por tenant, inspección, resolución de modelos/tools con execution ID y CAS, recuperación sin replay, cambios de plan y reconciliación/cancelación de Objective durable o local. Tripwires npm sustituyen las entradas nativas síncronas y comprueban que las alternativas asíncronas no las invocan. Python verifica que la apertura y las llamadas nativas se ejecutan fuera del hilo del event loop.

Gates de workers comprueban exclusión durante mantenimiento/cierre, retención de lecturas y cargas, ocupación del agente durante cambio de plan y liberación solo después de terminar. La prueba de rechazar un turno durante mantenimiento detectó un contador negativo: el cleanup liberaba el store cuando retain había fallado. Ambos wrappers ahora marcan la adquisición solo después de retain exitoso, y la regresión comprueba cierre posterior. El setter síncrono de planes respeta también el guard de mantenimiento.

Python añade cancelaciones repetidas durante inspección, carga de agente, cambio de plan, cierre y cancelación de objetivo. Espera el worker; un plan conocido se refleja antes de liberar y una cancelación conocida señala los callbacks aunque su waiter haya cancelado. Un store creado tarde tras cancelar se cierra en otro worker antes de informar cancelación; se comprueba reapertura del mismo directorio.

`tests/async-storage-parity.py` abre store/objetivo/agente en Node, instala un plan asíncrono y deja una verificación pendiente. Python resuelve con el execution ID, usa el plan y ejecuta un turno. Node vuelve a consultar texto, plan, veredicto y uso: dos modelos, entrada 7 y salida 5, sin otra llamada. Pasa con Node 24/Python 3.10 y está en CI.

La entrega incorpora alternativas públicas. Los checks/inspecciones y claims/finalizaciones internos de Objective/Trigger todavía usan rutas síncronas; migrarlas y medir latencia de la aplicación bajo carga siguen pendientes. Estas pruebas usan fixtures locales y no acreditan ahorro de tokens/facturación ni una cota de bloqueo del event loop.

## Control interno asíncrono de objetivos y triggers

Pasaron las 209 pruebas Rust, 155 npm en Node 24.15.0 y 153 Python en 3.10.22. Cinco nuevas pruebas por binding incluyen escenarios parametrizados de claims y finalizaciones de verificación, resumen y trigger. Los 16 casos de almacenamiento/control asíncronos pasan también en Node 26.13.0 y los 17 equivalentes en Python 3.15. Clippy, formato, TypeScript estricto y actionlint correctos. Las seis paridades entre Node 24 y Python 3.10 pasan.

Un flujo completo de delegación, verificación, resumen, eventos y watcher falla si intenta usar el despachador síncrono de Objective o del store. Python contrasta además el thread de I/O con el del event loop. Ese flujo requiere exactamente seis modelos bajo el objetivo compartido; no aparecen inferencias de revisión/reparación adicionales. La delegación comprueba cuota/profundidad en el hijo antes de efectos y conserva su validación inmediata de tipo/ocupación/scope.

Los claims pendientes retienen ámbito/store antes del primer await; cerrar o resolver mientras tanto falla. Cancelarlos no invoca el callback, conserva una entrada incierta y no permite replay. Se comprueba ID obsoleto y resolución correcta seguida de consulta cacheada sin efectos. Las finalizaciones conocidas se guardan incluso si se cancela la espera; un resultado confirmado sigue consultable con la raíz detenida, sin gastar otra tool. Python recibe cancelaciones repetidas y espera el worker/cleanup antes de soltar guards.

Una prueba conserva una inspección antigua enabled=true mientras disable termina, antes de registrar la espera del watcher. El watcher vuelve a comprobar tras registrarla y termina; no pierde el wake. Python cancela además el waiter de disable después de escribir: el estado conocido despierta el watcher y los scopes quedan liberados. No se usan thresholds de rendimiento para afirmar esta propiedad; el timeout del test solo detecta una espera perdida.

`tests/async-storage-parity.py` ahora registra además un trigger incierto desde Node, lo abre y resuelve asíncronamente en Python y consulta su recibo desde Node sin repetir el callback. Conserva verificación/plan/uso previos, con dos intentos de tools raíz, dos modelos, entrada 7 y salida 5. Registro y controles de Trigger tienen factory/variantes asíncronas; no cambian formatos.

`scripts/measure-objective-runtime.mjs` y `.py` registran latencia observada con HiveDB temporal, cuatro ramas concurrentes por agente y contexto de 16 KiB. Con un agente requieren warmup 4 modelos y trabajo 8 modelos/9 tools; con cuatro, warmup 16 y trabajo 32 modelos/36 tools. Incluyen verificaciones, eventos y resúmenes programáticos sin inferencias auxiliares y exigen reservas vacías. Los tokens son sintéticos de fixture.

En la ejecución local con cuatro agentes, el exceso del timer de 10 ms tuvo p95/máximo ≈0,59/1,05 ms en Node 24 y ≈1,25/2,15 ms en Python 3.10. Los reportes `evals/objective-runtime-node.json` y `evals/objective-runtime-python.json` incluyen muestras, duración, warmup y costos. Son observaciones variables de scripts ejecutados en paralelo, sin baseline anterior ni comparación justa entre runtimes; no son cotas de producción ni ahorro facturado. CI ejecuta el workload y sus aserciones de presupuesto, sin umbral de latencia.

El primer fixture intentó emitir eventos concurrentes en un mismo trigger y recibió el rechazo esperado por una ocurrencia pendiente. El fixture final usa triggers independientes y espera todas sus tareas antes de cerrar, también en error. Se conserva la regla que evita repetir trabajo ambiguo. El requisito de handles/rutas durables asíncronas y medición local queda cubierto en PLAN.md; distribución multiplataforma y evaluación con modelos remotos siguen abiertas.

## MSRV y evidencia de avisos para distribución

Instalado y seleccionado explícitamente para la comprobación Rust 1.95.0 (59807616e, 2026-04-14), sin cambiar el toolchain predeterminado. Check del workspace con all-targets, lock y offline correcto en Linux x64. Con el mismo compilador pasan las 6 pruebas de handles durables y las 8 de salida estructurada: 14 casos existentes, no un incremento del total Rust. CI tiene un job MSRV preparado con esos comandos. Otras plataformas siguen sin certificarse.

El inventario recoge 332 dependencias de todos los targets/build/dev, conserva expresiones declaradas y deduplica 224 documentos. Los archivos .crate se contrastan con los checksums de Cargo.lock antes de leer avisos; los suplementos se ligan a versión, checksum, commit y SHA-256 del texto. Incluye avisos anidados y AUTHORS. No se eligen ramas SPDX ni se infiere qué código está enlazado en un binario concreto.

Pasan 7 pruebas Python 3.10 del generador: deduplicación, ausencia de rutas locales, gaps explícitos, rechazo de archivo .crate modificado/lock obsoleto, VCS/texto incorrecto, evidencia de AUTHORS ligada al archivo, path traversal y drift de salidas. El check real pasa offline en Python 3.10. `--require-complete` devuelve el código 2 esperado por cinco revisiones pendientes, detalladas en third_party/README.md; no se declara completa la revisión.

Pasan 3 pruebas nuevas del empaquetador nativo en Node 24 y las 2 existentes del selector/plataformas. Se comprueban avisos/procedencia en previews, rechazo de paquetes públicos con gaps y rechazo de Cargo.lock/avisos modificados. Los tarballs reales del facade y nativo debug incluyen agregado y catálogo; el facade no incluye el binding de desarrollo. Un consumidor npm temporal pasa en Node 24.15.0 con PATH sin Rust/Bun.

Maturin 1.16.0 construye el wheel local cp310-abi3-linux_x86_64. Se verifica LICENSE, THIRD_PARTY_NOTICES.txt e inventario bajo dist-info/licenses, los bytes del agregado y el módulo async `_storage.py`. La instalación aislada y smoke test en Python 3.10 pasan con PATH sin Rust/Bun. Es un build debug local, sin certificación manylinux y sin reemplazar los artefactos portables anteriores. No se publicaron paquetes ni se ejecutó la matriz remota. Actionlint correcto para workflows de regresión y nativos.

## Procedencia de código vendorizado de tiktoken

El catálogo pasa a 225 documentos únicos y conserva 332 dependencias/cinco paquetes pendientes. Incorpora el MIT de OpenAI del commit 4e71bbe0c078468e00fefbf94b39849389f346e5. La cadena offline comprueba árbol no truncado ligado al commit VCS del wrapper, gitlink, blob de .gitmodules/URL del hijo y blob del código vendorizado leído del .crate verificado. Los suplementos normales mantienen su regla de commit exacto. El pendiente de tiktoken-rs se acota a atribución de assets; no se cerró la revisión ni se cambió el gate público.

Pasan 11 pruebas Python 3.10 del generador, incluidas cuatro nuevas para procedencia de submódulos: conservación de ambos commits, rechazo de padre/hijo/repositorio/fuente incorrectos, evidencia alterada o truncada y aviso hijo que no sustituye aviso del wrapper. Pasan las tres pruebas del empaquetador con Node 24.15. Check offline real correcto; require-complete devuelve el código 2 esperado y enumera cinco revisiones.

Se generaron tarball npm del facade y wheel del proyecto Python bajo /tmp/hive-sdk-submodule-packages y /tmp/hive-sdk-submodule-wheels. Se contrastaron los bytes de avisos/catálogo empaquetados contra los del workspace y se comprobó inclusión del documento OpenAI y estado incomplete. El wheel es debug local cp310-abi3-manylinux_2_39_x86_64; no sustituye la validación portable manylinux 2.28 ni la matriz. No se repitió la regresión completa del motor: no cambió el runtime. No se publicaron paquetes.

## SDK independiente desde cero — 2026-11-10

El alcance usa APIs propias y bases nuevas, sin importación o compatibilidad con datos del SDK anterior. Se retiraron extractor/preparador, importación de historial y recibos, sus comandos del motor, métodos npm/Python, tipos, pruebas, reportes y pasos de CI específicos. Se actualizaron README, plan, especificación y pendientes de publicación. El motor, persistencia propia y control de presupuestos permanecen disponibles.

Bindings debug reconstruidos desde el código actual. La regresión completa pasa: 209 pruebas Rust, 158 npm en Node 24.15 y 153 Python en 3.10.22. Formato, Clippy workspace/all-targets con -D warnings, TypeScript estricto, actionlint y check offline de avisos correctos. El catálogo conserva 332 dependencias/225 documentos/cinco revisiones pendientes; este cambio de alcance no habilita publicación ni certifica la matriz de plataformas.


## Previews GNU x64 actuales — 2026-11-10

Reconstruidos ambos bindings release en la imagen manylinux_2_28_x86_64 con digest 9555ba6c479fa69a2aa80ad1dc57154a60d562b19df159361d9379a02beb1e36, Rust 1.99.1 y Maturin 1.16.0. Se reutilizó el builder/caché anterior, introduciendo una instantánea de las fuentes actuales; se comprobaron SHA-256 de sus 387 archivos dentro del contenedor y ausencia de fuentes Rust adicionales. El build terminó correctamente. No se cambiaron las bibliotecas debug de desarrollo.

Los tres artefactos de dist/npm y dist/wheels reemplazan sus previews anteriores. dist/build-info.json y evals/distribution-linux-x64-gnu.json fijan hashes/tamaños, manifiesto de fuentes e identidad del builder. Los tarballs conservan private:true, versión coordinada, avisos/inventario exactos y hash del binding release; el facade excluye el binding de desarrollo. El wheel cp310-abi3-manylinux_2_28_x86_64 conserva los tres archivos de licencia bajo dist-info/licenses, módulos Python actuales sin sobrantes y un único binding del paquete. Ambos bindings requieren símbolos GLIBC como máximo 2.28.

Pasan siete instalaciones: npm Node 24.15.0 y 26.13.0 locales; wheel Python 3.10.22 y 3.15.0 locales; npm Node 24.22.2 en un contenedor Debian bookworm nuevo; wheel Python 3.10.22 y 3.15.2 en un contenedor manylinux nuevo. npm instala offline sin scripts y el wheel sin índices/dependencias. Los PATH de consumidor solo contienen la instalación Node o el bin del venv; Python usa -I y elimina PYTHONPATH.

Los smoke tests ahora ejercitan factories asíncronas de SessionStore/Agent/Objective, tool de suma, reapertura del historial y contabilidad durable (2 modelos, 1 tool, entrada 2, salida 2), además de objetivo local. Son callbacks/uso sintéticos, sin modelos de pago. Los helpers y fixtures están ligados por hash en el reporte. Estas son pruebas funcionales de paquetes instalados, no una repetición de la regresión completa contra los binarios portables.

GNU x64 queda actualizado con evidencia local. Las otras cinco plataformas, matriz remota, publicación/instalación desde registros y cinco revisiones de avisos siguen pendientes. No se publicaron paquetes; la revisión incompleta de avisos sigue visible y no se retiró private:true.

## Previews musl x64 — 2026-11-10

Construidos desde la misma instantánea de 387 fuentes que GNU, verificadas dentro de un contenedor musllinux_1_2_x86_64 con digest cf82db9d8dc8c5c2cb27f422208baf266ebc8ad70be4ab1bc79bcc8807a6432d. Builder Alpine 3.22.8, Rust 1.99.1 nativo x86_64-unknown-linux-musl, Maturin 1.16.0, RUSTFLAGS=-C target-feature=-crt-static y dos trabajos Cargo. Se usó almacenamiento temporal aislado, sin modificar los contenedores existentes del usuario ni sus fuentes anteriores. La compilación fría de ambos bindings terminó correctamente.

El tarball linux-x64-musl conserva private:true, arquitectura/libc correctos, avisos Cargo e inventario exactos y hash release; el facade compartido coincide con el de GNU. El wheel cp310-abi3-musllinux_1_2_x86_64 contiene los módulos Python actuales y los tres archivos de licencia/inventario esperados. Se verificaron ELF x64, libc.musl-x86_64.so.1 y ausencia de versiones de símbolos GLIBC en los bindings y la biblioteca incluida.

Pasan cuatro instalaciones limpias: npm Node 24.22.2 y 26.13.0 sobre Alpine 3.24.4; wheel Python 3.10.22 y 3.15.2 sobre un contenedor musllinux nuevo. npm instala offline sin scripts; Python sin índices/dependencias, -I y sin PYTHONPATH. Los PATH de consumidores ocultan Rust/Bun. Los fixtures ejercitan factories asíncronas, tools, reapertura HiveDB, objetivo durable con 2 modelos/1 tool/entrada 2/salida 2 y objetivo local. No hay modelos remotos ni ahorro facturado; son pruebas de instalación/flujo funcional y no la regresión completa portable.

Maturin agregó libgcc_s.so.1 al wheel. Se identificó el origen libgcc-14.2.0-r6 y su hash 0cd532bd6739a5419c5a14e8897ad4e9de0df77c9f4e8e6ba202e369a8c8b4e5; el ELF reparado del wheel tiene hash 42b5f2c8ca13e5e173d837f5117a04b6c042437548112bde7879251ae8dcff8f. La revisión pendiente por archivo/avisos está en third_party/runtime-review.json y se suma a las cinco revisiones Cargo. No se interpretó la expresión del paquete Alpine como autorización suficiente ni se habilitó publicación.

Artefactos en dist/npm y dist/wheels, separados de GNU; evidencia con hashes en dist/build-info-musl.json y evals/distribution-linux-x64-musl.json. Restan cuatro plataformas, matriz remota, atribuciones y publicación/instalación desde registros. No se publicaron paquetes.

## Runbook del proyecto local — 2026-11-10

Documentados avance, límites de la evidencia y etapas del piloto en docs/progress-2026-11-10.md y docs/local-project-validation.md, con ejemplo examples/local-project-validation.mjs. Se ejecutaron literalmente los dos primeros bloques bash del runbook bajo Node 24.15.0: crean consumidor npm fuera de Hive, instalan offline facade/binding GNU y ejecutan modo offline. La prueba pasa contra @johpaz/hive-agents 1.1.4 leyendo solo name/version del manifiesto: completed, dos callbacks de modelo, una tool y reapertura de Agent sin otra inferencia. El hash del binding instalado coincide con el manifest del artefacto release.

La evidencia evals/local-project-validation.json fija versión/binario, hash del ejemplo y tarballs, políticas y resultado. Los tokens 2/2 son sintéticos; no hubo llamadas pagadas, apertura de bases anteriores, cambios de UI ni adaptación efectiva del backend. La reapertura es en el mismo proceso/store. El modo live y el puente del backend permanecen pendientes de ejecución/implementación. Se comprobaron sintaxis del ejemplo y enlaces locales. No cambió el runtime ni se repitió su regresión completa por esta actualización de documentación/ejemplo.
