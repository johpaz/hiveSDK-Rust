# Narraciones y oficina 3D sobre el gateway Rust

El cliente Rune conserva su escena wgpu, HUD, simulación de abejas y feed. El gateway publica
actividad de ejecuciones reales del SDK; HiveDB conserva agentes, configuración, narraciones y estado.

- `process` identifica agente y turno raíz. `turnDone:false` permite completar un paso o un hijo sin
  cerrar el turno del coordinador. La respuesta final confirmada reemplaza sus fragmentos anteriores.
- `GET /api/agents` presenta el contrato de Rune, incluidos los especialistas globales del catálogo.
  `GET /api/office` y `canvas_subscribe` proporcionan el snapshot de la oficina.
- Una revisión persistente ordena los snapshots. La oficina agrega ejecuciones concurrentes por
  agente y rechaza datos anteriores a su snapshot actual. El broadcast transporta datos de HiveDB.
- `task_delegate` ejecuta un especialista habilitado, con proveedor propio o heredado y sesión
  durable independiente. El padre recibe su resultado; los fragmentos del hijo no se mezclan con
  la respuesta final. El objetivo compartido del turno aplica límites y el stop se propaga al hijo.
- La delegación inicial es síncrona y de texto, con profundidad uno; herramientas externas,
  verificaciones automáticas y ejecución asíncrona en segundo plano quedan pendientes.
- La oficina añade un compositor y la actividad desplegable para consultar sin abandonar la escena.
  Al reiniciar o perder la conexión se muestra la incertidumbre, sin reproducir inferencias.

Pruebas deterministas con el motor real y proveedor de fixture verifican las transiciones,
delegación, persistencia, cancelación, aislamiento de propietarios y concurrencia. Rune comprueba
el protocolo, el estado derivado de la escena y la presencia del compositor sin necesitar una GPU.
