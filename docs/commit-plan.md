# Plan de commits (propuesta, sin ejecutar)

El árbol tiene el trabajo de varias funciones mezcladas, y **varios archivos pertenecen a más de una** (`server.rs`, `server/tests.rs`, `config.rs`, `channels.rs`, `lib.rs` y la mayoría de `docs/`). Separar por función exigiría `git add -p` hunk a hunk; separar **por capas**, en este orden, deja cada commit compilando y con sus pruebas:

| # | Capa | Archivos | Mensaje sugerido |
|---|---|---|---|
| 1 | Avisos de terceros | `THIRD_PARTY_NOTICES.*`, `third_party/**`, `packages/*/THIRD_PARTY_*` | `chore: regenerar el inventario de avisos de terceros` |
| 2 | Núcleo del SDK | `crates/hive-core/src/**`, `crates/hive-core/Cargo.toml`, `crates/hive-core/examples/**`, `Cargo.toml`, `Cargo.lock` | `feat(core): SSE MCP, computer use, imágenes de herramientas, Ollama nativo, revisor del oráculo, Excel/Word/PPT y tope de salida derivado` |
| 3 | Pruebas del núcleo | `crates/hive-core/tests/**`, `tests/**`, `scripts/make-xlsx-date-fixtures.py` | `test(core): pruebas de las funciones anteriores` |
| 4 | Bindings | `crates/hive-napi/**`, `crates/hive-python/**`, `packages/**` | `feat(bindings): Ollama, contextWindow, controles de computer use y transporte sse` |
| 5 | Gateway | `crates/hive-gateway/**` (incluye el catálogo) | `feat(gateway): MCP, computer use, oráculo, Slack, imágenes en canales, Ollama nativo y ventanas de contexto` |
| 6 | Documentación | `docs/**`, `PLAN.md`, `README.md` | `docs: …` |
| 7 | Scripts y CI | `scripts/**` (resto), `.github/**` | `ci: pruebas externas, muestras de Office y CI` |
| — | Repositorio del escritorio (aparte; aún sin ningún commit) | todo el repo: pestañas OCR, Control del equipo y Oráculo (`src/ocr.rs`, `computer.rs`, `oracle.rs`, `remote.rs`), prueba de contrato real y clippy limpio | `feat(desktop): primer commit con las pestañas OCR, Control del equipo y Oráculo y la prueba de contrato con el gateway real` |

Comandos (revisa `git status` antes; no se ha ejecutado nada):

```bash
git add THIRD_PARTY_NOTICES.* third_party packages/*/THIRD_PARTY_* && git commit
git add Cargo.toml Cargo.lock crates/hive-core/src crates/hive-core/Cargo.toml crates/hive-core/examples && git commit
git add crates/hive-core/tests tests scripts/make-xlsx-date-fixtures.py && git commit
git add crates/hive-napi crates/hive-python packages && git commit
git add crates/hive-gateway && git commit
git add docs PLAN.md README.md && git commit
git add scripts .github && git commit
```

Notas:
- Tras el commit 2 y antes del 3, `cargo build` ya compila; las pruebas llegan en el 3. Los commits 4 y 5 dependen del 2.
- `Cargo.lock` cambió por `core-graphics`, `windows-sys` (solo en sus plataformas), `x11rb` y `zbus`; va con el núcleo.
- Si prefieres historial por función, usa `git add -p` en `server.rs`, `server/tests.rs`, `config.rs`, `channels.rs` y `lib.rs`; el resto de archivos sí es de una sola función.

- El commit 7 incluye ahora `scripts/package-gateway.py` y `.github/workflows/gateway.yml` (binarios del gateway; ver `docs/gateway-binary.md`). Los docs nuevos (`ocr.md`, `gateway-binary.md`) van en el 6.
