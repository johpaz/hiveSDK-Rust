# Hive SDK Rust — Python (preview)

Binding PyO3 con API asyncio, tools y proveedores Python. Python >=3.10.

El desarrollo local puede compilar ambos bindings con `python3 scripts/build-dev.py` desde la raíz. Añadir `packages/python/python` a `PYTHONPATH` para importar `hive_sdk`.

Para construir wheels: `maturin build --release` desde esta carpeta, con Maturin instalado. La distribución multiplataforma y publicación PyPI siguen pendientes. Ver el README raíz para ejemplos y limitaciones.
