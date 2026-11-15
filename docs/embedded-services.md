# Servicios embebidos de catálogo

El feature Rust `services` añade servicios sobre el mismo handle HiveDB de `SessionStore`, sin abrir un servidor ni volver a abrir los archivos. Los bindings npm/Python incluyen este feature. El motor Rust mínimo conserva `default = []`.

```javascript
const store = await SessionStore.open({ path: '/tmp/my-hive', tenantId: 'customer-a' });
const memory = store.services.collection('memory');
const row = await memory.create('delivery', { title: 'Entrega', content: 'Viernes' });
await memory.update(row.id, row.revision, { content: 'Sábado' });
const hits = await memory.search('sábado');
await store.closeAsync();
```

```python
store = await SessionStore.open('/tmp/my-hive', tenant_id='customer-a')
memory = store.services.collection('memory')
row = await memory.create('delivery', {'title': 'Entrega', 'content': 'Viernes'})
await memory.update(row['id'], row['revision'], {'content': 'Sábado'})
hits = await memory.search('sábado')
await store.close_async()
```

## Contrato

`collection(kind)` admite `agents`, `skills`, `tools`, `memory`, `providers`, `models`, `ethics`, `mcp_servers`, `endpoints` y `cron`. Los schemas autoritativos se encuentran en `hive_agent_core::services::schema`; npm declara sus tipos en `index.d.ts`.

Las operaciones son create/get/list/update/delete/setEnabled/search (snake_case en Python). Cada registro contiene `id`, `kind`, `revision` y `data`. Update y delete requieren la revisión leída; `CATALOG_CONFLICT` exige releer y decidir cómo resolver. No hay reintentos ocultos.

Las claves codifican `[tenant, kind, id]` como JSON. Listados, cursores, referencias y búsqueda permanecen dentro del ámbito. El host debe autorizar al tenant antes de entregar su store: `forTenant` es una capacidad de la aplicación, no una barrera contra código anfitrión malicioso. Las operaciones nativas liberan el hilo del lenguaje y las rutas async conservan los guards de mantenimiento existentes.

Los documentos no admiten campos desconocidos ni credenciales inline. Los proveedores/endpoints contienen URLs HTTP(S) sin credenciales, query ni fragmento. No se implementa todavía un servicio de secretos embebido. Los registros no se importan de colecciones legacy del gateway; ese adaptador conserva sus datos y reutiliza la primitiva compartida de patch versionado.

Las referencias de agentes a tools, skills, servidores MCP y proveedor/modelo se validan al guardar. No hay claves foráneas transaccionales: deshabilitar o borrar posteriormente una capacidad puede invalidar una referencia y el futuro ejecutor deberá volver a comprobarla. Un registro de tool no certifica que haya un ejecutor instalado. Tampoco crear un cron conecta un scheduler.

Los borrados son tombstones con CAS. Recrear el mismo ID conserva una revisión creciente; una escritura anterior al borrado no puede sobrescribir el registro recreado. No hay purga automática. Los listados admiten 1–200 elementos y `next_cursor`; omitir el cursor inicia un recorrido nuevo. No son snapshots transaccionales entre páginas.

La búsqueda BM25 local devuelve `{record, score}`, omite deshabilitados y ordena empates por ID. Está limitada a 10 000 registros y 16 MiB por consulta, con queries de hasta 4096 bytes y 64 términos distintos. No reproduce el ranking ni todas las búsquedas por subcadena de TS. Los documentos tienen un techo de 256 KiB y los campos de texto principales, 65 536 caracteres.

## Tools e importación de skills

`await store.services.createTools()` / `await store.services.create_tools()` devuelve ocho tools con ejecutores Rust: memory_write/read/list/search/delete, skill_search, tool_search y agent_find. Se pasan al constructor de Agent como cualquier tool. El ciclo del agente mantiene claims, ledger, validación y presupuestos; el servicio no hace inferencias.

`importSkill(path)` / `import_skill(path)` lee un archivo SKILL.md o una carpeta que lo contenga. Requiere frontmatter YAML válido con name/description, conserva metadatos de workflow y cuerpo, rechaza claves repetidas y archivos mayores de 256 KiB, y valida las tools referenciadas antes de crear el registro. La importación es explícita; no ejecuta pasos ni instala dependencias. No hay todavía bundle adaptado ni hot reload.

## MCP y HiveAgents

McpClient expone listResources/readResource/listPrompts/getPrompt (snake_case en Python). Los listados devuelven páginas y un nextCursor opcional; el consumidor solicita la página siguiente explícitamente. Resources/prompts no se inyectan en contexto ni se ejecutan automáticamente. Las operaciones respetan capacidades anunciadas, tamaño, timeout, cancelación y una solicitud activa por conexión. Resultados no confirmados cierran la sesión sin repetirlos.

HiveAgents está expuesto también en ambos bindings y utiliza el adaptador Rust existente: estado/carga del GGUF, alias local para inferencia y lease por backend. Requiere baseUrl/base_url; admite contextWindow/context_window. No añade nuevos reintentos de inferencia.
