# Diseño: Messages API nativa

Continuación del plan autorizado para flexibilizar proveedores. Se consideran compatibilidad fingida con Chat Completions, transporte duplicado o adaptador de protocolo con transporte compartido. Se elige la tercera opción: conserva presupuestos, cancelación y deadlines en una sola ruta HTTP.

El transporte común recibe un protocolo explícito; las APIs Rust OpenAiCompatible y Anthropic conservan constructores propios. Los bindings usan una selección interna por kind; configuraciones históricas sin kind siguen siendo OpenAI. npm/Python comparten la mecánica de iteración/cancelación pero presentan clases públicas separadas.

Messages se traduce desde el historial neutral: system superior, bloques text/tool_use/tool_result y schemas input_schema. Se agrupan resultados de un lote y se rechazan huérfanos. Uso normalizado incluye input de caché; snapshots SSE acumulados se reconcilian únicamente al confirmar el reply.

El ensamblador exige orden de eventos y bloques, argumentos JSON completos y message_stop. Solo texto y tools locales pueden sobrevivir al historial neutral. Thinking, firmas, citas, server tools y pausas se rechazan hasta ampliar contratos. No hay fallback o continuación automática. Un resultado incierto conserva claim y reserva.

API version predeterminada 2026-10-21, configurable explícitamente; cap neutral domina el default obligatorio de max_tokens. Credentials y configuración de red no se serializan en estado. La verificación usa mocks HTTP/SSE locales: wire, headers, caché, fragmentación, herramientas, errores, cancelación, deadline, reanudación y regresión de OpenAI. Sin llamadas a modelos de pago.
