# Hive SDK Rust

Motor de agentes en Rust con APIs para JavaScript/TypeScript y Python. Proyecto independiente del SDK Bun existente; no modifica `../../hive` ni `../../hive-sdk`.

**Estado: SDK nuevo desde cero, con una primera implementación funcional; todavía no publicado.** Se inicia con bases nuevas y APIs propias. El motor no depende de Bun, Node ni Python. Los bindings ejecutan las tools y proveedores personalizados en el lenguaje de la aplicación.

## Qué funciona hoy

- [Cola durable y pool de ejecutores Rust](docs/durable-queue.md), opcionales: prioridad/FIFO por lane, idempotencia, leases y fencing, reintentos seguros, cancelación cooperativa y claims atómicos con presupuesto raíz. Callbacks Node/Python e intercambio con recuperación tras salida abrupta.
- [Delegación durable a especialistas](docs/durable-delegation.md): grupos atómicos, sesiones estables, síntesis única y pruebas de aceptación ligadas al resultado, con APIs Node/Python.
- [Gateway con delegación durable](docs/gateway-durable-delegation.md): consultas paralelas de texto, objetivo compartido persistente y una continuación del coordinador con fallos/certeza explícitos.
- [Scheduler embebido](docs/native-scheduler.md): fechas, intervalos y cron nativo, publicación atómica sobre la cola, límites y sondeo optativo en Rust/Node/Python.
- [Ampliación de paridad funcional con TS](docs/parity.md), con referencia por hashes y exclusión de gestión de proyectos. Sigue incompleta; la matriz separa capacidades parciales y pendientes.
- [Servicios de catálogo embebidos](docs/embedded-services.md): CRUD de diez tipos de entidad, revisiones optimistas, ámbitos por tenant, búsqueda BM25 e importación YAML de skills. Ocho tools nativas de memoria/descubrimiento, disponibles en npm y Python.
- [Navegador CDP opcional](docs/native-browser.md): cliente Rust para conectar a una página existente, navegar, interactuar, extraer texto y capturar imágenes; sin controlador Node/Bun. La validación con Chromium real sigue pendiente por restricciones del sandbox.
- MCP resources/list, resources/read, prompts/list y prompts/get, incluso en servidores sin tools; exposición explícita en npm y Python.
- Máquina de estados Rust para conversación → modelo → tools → respuesta.
- Handles Rust para agentes en memoria y sesiones durables, sin snapshots completos por transición del ciclo. Los checkpoints durables usan workers con espera al cancelar. [Memoria](docs/engine-handles.md) y [sesiones durables](docs/durable-handles.md), con mediciones locales.
- Factories `open` y alternativas públicas asíncronas para cargar, inspeccionar, reconciliar y cerrar sin usar las entradas nativas síncronas de almacenamiento. [API y alcance pendiente](docs/async-storage.md).
- Control durable interno asíncrono de objetivos, delegación, resúmenes, verificaciones y triggers; ámbitos protegidos durante claims/finalizaciones y watchers sin inferencias mientras esperan. [Contrato y medición de concurrencia](docs/objective-runtime.md).
- Transporte HTTP asíncrono Rust para Chat Completions, con cliente reutilizable, timeout, límite de bytes y cancelación de la solicitud activa.
- Adaptador nativo Anthropic Messages para texto y tools del cliente, JSON/SSE y normalización de uso de caché, con el mismo transporte y presupuestos.
- Gemini GenerateContent nativo con JSON/SSE, tools, firmas durables y uso de caché/thoughts normalizado.
- Tools síncronas/asíncronas y proveedores personalizados desde npm y Python.
- Historial en memoria o persistente en HiveDB, eventos observables, cierre explícito y rechazo de ejecuciones concurrentes sobre el mismo agente.
- Sesiones identificadas por inquilino, agente y conversación; control de versiones y recuperación explícita de ejecuciones interrumpidas.
- Checkpoints después de cada respuesta y resultado de tool; reanudación explícita conservando presupuesto e identidad del turno.
- Ledger de tools con ID de ejecución y resultados; operaciones inciertas requieren reconciliación antes de continuar.
- Límites de llamadas, detección de resultados repetidos y presupuesto opcional de tokens reportados.
- Resultado estructurado que distingue una respuesta terminada de una ejecución detenida.
- Salida JSON final opcional mediante schema compartido, adaptadores nativos y validación Rust sin llamadas de reparación.
- Validación JSON Schema en Rust al registrar tools y antes de sus callbacks, con feedback estructurado para corregir argumentos inválidos.
- Streaming real de texto, fragmentos de tools y uso mediante iteradores asíncronos npm/Python, incluido `resumeStream` / `resume_stream`.
- Reserva de salida antes de llamar al proveedor y deadline absoluto del turno, conservados al reanudar.
- Entrada acotada en bytes JSON; selección opcional de turnos antiguos completos sin borrar historial ni usar llamadas de resumen.
- Presupuesto BPE local con codificación explícita y margen; reporte de estimación separado del consumo real.
- Cliente MCP Rust para stdio y Streamable HTTP, catálogo validado y operaciones inciertas sin reintentos automáticos.
- Delegación explícita dentro del proceso mediante `Objective`: presupuesto compartido, profundidad y cancelación heredadas. También admite presupuesto raíz durable en HiveDB, coordinado con los checkpoints de cada sesión.
- Verificadores explícitos con callbacks, caché durable y reconciliación. La revisión LLM exige opt-in y consume el mismo presupuesto; no hay revisión automática al terminar.

**Pendiente:** capacidades adicionales de proveedores, evaluación de calidad/costo con modelos remotos y distribución de binarios multiplataforma. Búsqueda local, resúmenes explícitos presupuestados, planes de contexto, perfiles JSON, entrada acumulada y ventana combinada están disponibles. Hay una evaluación de fixtures reproducible, sin modelos de pago. Los triggers están desactivados por defecto. La cancelación HTTP está implementada; los callbacks de aplicación requieren cooperación y no se interrumpen hilos síncronos a la fuerza. `onEvent` sigue siendo de ciclo de vida; los deltas se leen con `stream`.

## Compilar y probar localmente

### Gateway para Hive Desktop Rune

`cargo build -p hive-gateway-rust` genera el servidor local para el cliente Rune vecino.
Usa la configuración existente de HiveDB y los transportes nativos del SDK; no requiere Bun ni Node.
La integración cubre chat, streaming, narraciones, cancelación, sesiones, oficina 3D en vivo y
delegación síncrona de consultas de texto a los especialistas de HiveDB. Consulta
[`../hive-desktop-rune/README.md`](../hive-desktop-rune/README.md) para preparar una copia aislada y abrir la app.
`cargo test -p hive-gateway-rust` verifica el contrato HTTP/WebSocket con un proveedor local determinista.
Las dependencias de login de esta primera entrega están preparadas para Linux (`libcrypt` / `libargon2`).

El proveedor HiveAgents usa el adaptador nativo `kind: "hiveagents"`, con preparación del GGUF
mediante `/api/status` y `/api/load`, contexto obtenido de HiveDB e inferencia con `model: "local"`.
Conserva el GGUF configurado en los checkpoints; no repite inferencias fallidas. Ver
[diagnóstico de la delegación y validación](docs/hiveagents-delegation-fix.md).

### SDK y bindings

Requiere Rust >=1.95, Node >=24 y Python >=3.10. Estos son requisitos de desarrollo: los paquetes finales deberán incluir binarios precompilados para el consumidor.

Desde esta carpeta:

```bash
python3 scripts/build-dev.py
cargo test --locked
node --test packages/npm/test/*.test.mjs
PYTHONPATH=packages/python/python python3 -m unittest discover -s packages/python/tests -v
PYTHONPATH=packages/python/python python3 tests/session-parity.py
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Las pruebas usan proveedores deterministas y servidores HTTP locales, sin API keys reales ni gasto en modelos.

## JavaScript / TypeScript

```javascript
import { Agent, OpenAICompatible } from './packages/npm/index.mjs';

const agent = new Agent({
  model: process.env.HIVE_MODEL,
  provider: new OpenAICompatible({ apiKey: process.env.OPENAI_API_KEY }),
  tools: [{
    name: 'sumar',
    description: 'Suma dos números',
    parameters: {
      type: 'object',
      properties: { a: { type: 'number' }, b: { type: 'number' } },
      required: ['a', 'b'],
    },
    execute: async ({ a, b }) => a + b,
  }],
});

const result = await agent.run('Suma 2 y 3 usando la tool');
console.log(result.status, result.content, result.stop_reason, result.stats);
agent.close();
```

El nombre npm provisional es `@johpaz/hive-sdk-rust`. El paquete tiene `private: true` hasta completar la distribución y validación de binarios. No se publica ni reemplaza el SDK actual automáticamente.

## Python

Tras el build local, usar `PYTHONPATH=packages/python/python`:

```python
import asyncio
import os
from hive_sdk import Agent, OpenAICompatible, Tool

async def main():
    agent = Agent(
        model=os.environ['HIVE_MODEL'],
        provider=OpenAICompatible(api_key=os.environ['OPENAI_API_KEY']),
        tools=[Tool(
            name='sumar',
            description='Suma dos números',
            parameters={'type': 'object', 'properties': {
                'a': {'type': 'number'}, 'b': {'type': 'number'}},
                'required': ['a', 'b']},
            execute=lambda args: args['a'] + args['b'],
        )],
    )
    result = await agent.run('Suma 2 y 3 usando la tool')
    print(result)
    agent.close()

asyncio.run(main())
```

El nombre PyPI provisional es `johpaz-hive-sdk-rust`; se importa como `hive_sdk`. El proyecto incluye configuración Maturin para generar wheels; todavía no hay artefactos publicados.

## Streaming y cancelación

```javascript
const controller = new AbortController();
for await (const event of agent.stream('Explica el resultado', { signal: controller.signal })) {
  if (event.type === 'text_delta') process.stdout.write(event.text);
  if (event.type === 'result') console.log(event.result);
}
// Desde otro evento: controller.abort() o agent.cancel().
```

```python
cancel_event = asyncio.Event()
async for event in agent.stream('Explica el resultado', cancel_event=cancel_event):
    if event['type'] == 'text_delta':
        print(event['text'], end='', flush=True)
    if event['type'] == 'result':
        print(event['result'])
# Desde otra tarea: cancel_event.set() o agent.cancel().
```

El resultado final llega en `result`; los deltas son parciales y no confirman el turno. Las tools esperan argumentos ensamblados, respuesta terminada y validación. Un error/cancelación deja la operación persistente incierta si aún no hay respuesta completa guardada, sin reintentar al proveedor.

Al abandonar un iterador, cerrarlo: JavaScript lo hace al salir de `for await`; en Python usar `contextlib.aclosing` o `await events.aclose()`. `resumeStream()` / `resume_stream()` continúan un checkpoint limpio conservando contadores. Ver [contrato y límites](docs/streaming.md).

## Sesiones durables con HiveDB

HiveDB es una biblioteca de almacenamiento Rust embebida. Aquí se enlaza directamente al motor: no hay proceso de base de datos, paquete npm de HiveDB ni instalación Python adicional. La dependencia está fijada al commit de HiveDB 0.7.3 en Git; compilar desde fuentes necesita descargarla una vez. Los consumidores de futuros binarios precompilados no necesitarán hacerlo.

```javascript
import { Agent, SessionStore } from './packages/npm/index.mjs';
const store = new SessionStore({ path: './data/hive-sdk', tenantId: 'empresa-1' });
const agent = new Agent({
  model: 'mi-modelo', store, agentId: 'asistente', sessionId: 'conversacion-1',
  provider: { complete: async () => ({ content: 'Hola' }) },
});
await agent.run('Hola');
agent.close();
store.close();
// Al reabrir el mismo path/tenant/agente/sesión con la misma configuración,
// se recupera el historial. Los callbacks deben registrarse de nuevo.
```

En Python: `store = SessionStore('./data/hive-sdk', tenant_id='empresa-1')` y `Agent(..., store=store, agent_id='asistente', session_id='conversacion-1')`.

Compartir una instancia de store entre agentes. Usar `store.forTenant('otra-empresa')` / `store.for_tenant('otra-empresa')` para otro ámbito sobre el mismo handle; cerrar todos los ámbitos para liberar la base. Un segundo handle independiente sobre el mismo directorio puede fallar por el lock del motor.

La configuración de modelo, system prompt, tools y política debe coincidir al reabrir; las credenciales y callbacks se suministran de nuevo. Cambiar configuración requiere otra sesión. Los IDs separan registros; la autorización de acceso a cada inquilino pertenece a la aplicación.

Cada ejecución guarda el prompt antes de empezar y confirma cada transición del motor en HiveDB. Una operación se marca antes de despacharla; su resultado y el nuevo checkpoint se guardan en una transacción. El historial visible de conversación se confirma al terminar el turno.

Para continuar un checkpoint limpio, reconstruir el agente con la misma configuración y `resume: true` en npm / `resume=True` en Python, y llamar `await agent.resume()` en ambos. No se envía otro prompt ni se reinician contadores. La carga normal sigue devolviendo `SESSION_INTERRUPTED` para ejecuciones inconclusas.

Si el modelo o una tool empezó sin resultado guardado, la reanudación falla con `SESSION_OPERATION_UNCERTAIN`. Usar `store.inspect` para conocer la operación y, después de verificar su resultado externo, `resolveTool` / `resolve_tool` o `resolveModel` / `resolve_model`. No se vuelve a ejecutar automáticamente. Ver [ejemplos de recuperación](docs/checkpoints.md).

Para descartar el turno completo, `store.recover(...)` conserva su ledger como abandonado y restaura la conversación anterior. Detener primero al ejecutor original y revisar sus efectos externos. Usar una carpeta nueva para este SDK; no compartir la base productiva del SDK anterior.

## Política de ejecución

Por defecto: ocho llamadas al modelo y dieciséis intentos de tool por ejecución, contando ejecuciones y rechazos de argumentos; dos resultados idénticos de la misma tool con los mismos argumentos detienen la ejecución. Una respuesta final termina inmediatamente, sin llamadas adicionales de revisión.

Para polling deliberado, marcar la tool con `allowRepeatedResults: true` en npm o `allow_repeated_results=True` en Python. También se puede desactivar el detector global con `max_identical_tool_results: null` / `None`; los demás límites siguen activos.

Los schemas se compilan al registrar la tool. Sin `$schema`, se usa Draft 2020-12; se respetan los drafts reconocidos declarados. Se comprueban formatos conocidos; los desconocidos quedan como anotaciones. Las referencias deben estar incluidas en el schema: no se resuelven archivos ni URLs externos. No se insertan defaults ni convierten tipos.

Una llamada inválida emite `tool_rejected` y devuelve `INVALID_TOOL_ARGUMENTS` al modelo, sin ejecutar el callback. `stats.tool_rejections` cuenta estos rechazos y `stats.tool_calls` las ejecuciones reales. Ambos consumen el límite `max_tool_calls`. Los rechazos repetidos mantienen su detector incluso en tools habilitadas para polling; desactivar globalmente el detector sigue siendo posible. Ver [validación de tools](docs/tool-validation.md).

`policy.max_total_tokens` es opcional. Acumula entrada+salida reportadas y detiene al alcanzar el umbral; si el proveedor omite el uso, devuelve `usage_unavailable`. **No garantiza un máximo facturado:** no se cuenta exactamente la entrada antes de inferir. La reserva previa nueva se aplica a salida y depende de que el proveedor respete su límite y reporte el uso correctamente.

Para controlar salida antes de inferir y tiempo total:

```javascript
const policy = {
  max_output_tokens: 2048,       // máximo solicitado por llamada
  max_total_output_tokens: 8192, // salida agregada del turno
  max_run_ms: 60_000,            // incluye pausas y reanudaciones
};
```

En Python se usa el mismo dict. Estos límites son opt-in; null/None conserva el comportamiento anterior. El motor reserva el menor valor entre cap por llamada y salida restante, lo envía al proveedor y reconcilia con el uso real. Una llamada incierta mantiene su reserva. El deadline no se reinicia al reanudar. Ver [presupuestos y deadline](docs/budgets.md).

Los errores de tools vuelven al modelo como resultados de error. Un error del proveedor o del hook aborta sin confirmar el historial del turno; no se reintenta automáticamente. Los efectos externos de una tool ya ejecutada no se deshacen. Una parada por política conserva el diagnóstico y deja el agente terminal: usar una sesión nueva para otra ejecución explícita. La persistencia conserva esta parada al reabrir; no reinicia el presupuesto automáticamente.

Para acotar entrada, configurar `policy.max_context_bytes: 65536`. `context_overflow: 'stop'` devuelve `context_limit` sin inferir; `'drop_oldest_turns'` omite turnos antiguos completos de la solicitud conservando sistema, turno activo e historial. `request.context` informa bytes JSON y mensajes omitidos. Esta medida no equivale a tokens del modelo.

Para exigir JSON final, configurar `policy.structured_output: {name, schema}`. El schema se cuenta dentro del contexto y se valida en Rust; una respuesta inválida detiene con `invalid_structured_output`, conserva su texto/uso y no activa una llamada de reparación. Disponible en proveedores personalizados y adaptadores OpenAI, Anthropic y Gemini sujetos a compatibilidad del modelo. Ver [ejemplos y contrato](docs/structured-output.md).

También se admite `policy.input_token_budget: { encoding: 'cl100k_base', max_tokens: 8192, overhead_tokens: 256 }`. Se cuenta BPE local sobre el JSON neutral, sin red, y se expone `request.context.token_estimate`. La aplicación elige codificación y margen según su integración; no se adivinan por alias. Este conteo no reproduce la plantilla del vendor ni sustituye su uso real reportado.

## Documentación

- [Plan y avance](PLAN.md)
- [Estado de entrega al 10 de noviembre de 2026](docs/progress-2026-11-10.md)
- [Cómo validar el SDK con el proyecto local](docs/local-project-validation.md)
- [Especificación de contratos y comportamiento](SPEC.md)
- [Decisiones sobre el agent loop y el paper](docs/agent-loop.md)
- [Validación inicial y límites de las pruebas](docs/validation.md)
- [Checkpoints y reconciliación de operaciones](docs/checkpoints.md)
- [JSON Schema y corrección de argumentos](docs/tool-validation.md)
- [Streaming, deadlines y cancelación](docs/streaming.md)
- [Reservas de salida y deadline global](docs/budgets.md)
- [Contexto acotado sin llamadas de resumen](docs/context.md)
- [Presupuesto local de tokens de entrada](docs/tokens.md)
- [Anthropic Messages nativo](docs/anthropic.md)
- [Gemini y firmas durables](docs/gemini.md)
- [Tools MCP, transportes y reconciliación](docs/mcp.md)
- [Objetivos y delegación con presupuesto compartido](docs/objectives.md)
- [Verificadores explícitos y costo de revisión](docs/verification.md)
- [Triggers, callbacks y watchers opcionales](docs/triggers.md)
- [Fuentes, búsqueda local y planes de contexto con resúmenes proporcionados](docs/memory.md)
- [Generación explícita de resúmenes y costo auxiliar](docs/summary.md)
- [Paquetes nativos, wheels y matriz de distribución](docs/distribution.md)
