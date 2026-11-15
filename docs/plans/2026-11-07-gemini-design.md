# Diseño: Gemini con firmas y checkpoints

Continuación del plan autorizado para proveedores nativos. Alternativas: usar compatibilidad OpenAI, rechazar todas las firmas o implementar GenerateContent y estado opaco tipado. Se elige la última: evita perder las partes necesarias para continuar tools con modelos que usan firmas.

El transporte compartido añade rutas por modelo, key en header y JSON/SSE propios. SSE se confirma con STOP y fin HTTP limpio; no requiere DONE y no ejecuta tools al recibir partes intermedias. Se valida también cierre del framing final para no aceptar un cuerpo truncado después de STOP.

ProviderState es opcional y versionado, inicialmente Gemini v1 con parts originales. ModelReply lo entrega al motor; se valida contra contenido/calls antes de mutar y se guarda en Message. Request vuelve a usar partes originales, no un resumen reconstruido. Texto thought no es contenido visible. Firmas en texto vacío o partes sin texto se conservan. Otros protocolos nativos rechazan ese estado antes de conectar.

Los IDs remotos se conservan; si faltan, se generan IDs neutrales por posición sin incorporarlos como IDs remotos al responder. Los resultados conservan orden, nombre y JSON. Uso suma prompt+tool prompt y candidatos+thoughts; caché no se añade dos veces al prompt. Presupuestos y reservas del motor permanecen comunes.

No se activan herramientas del servidor, grounding, multimodal, argumentos parciales o continuaciones. Las pruebas cubren wire, tipos, firmas, contador, EOF, cancelación/deadline, restauración durable, cambio de protocolo, contexto acotado y Node→Python→Node. Sin llamadas a modelos de pago.
