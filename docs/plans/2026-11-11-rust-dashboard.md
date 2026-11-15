# Dashboard nativo Rust

Se habilita el Centro de Control existente, conservando su diseño y actualización cada
cinco segundos. La capacidad `dashboard` permite navegar sin habilitar las páginas de
ajustes todavía no migradas. Los agentes y estados proceden del mismo roster y snapshots
que la oficina; abrir el dashboard no dispara las consultas de todos los ajustes.

El gateway implementa las cinco consultas existentes: métricas de proceso, actividad,
versión local, consumo y propuestas pendientes. Actividad y consumo leen los rollups
canónicos de HiveDB. Los nuevos mensajes y respuestas confirmadas del modelo actualizan
esos rollups atómicamente, con claves de idempotencia. Los precios salen de `models`;
la ausencia de tarifa o medición se presenta como dato incompleto. El consumo mostrado
no certifica facturación de solicitudes interrumpidas sin respuesta confirmada.

Las métricas de CPU/RSS pertenecen al proceso Rust; no se inventa un heap JavaScript.
La versión es la del binario local y no implica una consulta de actualizaciones remotas.
Las propuestas solo se leen: no se ejecuta el curador ni se deshabilitan agentes.

Se validan agregaciones, idempotencia, autenticación, ventanas temporales y actividad
mediante pruebas sin sockets; la vista y navegación se verifican en Rune con estado local.

## Resultado de validación

- 24 pruebas del gateway pasan, incluidas autenticación, ventanas de horas, uso canónico
  con precios de HiveDB, idempotencia y recuperación de actividad a través de reinicios.
- La delegación registra uso del coordinador y del especialista bajo sus modelos reales.
- Dos pruebas Rune pasan: navegación con capacidad nativa y salud del enjambre heredada.
- Clippy del gateway, incluidos sus tests: sin advertencias. El chequeo estricto de la UI
  encuentra advertencias previas en media, A2UI, voz, reuniones, HUD, shell y fixtures.
- Tres pruebas de red del gateway y dos pruebas HTTP de dashboard requieren TCP local;
  se omiten en el sandbox que rechaza sockets. No se llama a modelos ni servicios de actualización.

La actividad previa de Rust se recupera únicamente desde sus enlaces de mensajes explícitos;
los rollups históricos de TypeScript no se vuelven a contar. No se reconstruye consumo de
modelos a partir de texto: los turnos anteriores sin registro de uso permanecen desconocidos.
