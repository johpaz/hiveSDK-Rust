# Secretos embebidos

El feature Rust `services` ofrece un vault cifrado en HiveDB con APIs asíncronas en Node y Python. No requiere gateway. El host entrega una clave de **32 bytes aleatorios**, un identificador público de clave y el mismo tenant usado por el `SessionStore`. Debe conservar esa clave fuera de la base y recuperarla al reabrir. No hay generación automática, fallback a variables de entorno, archivo ni llavero del sistema.

```js
// masterKey se obtiene del gestor de claves de la aplicación.
const vault = await store.openSecrets({ key: masterKey, keyId: 'app-key-v1' });
try {
  const meta = await vault.put('provider-key', suppliedToken); // revisión 0: crear
  const reference = { vault_id: 'default', name: meta.name };
  // Solo reference se guarda como credential_ref en providers/endpoints/mcp_servers.
  const value = await vault.reveal(reference.name); // acción explícita del host
} finally {
  await vault.close(); // antes de cerrar o mantener el store
}
```

```python
vault = await store.open_secrets(key=master_key, key_id='app-key-v1')
try:
    meta = await vault.put('provider-key', supplied_token)
    await vault.put(meta['name'], replacement_token, revision=meta['revision'])
    value = await vault.reveal(meta['name'])
finally:
    await vault.close()
```

`SecretVault.open` también está disponible. Rust usa `SessionStore::open_secret_vault`, `SecretKey` y `SecretValue::expose`. La clave no cruza el dispatcher JSON: los bindings reciben Buffer/Uint8Array o bytes. El vault abierto retiene su store; cerrarlo con operaciones pendientes devuelve `BUSY`. El cierre es idempotente y la cancelación Python espera la limpieza nativa.

## Contrato

- AES-256-GCM mediante ring, nonce aleatorio de 96 bits. El contexto autenticado liga tenant, vault, nombre, revisión, etiqueta de clave, estado de borrado y fechas; el ancla del vault también se autentica, incluso cuando está vacío.
- `info`, `inspect` y `list` devuelven metadatos; `reveal` devuelve texto o null/None. No hay tool de revelado registrada automáticamente, ni valores en el catálogo o en Debug Rust. La aplicación decide cuándo entregar un valor a una integración autorizada.
- `put` exige revisión 0 para crear o la revisión actual para modificar. `delete` exige la revisión actual y guarda un tombstone cifrado. Recrear exige la revisión del tombstone; una escritura antigua falla con `SECRET_CONFLICT`.
- Cada valor es UTF-8 de hasta 64 KiB. Hay hasta 1000 registros retenidos por vault, incluidos tombstones. `list` acepta páginas de 1 a 200 y cursores ligados al ámbito. No hay purga automática.
- Cada época de clave permite hasta un millón de cifrados confirmados contabilizados por el vault. Al alcanzar el límite, las mutaciones exigen rotación; leer continúa permitido. Las operaciones CAS fallidas también pueden haber generado ciphertext temporal: el contador no representa todos los usos históricos de una clave reutilizada por el host.

`rekey` exige otra etiqueta y material distinto. Debe usarse una clave nueva aleatoria, conservada por el host antes de iniciar. Reenvuelve todos los registros, incluidos tombstones, e incrementa sus revisiones en una sola transacción CAS junto al ancla. Una escritura concurrente provoca el reintento completo. La respuesta describe esa transacción; los handles con la clave anterior fallan en su siguiente operación. Una lectura ya iniciada puede terminar con el valor anterior. Si la respuesta se pierde tras el commit, el host debe comprobar explícitamente qué clave abre el vault, sin repetir escrituras de forma ciega.

## Integración y límites

`credential_ref: { vault_id, name }` es una referencia descriptiva en providers, endpoints y MCP servers del catálogo; no puede contener tenant ni credenciales inline. No comprueba existencia ni conecta automáticamente los ejecutores con el vault. Quedan pendientes resolución autorizada por integración, permisos más finos y adaptadores compartidos de aprovisionamiento de claves.

El gateway reutiliza el codec nativo para su formato existente; conserva su propio aprovisionamiento de clave/perfil. No importa ni migra esos secretos a los vaults embebidos.

Se limpian los buffers propios de clave/texto cuando es posible. No se garantiza borrado de todas las copias de memoria del proceso, claves expandidas de ring o strings de JavaScript/Python. Quien obtiene la clave puede revelar los valores. Las copias de seguridad y páginas antiguas pueden retener ciphertext previo; la rotación no es borrado seguro y no detecta la restauración íntegra de una base antigua. No reutilizar material de claves históricas: no se mantiene un registro global de claves usadas.

Pruebas: nueve contratos Rust del vault, validación de referencias, tres tests Node y cuatro Python; `tests/secrets-parity.py` comprueba commit seguido de salida abrupta de Node, reapertura y rotación Python y verificación Node. Todo usa claves ficticias y directorios temporales, sin perfiles ni cuentas reales. No constituye auditoría criptográfica ni paridad completa con los servicios TS.
