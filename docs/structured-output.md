# Salida estructurada explícita

`policy.structured_output` configura un contrato JSON para la respuesta final. El motor Rust valida el schema al construir el agente y valida la respuesta terminada antes de declararla completa. No llama otro modelo para corregir JSON ni retira bloques Markdown. Sin esta opción, el comportamiento anterior permanece disponible.

```js
const agent = new Agent({
  model: 'your-supported-model', provider,
  policy: {
    max_output_tokens: 256,
    structured_output: {
      name: 'answer_v1',
      schema: {
        type: 'object',
        properties: { answer: { type: 'string' } },
        required: ['answer'], additionalProperties: false,
      },
    },
  },
});
const result = await agent.run('Responde con el dato solicitado');
if (result.status === 'completed') console.log(JSON.parse(result.content));
```

```python
agent = Agent(model='your-supported-model', provider=provider, policy={
    'max_output_tokens': 256,
    'structured_output': {
        'name': 'answer_v1',
        'schema': {
            'type': 'object',
            'properties': {'answer': {'type': 'string'}},
            'required': ['answer'], 'additionalProperties': False,
        },
    },
})
result = await agent.run('Responde con el dato solicitado')
if result['status'] == 'completed':
    print(json.loads(result['content']))
```

Los ejemplos presuponen los imports habituales y un proveedor configurado; Python necesita `import json`. `name` acepta de 1 a 64 caracteres ASCII: letras, dígitos, `_` o `-`. `schema` debe ser un objeto JSON de hasta 64 KiB serializados. Comparte la validación offline de schemas de tools; no descarga referencias externas. Los proveedores personalizados reciben `request.structured_output` y están sujetos a la misma validación final.

La salida continúa siendo texto en `result.content`, preservando espacios, Unicode y la representación original de números. La aplicación elige cómo parsearlo y representar enteros grandes. No se añade un campo con datos convertidos automáticamente. Una respuesta final inválida produce `status: 'stopped'`, `stop_reason: 'invalid_structured_output'` y contenido de resultado null; el texto recibido queda en `agent.messages`. El uso conocido se liquida y el checkpoint queda terminal, sin una segunda solicitud de reparación. Otros límites pueden detener el turno antes de validar la salida.

Las respuestas intermedias que solicitan tools pueden incluir texto ordinario; el contrato se aplica a la respuesta sin tools. Los argumentos de tools siguen su propio schema. En streaming los `text_delta` son parciales y todavía no están validados: esperar el evento `result` con estado completed antes de consumirlos como datos confirmados. Truncamiento, rechazo o protocolo incompleto conservan el tratamiento existente del adaptador.

## Adaptadores y compatibilidad

| Adaptador | Campo enviado |
| --- | --- |
| OpenAICompatible, Chat Completions | `response_format: {type: 'json_schema', json_schema: {name, schema, strict: true}}` |
| Anthropic Messages | `output_config.format: {type: 'json_schema', schema}` |
| Gemini GenerateContent | `generationConfig.responseMimeType: 'application/json'` y `responseJsonSchema: schema` |

Los campos corresponden a las APIs [Chat Completions](https://developers.openai.com/api/docs/guides/structured-outputs?api-mode=chat), [Messages](https://platform.claude.com/docs/en/build-with-claude/structured-outputs) y [GenerateContent](https://ai.google.dev/api/generate-content?hl=en), cuya [metadata pública v1beta](https://generativelanguage.googleapis.com/$discovery/rest?version=v1beta) describe GenerationConfig. No se traduce esta opción a Responses ni Interactions.

La compatibilidad depende del modelo, endpoint y subconjunto de JSON Schema del proveedor. El SDK envía el schema original: no elimina restricciones para forzar su aceptación, no elige otro modelo ni repite una solicitud rechazada. Una validación local correcta no garantiza que el proveedor admita ese schema o su combinación con tools. Si el proveedor omite una restricción admitida localmente, la validación final sigue aplicándola.

## Presupuestos y persistencia

El descriptor completo cuenta en bytes y BPE neutrales. Los perfiles de cada adaptador cuentan su schema en el payload real; Gemini conserva MIME/schema en el conteo de generationConfig y excluye solo los parámetros de salida. El schema no se descarta para hacer caber el contexto. Estos conteos locales no incluyen instrucciones internas que pueda añadir un proveedor ni equivalen a facturación.

La política se guarda con la sesión y debe coincidir al reabrir. Cambiar nombre o schema exige una sesión distinta; no se modifica un turno pendiente. Los registros previos sin la opción siguen legibles. Lectores anteriores con políticas estrictas rechazan la nueva opción configurada, evitando continuar sin validación.

Las pruebas locales cubren JSON/SSE en ambos bindings, salida inválida sin reparación, payload sin reescritura, bytes/BPE, liquidación durable y reanudación Node → Python → Node después de una tool conocida. Son fixtures de protocolo; no certifican compatibilidad ni calidad de un modelo remoto.
