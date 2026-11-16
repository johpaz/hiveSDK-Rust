# Herramientas nativas: archivos, comandos y web

Feature Cargo `tools` (activado en los bindings). Sin dependencias nuevas ni procesos auxiliares. Cada herramienta es una definición `Tool` más un ejecutor; el Agent loop sigue siendo la autoridad de presupuesto, ledger e incertidumbre, y estas herramientas no llevan idempotencia propia.

```js
const tools = new NativeTools({ root: '/proyecto', command: { allowed_programs: ['git'] }, web: { search: { kind: 'brave', api_key } } });
const agent = new Agent({ model, tools: await tools.createTools() });
```

```python
tools = NativeTools({'root': '/proyecto', 'web': {}})
agent = Agent(..., tools=await tools.create_tools())
```

Solo se anuncian las herramientas que la configuración puede ejecutar: `command_run` requiere `command`, `web_fetch` requiere `web` y `web_search` requiere `web.search`.

## Configuración

| Campo | Defecto | Efecto |
| --- | --- | --- |
| `root` | obligatorio | Directorio existente; se canonicaliza al crear. |
| `max_read_bytes` / `max_write_bytes` | 256 KiB / 1 MiB | Topes de lectura por archivo/ventana (incluido el offset) y escritura. |
| `max_output_bytes` | 128 KiB | Tope de salida de comandos (por flujo) y cuerpos web. |
| `command.allowed_programs` | — | Nombres simples, sin separadores; coincidencia exacta. |
| `command.env` / `max_timeout_ms` | `{}` / 30 000 (máx. 600 000) | Entorno añadido y techo del timeout. |
| `web.allow_private_hosts` | `false` | Permite destinos locales/privados. |
| `web.allowed_hosts` | `[]` | Si no está vacío, `web_fetch` solo acepta esos hosts. |
| `web.timeout_ms` | 15 000 | Máximo 120 000. |
| `web.search` | ninguno | `brave` (resultado normalizado) o `custom` (JSON devuelto tal cual, acotado). |

Los campos desconocidos se rechazan. Los argumentos se validan contra el JSON Schema de cada herramienta (`additionalProperties:false`) antes de ejecutar.

## Archivos (`fs_*`)

`fs_read` (ventana por líneas, UTF-8), `fs_write` (escritura atómica con temporal + rename; `create_dirs` explícito), `fs_edit` (reemplazo exacto; falla si no hay coincidencia o no es única, salvo `replace_all`), `fs_list`, `fs_glob` (`*`, `?`, `**`; sin `/` compara el nombre en cualquier profundidad; evaluación mediante programación dinámica) y `fs_grep` (cadena literal, sin regex).

Sandbox: se rechazan rutas absolutas y componentes `..`; el ancestro existente más cercano se canonicaliza, así que los symlinks que salen del root fallan. `fs_glob` y `fs_grep` no siguen symlinks y visitan como máximo 50 000 entradas (`truncated:true` si se alcanza). El recorrido lista los archivos de cada directorio antes que sus subdirectorios. **Límite conocido:** la comprobación y el uso no son atómicos frente a otro proceso que modifique el árbol a la vez.

## Comandos (`command_run`)

Ejecución directa, nunca mediante shell: `;`, `$VAR` y comodines llegan como texto. El proceso hijo recibe el entorno vacío más `PATH` del host y `command.env`. `cwd` debe ser un directorio dentro del root. El timeout mata el hijo directo y devuelve `timed_out:true` con la salida parcial; la cancelación lo mata y falla con `CANCELLED`. La salida se sigue drenando tras el tope para no bloquear al hijo. Los lectores/escritor de pipes se abortan al salir; el drenaje puede añadir hasta cuatro segundos. Los descendientes creados por el programa no se terminan como grupo. Cada llamada captura su propio token antes de despachar al worker: la cancelación de una tool del Agent afecta solo a esa llamada; `cancel()` explícito afecta a todas las llamadas activas de la instancia. Las operaciones de archivos ya iniciadas esperan el I/O y pueden haber escrito antes de observar cancelación. El programa permitido puede hacer cualquier cosa que su propio binario permita: la lista blanca limita qué se lanza, no qué hace.

## Web (`web_fetch`, `web_search`)

`web_fetch` hace GET http(s) sin credenciales incrustadas, no sigue redirecciones (devuelve `status` y `location`) y corta el cuerpo en `max_output_bytes` (`truncated:true`). Por defecto rechaza `localhost`, `*.local`, IP literales y direcciones resueltas no públicas (loopback, privadas, link-local, CGNAT, ULA, multicast); un objetivo bloqueado nunca recibe tráfico. El cliente fija las direcciones DNS ya validadas, desactiva proxies implícitos y mantiene el nombre original para TLS. Se rechazan también rangos de documentación, benchmarking, IPv4 reservada y transición IPv6 no pública; la política es conservadora. El timeout incluye DNS, headers y cuerpo, y la cancelación interrumpe la lectura del cuerpo. Una consulta DNS del sistema ya iniciada puede terminar en su worker después de cancelar, sin autorizar la conexión HTTP.

`web_search` usa un endpoint y credenciales de configuración de confianza (no pasan por la regla de hosts públicos). Los errores HTTP no incluyen la URL. Las pruebas de Brave Search usan un servidor local que imita su respuesta; el sandbox actual impide abrir ese socket. **No se ha ejecutado contra el servicio real**. El cuerpo JSON de búsqueda tiene un límite de `max(max_output_bytes, 1 MiB)`; los cuerpos GET y cada flujo de comandos usan `max_output_bytes` en bytes antes de convertir a texto.

## Estado y pendientes

Contratos preparados en `crates/hive-core/tests/tools.rs` (15 pruebas) y `packages/{npm,python}` (3 pruebas cada uno): escapes por `..`, absolutas y symlinks, límites, comandos (allowlist, entorno, stdin, timeout, cancelación, truncado), bloqueo de destinos privados sin contacto, truncado y redirecciones, y adaptadores de búsqueda. Las filas `filesystem_tools`, `command_tools`, `web_search` y `http_tools` permanecen `partial`: falta comparación diferencial con la referencia TS, una prueba con un proveedor de búsqueda real, y la herramienta HTTP genérica (métodos, cuerpo y endpoints dinámicos); `web_fetch` es solo GET. No hay extracción de contenido legible de HTML.

Validación local del 15 de noviembre: once contratos Rust de herramientas sin sockets, dos tests Node y dos Python aprobados; cuatro tests Rust web y un test web por binding requieren un entorno con loopback permitido. El intento Node devuelve EPERM al escuchar; esos casos no se cuentan como aprobados. Un contrato unitario adicional valida rangos IP sin usar red. También pasa el intercambio de archivos Node → Python → Node junto al vault (`tests/secrets-parity.py`).
