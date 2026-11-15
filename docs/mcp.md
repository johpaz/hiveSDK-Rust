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
