# Agent loop: terminar con evidencia y medir el costo

## Fuente y alcance

[Loop Engineering: Building Blocks, Adoption, and Impact, v3](https://arxiv.org/html/2608.21884v3), revisado el 22 de octubre de 2026. El artículo distingue el ciclo de una sesión de las estructuras que repiten sesiones. Resume presupuestos, estado durable, verificación y criterios de parada; no presenta evidencia experimental de que esas prácticas reduzcan costos o aumenten calidad. El experimento comparativo descrito es trabajo propuesto.

## Decisiones propias de Hive

Aplicar controles deterministas dentro del motor, sin agregar un LLM supervisor por defecto. La política de Hive es una decisión de diseño inspirada en ese análisis; no es un algoritmo validado por el paper.

Implementado en este corte:

1. Terminar al recibir contenido final sin llamadas a tools; no pedir confirmación automática a otro modelo.
2. Comprobar límites antes de despachar más trabajo, incluidos lotes de tools.
3. Detener repeticiones exactas de observaciones, con opt-out para polling deliberado.
4. Contabilizar uso reportado y señalar uso ausente. No presentar el umbral posterior a una respuesta como techo de facturación.
5. Distinguir parada de respuesta completa y exponer llamadas/tokens para evaluación.
6. No reintentar proveedores o efectos externos automáticamente.
7. Guardar conversación, checkpoints, ledger y parada de política en HiveDB. La reanudación explícita conserva presupuesto y observaciones; una operación incierta exige reconciliación, no una nueva llamada automática.
8. Reservar salida antes de despachar y mantener un deadline absoluto por turno. Los resultados conocidos liquidan la reserva; una operación incierta no la devuelve automáticamente ni obtiene más tiempo al reabrir.
9. Acotar entrada en bytes JSON y BPE local explícito; detener o retirar turnos antiguos completos sin llamadas de resumen, conservando historial y turno activo. Hay perfiles JSON por adaptador, saldo acumulado de entrada y ventana estimada de entrada + cap de salida; no equivalen a un tokenizer privado ni a un techo facturado.

Controles durables ya disponibles:

- [Objective](objectives.md) comparte presupuesto raíz y reservas entre padre/hijos y operaciones auxiliares explícitas; los claims/resultados durables se coordinan con checkpoints.
- [Verificadores opt-in](verification.md), [resúmenes explícitos](summary.md) y [triggers](triggers.md) con ledger/reconciliación y sin activar inferencias automáticamente al esperar o terminar.
- Perfiles de entrada, saldo acumulado, reserva de salida y deadline global, con transporte cancelable y callbacks cooperativos.

Pendiente en la integración real:

- Conectar el ID durable de operación a idempotencia en los sistemas externos y reconciliar efectos inciertos. El SDK registra el resultado pero no convierte por sí solo una API externa en exactamente una ejecución.
- Definir verificaciones de negocio comprobables y medir calidad/costo con modelos reales. Una respuesta final no demuestra que la tarea externa esté resuelta.
- Validar los callbacks y su cancelación bajo el flujo local, conservando el presupuesto raíz y sin reiniciar automáticamente ejecuciones detenidas. Ver [el runbook local](local-project-validation.md).

## Cómo decidir si una mejora merece más llamadas

Comparar un baseline sin llamadas auxiliares con cada política candidata sobre las mismas tareas, entradas y versiones de modelos. Medir éxito comprobable, llamadas, tokens, latencia y paradas erróneas; incluir tareas directas, uso de tools, errores recurrentes, polling y dependencias cambiantes.

Los tests deterministas de esta entrega prueban límites y rutas de control. No miden calidad de modelos ni demuestran ahorro en producción. Ningún porcentaje de ahorro se atribuye al paper o a estas pruebas.
