# Integración con Rune — validación local, 2026-11-10

Se compilaron `hive-gateway-rust` y `hive-desktop-rune` en modo debug. El gateway usa
el motor Rust, las colecciones del proyecto Hive y una sola instancia de HiveDB.

## Comprobaciones realizadas

- Gateway: 14 pruebas aprobadas. Cubren cifrado compatible con Node, bcrypt/Argon2,
  resolución de configuración, copia con bloqueo de redb, contexto entre turnos,
  streaming, cancelación y persistencia al reabrir la base; también delegación real del motor,
  narraciones, catálogo global, agregación de ejecuciones y estados interrumpidos.
- HTTP/WebSocket: parser y handshake comprobados con transporte Tokio en memoria,
  incluyendo rechazo de Origin, tamaño excesivo y Content-Length duplicado.
- Núcleo: 18 pruebas aprobadas de sesiones, handles y handles durables.
- Rune: la suite general produjo 413 pruebas aprobadas, 71 fallidas por bloqueo
  de sockets locales del sandbox y 8 ignoradas. No se consideran aprobadas las 71.
- Rune, ciclo de vida: 3 pruebas específicas aprobadas; 2 pruebas con servidor real
  permanecen ignoradas. Se verificaron identidad/protocolo/perfil/base del servidor
  y diagnóstico temprano cuando el hijo termina durante el arranque.
- Clippy del gateway y sus targets: sin advertencias. Scripts Linux: sintaxis validada.

Las tres pruebas del gateway que requieren TCP se intentaron y fallaron con
`Operation not permitted` al reservar puertos. Después se ejecutó la suite local con:

```bash
cargo test -p hive-gateway-rust -- --skip real_http_ws --skip cancellation_and_provider --skip copied_credentials
cargo test -p hive-agent-core --test sessions --test handles --test durable_handles
```

## Narraciones y oficina: segunda entrega

Ambos binarios se recompilaron con protocolo 2. La suite de Rune pasó además las cuatro pruebas
seleccionadas por `rust_`, incluyendo las narraciones y el compositor de la oficina. El gateway
pasó sus 14 pruebas sin sockets, y Clippy no reportó advertencias.

El usuario confirmó que el chat con su proveedor real ya responde. En esta segunda entrega no se
realizaron nuevas llamadas de pago ni se modificó su perfil. La delegación se probó con el motor
real del SDK y respuestas deterministas; la ejecución visual con proveedor real queda pendiente
en este sandbox, que continúa bloqueando sockets locales.

## Historial de la copia del perfil en la primera entrega

Se preparó una copia aislada en `/tmp/hive-rune-rust-test`, con directorio 0700 y
clave maestra copiada con permisos 0600. El perfil original no se modificó.
Su coordinador tiene Gemini asignado explícitamente; la integración conserva
esa asignación aunque otro modelo figure como activo por defecto.

`--check-config` detectó que la clave maestra disponible en este entorno no logra
descifrar el secreto del proveedor de la copia. No se reemplazaron claves ni se
intentaron llamadas a un proveedor de pago. La prueba visual y con proveedor real
queda pendiente tanto por este diagnóstico como por el bloqueo de sockets.

Con Hive anterior detenido, ejecutar desde una sesión normal de escritorio:

```bash
./hive-desktop-rune/packaging/linux/run-rust-test.sh
```

El lanzador compila ambos ejecutables, crea una copia nueva, resuelve la clave
maestra existente y comprueba la configuración antes de abrir Rune. Si el descifrado
vuelve a fallar, revisar la clave maestra del perfil original antes de repetir la copia.
