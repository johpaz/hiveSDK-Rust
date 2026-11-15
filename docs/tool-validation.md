# Validación de tools

La misma implementación Rust valida argumentos en Rust, npm y Python. No se carga un validador distinto por lenguaje y no se hace una llamada LLM para validar.

## Schemas al registrar

`parameters` debe declarar un objeto JSON Schema (`type: "object"`). El schema se compila al construir el agente; `INVALID_TOOL_SCHEMA` impide usar una definición inválida. Se usa [jsonschema 0.58.6](https://docs.rs/jsonschema/0.58.6/jsonschema/), con [Draft 2020-12](https://json-schema.org/draft/2020-12/) por defecto y el draft reconocido que indique `$schema` cuando se declare.

Se aplican tipos, required, límites numéricos y de texto, arrays, combinadores, condicionales, referencias incluidas y las demás restricciones soportadas por ese draft y el validador. Los formatos conocidos se comprueban. Formatos/keywords personalizados no reconocidos permanecen como anotaciones; no se ofrece todavía un registro público de validadores personalizados.

Referencias a `$defs`, `definitions`, anchors o recursos incluidos en el mismo schema pueden resolverse. Una referencia que requiera descargar una URL o leer un archivo externo se rechaza. Incluir las definiciones necesarias al publicar la tool.

No se insertan valores default, convierten strings a números ni modifican argumentos. Las reglas de negocio y autorización siguen siendo responsabilidad del callback.

## Feedback al modelo

Si los argumentos fallan, el callback no corre. La acción observable es `tool_rejected`; su resultado se incorpora como mensaje de tool para que el modelo pueda corregir la llamada dentro del turno normal:

```json
{
  "error": {
    "code": "INVALID_TOOL_ARGUMENTS",
    "tool": "guardar",
    "message": "Arguments do not match the tool JSON Schema",
    "issues": [{
      "instance_path": "/edad",
      "schema_path": "/properties/edad/type",
      "message": "value is not of type integer"
    }],
    "truncated": false
  }
}
```

El texto concreto depende del validador. Usar `code` y las rutas para procesar errores programáticamente. Se incluyen hasta ocho issues, con mensajes de hasta 512 caracteres, y se marca `truncated` si se omiten issues. Los mensajes enmascaran valores de instancia; los argumentos originales y las rutas aún pueden aparecer en el historial/ledger, por lo que no son logs anonimizados.

Solo se validan argumentos JSON parseados: JSON mal formado, tools desconocidas o IDs de llamada duplicados/vacíos siguen siendo errores de protocolo, sin reintento automático.

## Control de iteraciones

- `tool_calls`: callbacks ejecutados, incluidos los que devolvieron un error.
- `tool_rejections`: llamadas rechazadas por schema, sin callback.
- `max_tool_calls`: máximo de la suma anterior por turno; se conserva al reanudar.

El feedback consume una continuación normal al modelo, contabilizada en sus llamadas/tokens. No hay un loop de reparación separado ni revisión adicional por defecto. Dos rechazos idénticos detienen el turno con la política inicial. El opt-out individual de polling solo permite resultados ejecutados repetidos; no desactiva el detector para argumentos inválidos. El opt-out global continúa disponible y los demás límites siguen aplicándose.

## Persistencia

Una llamada rechazada no se marca como operación externa en curso. Feedback, contador, entrada `rejected` del ledger y próximo checkpoint se guardan en una misma transacción. Al reanudar se conserva su costo y el detector de repetición. También se revalida una tool pendiente programada por una versión anterior antes de permitir el despacho.

Los schemas compilados se reutilizan mediante una caché de hasta 64 entradas por proceso; no se serializan en HiveDB. La validación y compilación son síncronas en esta entrega. Deadlines de operaciones, cuotas de tamaño/complejidad de schemas y aislamiento para schemas no confiables son trabajo pendiente antes de exponer registro de tools a terceros.
