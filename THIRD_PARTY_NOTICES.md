# Dependencias de terceros

HiveDB (`hivedb-core` y `hivedb-index`) es una dependencia Rust bajo Apache-2.0, mantenida en https://github.com/johpaz/hive-db. Esta implementación usa el commit `10fcd921d79872db881914f2e3f45bb3c20ed917` (v0.7.3), sin copiar ni modificar sus fuentes.

El validador `jsonschema` 0.58.6 y sus dependencias se enlazan desde crates.io. El proyecto del validador está en https://github.com/Stranger6667/jsonschema y declara licencia MIT. Las funcionalidades de recuperación HTTP/file están deshabilitadas en esta integración.

La licencia MIT de este SDK no reemplaza las licencias de sus dependencias. Antes de distribuir binarios públicos, generar e incluir el inventario completo de licencias y avisos de las dependencias enlazadas.

La tokenización local usa `tiktoken-rs` 0.12.1 bajo MIT, fijada en Cargo.toml y Cargo.lock. Proyecto: https://github.com/zurawiki/tiktoken-rs. Sus vocabularios cl100k_base y o200k_base están incluidos por el crate y se enlazan con los binarios; no se descargan durante la ejecución. La implementación deriva de https://github.com/openai/tiktoken (MIT). Incluir los avisos y licencias correspondientes al preparar los artefactos públicos.
