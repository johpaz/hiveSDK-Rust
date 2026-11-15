# Navegador CDP nativo

El feature Rust `browser` añade un cliente CDP por WebSocket. Los bindings npm/Python lo incluyen y exponen Browser. El navegador ejecuta su propio motor; el control y transporte del SDK están en Rust, sin procesos auxiliares Node/Bun ni Playwright.

La aplicación arranca su Chromium y obtiene la URL **de una página** desde el endpoint de depuración `/json/list` o `/json/new`. El SDK se conecta a esa URL ws/wss. La URL del navegador `/devtools/browser/...` no sustituye a la URL de página `/devtools/page/...`.

```javascript
const browser = await Browser.connect(pageWebSocketUrl, { timeoutMs: 10000 });
try {
  await browser.navigate('https://example.com');
  await browser.typeText('#search', 'Hive');
  await browser.click('button[type=submit]');
  console.log(await browser.extract('body'));
  const { data } = await browser.screenshot(); // PNG en base64
} finally { await browser.close(); }
```

Python ofrece `await Browser.connect(url, timeout_ms=10000)`, `type_text` y los mismos métodos async navigate/evaluate/click/extract/screenshot/close. Admite async with. Rust ofrece Browser::connect y métodos equivalentes; dispatch comparte la traducción de comandos de ambos bindings.

El host conserva propiedad del proceso, perfil y pestaña. `close` cierra la conexión, sin terminar el navegador. `navigate` confirma Page.navigate, no la carga completa ni la disponibilidad de selectores: el consumidor debe esperar explícitamente antes de interactuar. Click usa DOM click y type_text asigna value y emite input/change; no simulan todos los eventos de teclado/ratón ni sustituyen computer use.

Hay una operación CDP activa por conexión, frames/respuestas de hasta 8 MiB, comandos de hasta 1 MiB y timeout configurable de 1–120 000 ms. Timeout, cancelación o errores de transporte/protocolo descartan la conexión para evitar repetir acciones inciertas. `cancel` cancela la conexión completa y requiere reconectar explícitamente. JavaScript expone cancel; Python también cancela el transporte al cancelar una tarea activa y espera al worker antes de devolver el control.

## Validación

Pruebas locales: respuestas/eventos CDP, error de script, timeout sin replay, cancelación y validación de URL en bindings. El test de Chromium real está disponible explícitamente:

```bash
HIVE_BROWSER_TEST_EXECUTABLE=/path/to/chromium cargo test -p hive-agent-core --features browser --test browser native_chromium -- --ignored
```

Crea un perfil temporal y una página HTTP local; comprueba navegación, escritura Unicode, click, extracción y screenshot. Usa --no-sandbox únicamente en ese fixture aislado de pruebas. En el entorno de esta entrega Chromium/headless-shell abortaron por syscalls de sockets no permitidas. Esta prueba no se cuenta como aprobada. No hay todavía lanzamiento automático, herramientas de navegador integradas al catálogo ni backend OS de computer use.
