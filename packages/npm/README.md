# Hive SDK Rust — npm (preview)

Binding nativo Rust con tools/proveedores JavaScript y TypeScript. Node >=24; no requiere Bun.

Desde esta carpeta: `npm run build:native` y `npm test`. Para compilar ambos bindings de desarrollo, ejecutar `python3 scripts/build-dev.py` desde la raíz del proyecto.

El build local usa Rust. Antes de publicar se distribuirán binarios por plataforma; actualmente el paquete tiene `private: true`. Ver el README raíz para ejemplos, limitaciones y plan de distribución.
