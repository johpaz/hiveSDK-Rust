# Contexto acotado sin llamadas auxiliares

La policy compartida añade `max_context_bytes` (entero positivo o null/None) y `context_overflow` (`stop` o `drop_oldest_turns`). Por defecto no hay límite y el modo es `stop`. Elegir eliminación requiere un límite explícito, de bytes o de tokens BPE locales.

```javascript
const policy = {
  max_context_bytes: 64 * 1024,
  context_overflow: 'drop_oldest_turns',
};
```

Python recibe el mismo dict. Este valor es un ejemplo de tamaño, no una equivalencia con la ventana de tokens de un modelo.

Rust cuenta bytes UTF-8 de la serialización JSON compacta de `{model, messages, tools}`, con los contratos neutrales del SDK. Incluye sistema, schemas, argumentos, resultados, comillas y escapes JSON. Excluye cap de salida, deadline y el propio reporte. Cuenta mediante un writer sin crear un buffer del payload completo.

No es conteo de tokens ni tamaño del body HTTP: cada adaptador transforma ese contrato. Tampoco garantiza una ventana de contexto del proveedor ni un techo de facturación. Un proveedor personalizado que añada mensajes por su cuenta debe controlar esa entrada adicional.

`ModelRequest.context` informa `serialized_bytes` y `dropped_messages` cuando el límite está activo. Aparece en eventos de modelo y proveedores personalizados, con respuestas completas y streaming. El HTTP nativo no lo envía al vendor.

`stop` termina con `context_limit` antes de llamar al proveedor y no reserva salida para una solicitud que no se hará. `drop_oldest_turns` retira la conversación antigua más temprana, completa, hasta que quepa. Un turno empieza con user e incluye assistant, lotes de tools y sus resultados hasta el siguiente user. Conserva sistema y todo el turno activo. No corta cadenas, schemas ni pares llamada/resultado. Si la entrada obligatoria no cabe, termina con `context_limit`.

La selección no cambia `agent.messages`, el historial guardado ni el ledger. Los mensajes omitidos siguen disponibles en HiveDB. Reanudar reconstruye la misma selección; repetir la comprobación no descuenta otra vez mensajes ni consumo.

Si un mensaje contiene provider_state (por ejemplo firmas Gemini), acompaña al turno completo y participa en los bytes/BPE del contrato neutral. No se recortan partes de ese estado ni firmas dentro del turno activo. El payload del vendor puede tener otro tamaño; el reporte mantiene su definición neutral.

Una tool puede producir un resultado que exceda el límite. Su resultado conocido y checkpoint se guardan antes de detener; no se repite ni se trunca para forzar otra inferencia. Esta política limita lo enviado al modelo, no almacenamiento o memoria total del historial.

También existe `input_token_budget`, que cuenta BPE local del JSON neutral con codificación y margen explícitos. Añade `context.token_estimate` sin modificar el significado del reporte de bytes ni del uso real. Si se combinan, la solicitud debe cumplir ambos límites. Ver [tokens](tokens.md).

No hay resumen automático por LLM ni reintento para encajar el contexto. Se puede instalar explícitamente un resumen proporcionado y recuperar turnos originales mediante fuentes verificadas y búsqueda local: ver [memoria de contexto](memory.md). La [generación explícita](summary.md) usa callbacks presupuestados y registra el costo auxiliar. Los perfiles JSON, entrada acumulada y ventana combinada están disponibles en `input_token_budget`. Omitir contexto puede perder información útil para el modelo; `stop` permite exigir otra estrategia explícita.
