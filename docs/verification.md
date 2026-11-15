# Verificadores de objetivos

`Agent.run()` termina la conversación; no acredita que una operación externa haya cumplido su objetivo. La aplicación puede comprobarlo explícitamente con `Objective.verify`. No se ejecuta al terminar un agente ni abre otro ciclo de corrección.

```javascript
const facts = { receipt: { state: 'committed', sum: 5 }, expected: 5 };
const check = await objective.verify('sum-receipt-v1', facts, value => ({
  passed: value.receipt?.state === 'committed' && value.receipt.sum === value.expected,
  reason: 'Comparación del recibo con el resultado esperado',
  evidence: value.receipt,
}));
console.log(check.verdict.passed);
```

```python
def check_receipt(value, context):
    receipt = value['receipt']
    return {'passed': receipt is not None and receipt['state'] == 'committed'
                      and receipt['sum'] == value['expected'],
            'reason': 'Comparación del recibo con el resultado esperado',
            'evidence': receipt}

check = await objective.verify('sum-receipt-v1', facts, check_receipt)
print(check['verdict']['passed'])
```

El veredicto exige `passed` booleano. `reason` es una cadena opcional y `evidence` un valor JSON opcional. No se convierten strings o números a booleanos ni se aceptan campos desconocidos. Clave: 1–256 bytes no vacíos; entrada y veredicto: hasta 64 KiB JSON UTF-8 cada uno; razón: hasta 4096 bytes. La entrada debe ser datos JSON; el callback recibe una copia de su representación JSON. No usar objetos con métodos, fechas como instancias ni valores cíclicos.

## Presupuesto, claves y recuperación

Cada comprobación nueva consume un intento del presupuesto raíz `max_tool_calls`, compartido con tools y sus rechazos. No consume una llamada al modelo si el callback no delega. `passed: false` es un resultado confirmado: no dispara reflexión, otra consulta ni corrección automática. La aplicación decide sus siguientes acciones dentro del mismo saldo.

La clave identifica entrada y versión del criterio. Misma clave, entrada JSON y opciones de revisión confirmadas devuelve el recibo guardado sin invocar el callback. Cambiar entrada/opciones con la misma clave falla antes de ejecutar; para otro criterio usar otra clave. El orden de propiedades JSON no cambia la identidad. No reutilizar una clave si se necesita comprobar de nuevo un estado externo que pudo cambiar.

El resultado contiene `key`, `execution_id` y `verdict`. El contexto contiene `objective_id`, `verification_key` y `execution_id`; npm añade `signal` y `delegate` no enumerables. Python los expone como atributos `cancel_event` y `delegate`. Entrada, opciones y veredicto se guardan en `objective.inspect().verifications`; elegir hechos/recibos apropiados para persistir.

Si un callback falla, devuelve un veredicto inválido o el proceso muere, queda pendiente. Otro intento con esa clave produce `VerificationUncertain`, derivada de `ToolOperationUncertain`; no vuelve a ejecutar la comprobación ni se convierte en feedback seguido de otra inferencia. Puede haberse interrumpido incluso antes de que comenzara el callback. Detener primero al ejecutor original y verificar los hechos externos antes de resolver:

```javascript
const entry = objective.inspect().verifications['sum-receipt-v1'];
objective.resolveVerification({ key: 'sum-receipt-v1', executionId: entry.execution_id,
  verdict: { passed: true, evidence: { receipt: 'recibo-comprobado' } } });
```

Python: `objective.resolve_verification(key=..., execution_id=..., verdict=...)`. Exige el ID pendiente y rechaza resoluciones repetidas o IDs antiguos. No devuelve intentos al presupuesto. Con un objetivo durable, claim/contador y veredicto/ledger se confirman atómicamente en HiveDB y se recuperan al reabrir; sin store, el registro dura lo que el proceso.

## Revisión con modelo, solo opt-in

Para una comprobación que necesite modelo, usar el agente revisor mediante el contexto. Esta ruta requiere una referencia de evaluación y una justificación explícitas, registradas junto al claim:

```javascript
const check = await objective.verify('semantic-check-v1', facts, async (value, context) => {
  const result = await context.delegate(reviewer, JSON.stringify(value));
  if (result.status !== 'completed') throw new Error('Revisión inconclusa');
  return JSON.parse(result.content);
}, { modelReview: {
  evaluationId: 'evaluaciones/mi-tarea-v3',
  justification: 'La evaluación de esta tarea justifica esta revisión adicional',
} });
```

Python usa `model_review={'evaluation_id': ..., 'justification': ...}` y un callback `async def` con `await context.delegate(reviewer, prompt)`. La referencia y razón no pueden estar vacías; registrarlas no demuestra que una evaluación externa sea válida. La aplicación debe comparar costo y calidad de su tarea antes de habilitar la revisión.

El revisor hereda el mismo objetivo, su cancelación y profundidad 1. Sus descendientes también consumen el saldo raíz. No puede sustituirlo por otro objetivo ni obtener llamadas nuevas al reabrir. `max_depth: 0` impide revisores. Sin opt-in, `delegate` falla antes de llamar al modelo. Si el resultado no puede convertirse al veredicto estricto, no se hace otra solicitud para repararlo.

El ámbito espera a todos los revisores iniciados, aunque el callback no los espere explícitamente. Al fallar, los cancela y espera a su cierre. El contexto no puede usarse después de terminar el callback. No es un sandbox para código de aplicación que llame directamente a proveedores u otros servicios: usar `context.delegate` para la revisión gestionada por el SDK.

## Cancelación y evidencia de pruebas

`verify` acepta `signal` en npm y `cancel_event` en Python. También responde a `objective.cancel()` y al deadline original del objetivo. Si un resultado conocido llega después de cancelación, se confirma antes de comunicarla al caller; `inspect` y una lectura posterior de la misma clave permiten recuperarlo sin repetir trabajo.

Python ejecuta callbacks síncronos en un executor y mantiene ocupado el store hasta que terminan, incluso ante cancelaciones repetidas. Los callbacks que delegan deben ser asíncronos. No se matan threads ni se deshacen efectos externos.

`node scripts/evaluate-verifiers.mjs` genera [el reporte de fixtures](../evals/verifier-contract.json). Sus tres chequeos programáticos coinciden con los hechos conocidos. El revisor simulado añade una llamada por caso y acepta dos fixtures incorrectos. Los tokens son reportes sintéticos: esta prueba valida contabilidad y contratos; no mide calidad, facturación o ahorro de modelos de producción. No justifica habilitar revisiones LLM por defecto.

El ledger durable de objetivos usa formato 4 (incluye el kernel de resúmenes) y lee 1–3 conservando saldos. Las sesiones vinculadas usan formato 3; al instalar planes de memoria pasan a formato 4. Los lectores que desconocen esos formatos fallan antes de ejecutar. Reconstruir ambos bindings juntos y no mezclar artefactos de snapshots distintos.
