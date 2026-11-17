# Tools MCP

El cliente MCP vive en Rust y comparte implementación entre npm y Python. Admite stdio y Streamable HTTP para las revisiones `2025-11-25` (por defecto) y `2025-06-18`. La revisión se elige explícitamente; no hay fallback ni soporte declarado para la revisión 2026.

## npm

```javascript
import { Agent, McpClient } from '../packages/npm/index.mjs';

const mcp = await McpClient.connect({
  command: 'mi-servidor-mcp', args: [],
  env: { SERVICE_TOKEN: process.env.SERVICE_TOKEN },
});
// Alternativa: McpClient.connect({ transport: 'http',
//   url: 'https://servicio.example/mcp', headers: { Authorization: 'Bearer …' } });
const agent = new Agent({ model: 'mi-modelo', provider,
  tools: mcp.tools({ prefix: 'servicio_', names: ['buscar'] }) });
try { console.log(await agent.run('Busca el documento')); }
finally { agent.close(); await mcp.close(); }
```

`provider` es un proveedor configurado por la aplicación. `catalog` devuelve una copia de las definiciones remotas. `info` expone el resultado del handshake, incluidas instrucciones del servidor; estas no se añaden automáticamente al prompt.

## Python

```python
from hive_sdk import Agent, McpClient

async with await McpClient.connect(command='mi-servidor-mcp') as mcp:
    agent = Agent(model='mi-modelo', provider=provider,
                  tools=mcp.tools(prefix='servicio_', names=['buscar']))
    try:
        result = await agent.run('Busca el documento')
    finally:
        agent.close()
```

HTTP usa `await McpClient.connect(transport='http', url=..., headers=...)`. No instala servidores; stdio ejecuta directamente el comando, sin shell. Solo hereda variables básicas de entorno; pasar credenciales explícitamente en `env`. La configuración de proceso, headers y credenciales no se guarda en el checkpoint del agente.

## Presupuesto y resultados inciertos

Las tools adaptadas pasan por validación y presupuesto de intentos del motor, como cualquier tool local. El descubrimiento paginado sucede una vez al conectar, no en cada inferencia. Los schemas de entrada y salida se compilan en Rust, sin descargar referencias. El catálogo permanece fijo durante la conexión; cambiarlo requiere reconectar y construir otra configuración del agente.

Un resultado MCP `isError` y una respuesta JSON-RPC de error confirmada son errores conocidos. Un resultado que incumple `outputSchema` se devuelve como error estructurado con el resultado recibido. Estos casos permiten continuar dentro del presupuesto restante.

Timeout, cancelación, respuesta perdida o malformada después del despacho producen `McpOperationUncertain`, derivada de `ToolOperationUncertain`. El agente conserva el claim durable de la tool, cierra la conexión y no vuelve a llamar al modelo. Reconectar no repite la operación. Revisar el efecto externo y usar la [reconciliación explícita](checkpoints.md) antes de reanudar. Cerrar un socket o proceso no demuestra que se deshiciera un efecto remoto.

La metadata `_meta["hive/context"]` incluye `run_id`, `execution_id` y `tool_call_id` cuando existen. El servidor puede usarla para implementar idempotencia. El SDK no garantiza exactamente una ejecución externa.

## Límites y transporte

Valores por defecto: timeout de 30 segundos por operación, 8 MiB por mensaje/respuesta, 256 tools, 16 páginas y 128 mensajes entrantes por operación. Opciones npm: `timeoutMs`, `maxMessageBytes`, `maxTools`, `maxPages`, `maxMessages`, `protocolVersion`; Python usa sus equivalentes snake_case. Una conexión permite una llamada activa; otra llamada concurrente falla antes de despachar.

HTTP admite JSON y SSE, sesiones mediante `Mcp-Session-Id`, header de versión y cierre DELETE de mejor esfuerzo. No sigue redirects, repite solicitudes, renueva autenticación ni reemplaza sesiones perdidas. No abre un stream GET de fondo. Se responde a ping; sampling, elicitation y roots se rechazan. No se recuperan recursos enlazados ni se interpreta contenido multimedia. Anotaciones remotas no conceden permisos.

Stdio cierra y recoge el proceso hijo directo. No promete terminar todos sus descendientes. Las notificaciones del servidor no alteran el catálogo registrado.

El alcance corresponde a [Streamable HTTP 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25/basic/transports) y al [ciclo de inicialización](https://modelcontextprotocol.io/specification/2025-11-25/basic/lifecycle), con las limitaciones anteriores.

## Varios servidores y recarga en caliente: `McpManager`

`McpManager` (Rust, npm y Python) mantiene un conjunto de servidores con nombre y lo **reconcilia** con el conjunto deseado sin reiniciar la aplicación:

```javascript
const manager = new McpManager();
const report = await manager.apply({
  docs:   { command: 'servidor-docs', args: [] },
  buscar: { transport: 'http', url: 'https://servicio.example/mcp', headers: { Authorization: 'Bearer …' } },
  viejo:  { command: 'otro', enabled: false },       // en el conjunto, sin conectar
});
// report: { added, removed, restarted, unchanged, disabled, failed: { nombre: motivo } }
const agent = new Agent({ model, provider, tools: manager.tools() });   // nombres <servidor>__<herramienta>
await manager.apply(nuevoConjunto, { drainMs: 5000 });                    // recarga
```

Python: `McpManager()` con `await manager.apply({'docs': {'command': ...}}, drain_ms=5000)`, `manager.tools()`, `manager.status`, `await manager.reconnect(name)` y `async with`.

- **`apply` recibe el conjunto completo.** Los servidores con la **misma config y vivos** se dejan como están (mismo proceso); los nuevos o cambiados se conectan **en paralelo**; los que ya no están se cierran. El resultado se publica **de una vez**: quien lea `status`/`catalog` no ve un conjunto a medias.
- **Un conjunto inválido no cambia nada** (nombre fuera de `1..64` letras, dígitos, `_`, `-` y sin `__`; config que no es un objeto; `enabled` que no es booleano; más de 64 servidores). **Un servidor que no conecta se informa en `failed`** y los demás no se ven afectados; su estado es `failed` con el motivo.
- **Nada se reconecta ni se reintenta solo.** Una llamada que termina incierta cierra su servidor (la política de solicitud única de siempre): pasa a `closed`, las llamadas siguientes fallan **sin enviar nada** con `MCP_SERVER_UNAVAILABLE … reconnect it`, y solo `reconnect(nombre)` o el siguiente `apply` (que reinicia lo cerrado o fallido aunque su config no cambie) abren una sesión nueva **para llamadas futuras**; la llamada incierta no se repite. Los servidores restantes no se tocan.
- **`drainMs`**: antes de cerrar un cliente retirado se espera hasta ese plazo a que termine la llamada en curso; sin espera (0), esa llamada queda incierta como en cualquier cierre.
- **Herramientas:** `tools()` devuelve las de los servidores conectados con nombre `<servidor>__<herramienta>` (sin colisiones entre servidores). Es una instantánea: una herramienta que sigue existiendo funciona tras una recarga (la llamada busca el servidor al ejecutarse y falla con un motivo claro si ya no está), pero **las herramientas nuevas requieren pedir `tools()` de nuevo y un Agent nuevo**; un Agent en curso no se actualiza.
- `status` lista `{name, state: connected|failed|disabled|closed, tools, error, config_hash, server}`; `catalog` las herramientas conectadas.

Validación: `tests/mcp_manager.rs` (6 pruebas con el servidor stdio de fixtures: conjunto en paralelo y nombres, recarga que conserva el proceso de lo sin cambios y reinicia lo cambiado, conjunto inválido sin efectos, un servidor caído que no afecta a otros y se recupera al corregir la config, llamada incierta sin reenvío más `reconnect`/`apply`, deshabilitados, y `drainMs` con una llamada lenta que termina frente a otra cortada) y 2 pruebas en npm (incluido un Agent usando las herramientas) y 2 en Python. **Solo se probó con el servidor de fixtures por stdio**; con servidores MCP reales y HTTP solo rige lo ya probado de `McpClient`. No hay observación de `tools/list_changed` ni reconexión con retroceso: son decisiones, no pendientes ocultos (ver arriba).

## Cambios en la lista de herramientas (`tools/list_changed`)

Un servidor MCP puede cambiar sus herramientas mientras está conectado y avisarlo con `notifications/tools/list_changed`.

- **Detectar.** El cliente no tiene un lector en segundo plano (lee dentro de cada intercambio), así que un aviso se **ve con la siguiente respuesta**, no en el instante en que ocurre. `McpClient.toolsChanged` / `tools_changed` lo indica, y el estado del `McpManager` trae `tools_changed` por servidor. Funciona por stdio y por HTTP.
- **Refrescar.** `client.refreshTools()` / `refresh_tools()` (y `manager.refreshTools(nombre)` / `refresh_tools`) vuelve a listar todas las páginas con los mismos límites y validaciones de la conexión y **adopta la lista nueva**: devuelve `{added, removed, changed}` por nombre (`changed`: otra descripción o esquema de entrada). Se puede llamar sin aviso previo, para listar ahora. Limpia el indicador **antes** de listar, de modo que un cambio anunciado durante el listado no se pierde. No se puede refrescar con una llamada en curso (`client already has an active request`, no se mezcla con ella en la misma conexión), con el cliente cerrado ni con un servidor desconectado; un servidor sin herramientas devuelve un diff vacío. Un fallo de protocolo al listar cierra el cliente como cualquier otro intercambio incierto.
- **Qué ve un Agent.** El catálogo (`catalog`, `tools()`) usa la lista nueva; **un `Agent` ya creado conserva las herramientas con las que se creó**: el conjunto de herramientas de una ejecución es parte de lo que la política y los presupuestos evaluaron, así que no cambia a mitad. Lo ve el siguiente `Agent` o la siguiente ejecución que se cree. Una llamada en curso sigue con la herramienta con que empezó.

**Validación:** `tests/mcp.rs` (aviso visto con la respuesta que lo trae, catálogo que no cambia solo, diff `added/removed/changed`, herramienta nueva invocable, indicador limpio, no se intercala con una llamada lenta, cliente cerrado, servidor sin herramientas) con un modo `mutating` del servidor stdio de pruebas, y lo mismo por npm y Python, con cliente y con manager. **No probado:** el aviso por HTTP contra un servidor real y varios servidores a la vez cambiando.

## Transporte SSE clásico (`transport: 'sse'`)

Es el transporte HTTP+SSE anterior a Streamable HTTP; lo habla, por ejemplo, el *MCP Server Trigger* de n8n.
Se abre un `GET` que devuelve un flujo `text/event-stream` y cuyo primer evento (`endpoint`) indica a qué URL
hacer `POST`; las respuestas llegan por ese mismo flujo como eventos `message`.

- `McpClient.connect({ transport: 'sse', url, headers })` / `McpClient.connect(transport='sse', url=...)`, y lo mismo en `McpManager.apply`. La revisión por defecto es `2025-03-26`; se aceptan `2024-11-05`, `2025-03-26`, `2025-06-18` y `2025-11-25` y el servidor puede responder con cualquiera de ellas.
- El endpoint de mensajes debe estar en el **mismo origen** (esquema, host y puerto) que el flujo y sin credenciales en la URL; si no, se rechaza antes de enviar nada (ni cabeceras). Sin redirecciones.
- La respuesta debe ser 2xx con `text/event-stream`, y el evento `endpoint` debe llegar dentro de `timeout_ms`.
- Se manejan comentarios de keep-alive, eventos desconocidos, `data:` en varias líneas, LF y CRLF, con los mismos límites de bytes y mensajes que los demás transportes.
- Un `ping` del servidor se responde por `POST`; otras peticiones del servidor reciben `-32601`; `tools/list_changed` se detecta igual que en los otros transportes.
- Sin reintentos: un `POST` rechazado o un flujo que termina mientras una llamada está en curso deja el resultado como **incierto** y cierra el cliente.

Verificado con servidores falsos (Rust, npm, Python) y, solo en lectura (conexión + `tools/list`), contra dos servidores reales de n8n.

## Gateway: servidores de HiveDB (`mcpServers`)

`hive-gateway-rust` conecta las filas habilitadas de `mcpServers` (`{id, name, enabled, transport, url, headers?}`)
con un `McpManager` y las mantiene al día cada pocos segundos: un cambio en las filas se aplica solo, y un servidor
caído o cerrado se reintenta cada 30 s (el manager por sí solo no reconecta).

- `GET /api/mcp/servers` muestra el estado real (`connected`, `connecting`, `disconnected`, `error`) y las herramientas listadas. Las URL salen sin secretos.
- Solo se conectan `sse` y `http`. Una fila `stdio` se marca como error: conectarla ejecutaría un comando elegido por quien pueda escribir la fila.
- Las herramientas se ofrecen **solo al coordinador** (profundidad 0) como `mcp_<servidor>__<herramienta>`; los especialistas siguen siendo de texto. Los nombres que no puedan ser un nombre de función seguro (espacios, más de 64 caracteres) se omiten.
- Cada llamada pasa por la misma política de herramientas y aprobaciones que cualquier otra (`docs/policy.md`). La política por defecto es *allow*; para herramientas con efectos (enviar correo, crear o cancelar eventos) se recomienda una regla `ask`, por ejemplo sobre `mcp_email__enviarEmail`.
- Una llamada se ejecuta una sola vez. Si la conexión se cae mientras corre, el modelo recibe `MCP_OPERATION_UNCERTAIN` (no la repite sin comprobarlo) aunque el servidor se reconecte después.

