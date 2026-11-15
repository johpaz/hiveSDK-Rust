# Webchat: conversaciones y narraciones

Referencia: `gateway/routes/conversations.ts`, `gateway/routes/chat.ts`,
`hive-ui/stores/conversationsStore.ts`, `ConversationList.tsx` y `useChatStreaming.ts`
del Hive original. Se lista por actividad reciente, se abre el último chat al iniciar,
se crea uno únicamente si no existe ninguno o al pulsar Nueva conversación, y se recuperan
los últimos 40 mensajes visibles. Los identificadores son opacos y se codifican en query.

HiveDB conserva hilos, mensajes y narraciones. Se validan propietario, canal y archivado.
Los hilos históricos sin sesión SDK reciben contexto textual inicial; las sesiones SDK
existentes mantienen sus checkpoints. Una operación incierta o una configuración incompatible
permite consultar el historial, pero exige revisión o un chat nuevo para continuar.

La UI carga por generación, impide enviar durante una carga y filtra eventos WS por hilo
al consumirlos. Cambiar de conversación requiere terminar o detener el turno actual. No se
reinician ni reproducen tareas pendientes. Los estados de oficina siguen siendo globales.

Las narraciones usan pasos estables por actividad/delegación, estados y horas; se agrupan
por turno, se restauran del historial y conservan completos los errores al expandir.
Consultar a un especialista y recibir su texto no implica verificar efectos externos.

## Validación realizada

- 32 pruebas del núcleo: historial inicial de texto, rechazo de mensajes de sistema/tools,
  sesiones, handles y checkpoints durables; sin reejecución de operaciones inciertas.
- 26 pruebas del gateway: lista ordenada, propietario/canal/archivado, continuidad textual
  de un hilo histórico, aislamiento entre chats y restauración de pasos terminados.
- 10 pruebas Rune seleccionadas: generaciones de cargas, eventos atrasados por hilo,
  lista en ventanas anchas/estrechas y regresiones de narraciones, oficina y dashboard.
- Clippy del núcleo y gateway con todos sus targets: sin advertencias. La UI conserva
  advertencias previas de Clippy documentadas en la integración del dashboard.
- Ambos binarios debug recompilados; configuración del perfil de prueba validada sin
  llamar a modelos. Las pruebas que requieren TCP local permanecen fuera de este sandbox.

Los eventos nuevos guardan una revisión monotónica para ordenar etapas emitidas en el
mismo milisegundo. Los errores de ejecución y sus estados terminales llevan `threadId`;
las notificaciones y los snapshots globales de oficina siguen siendo independientes del chat.
