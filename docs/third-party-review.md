# Revisión legal pendiente de dependencias (expediente)

`python scripts/third-party-notices.py --check --require-complete` bloquea el release mientras haya
paquetes sin aviso verificado. **Quedan 3 pendientes** (`rustls-platform-verifier-android` y los dos `winapi-*-gnu`);
`htmlescape` y `tiktoken-rs` están resueltos. Este documento reúne lo comprobado el 2026-11-17 para quien tome la
decisión. **No es una autorización de licencias**: la evidencia está en `third_party/review-evidence.json`.

| Paquete | Quién lo trae | ¿En binarios enviados? | Qué se comprobó | Decisión que falta |
|---|---|---|---|---|
| `htmlescape` 0.3.1 | `tantivy` 0.22 (vía `hivedb-index`), transitiva | Sí (los seis grafos) | Los 12 archivos del crate coinciden byte a byte con upstream en el commit `1699b539…` («Further improve error reporting»); la investigación anterior usó el commit `446b5a9`, por eso fallaban 2. **Upstream no contiene ningún texto de licencia, ni en ese commit ni en HEAD**; solo `Cargo.toml` declara `Apache-2.0 / MIT / MPL-2.0` (autor Viktor Dahl). No hay línea de copyright que citar. | **Resuelto el 2026-11-17 por el propietario:** la declaración del `Cargo.toml` empaquetado es el aviso (`packaged_evidence` en `upstream-sources.json`). |
| `tiktoken-rs` 0.12.1 | `hive-agent-core` (directa) | Sí | Los MIT del wrapper y de OpenAI están en el inventario (los dos accesibles en los commits fijados). Cinco assets coinciden con los hashes del script de descarga; `encoder.json` difiere en bytes pero no en valor JSON. OpenAI publica los vocabularios sin licencia propia. | **Resuelto el 2026-11-17 por el propietario:** basta el MIT de `openai/tiktoken` (ya en el inventario); queda en `resolved_reviews`. |
| `rustls-platform-verifier-android` 0.1.1 | Ninguno en las plataformas soportadas | No | Sin texto de licencia ni commit VCS; ninguna etiqueta upstream corresponde. | Excluirlo del inventario (solo Android) o dar procedencia. |
| `winapi-i686-pc-windows-gnu` 0.4.0 | Ninguno (Windows se publica con MSVC) | No | Upstream trae `LICENSE-MIT` y `LICENSE-APACHE`, pero ninguna etiqueta reproduce el archivo (mejor coincidencia ≈45 % de archivos). | Excluirlo del inventario o aceptar los textos del repositorio sin atribuir commit. |
| `winapi-x86_64-pc-windows-gnu` 0.4.0 | Ninguno | No | Igual (mejor coincidencia ≈84 %, etiqueta 0.3.6). | Igual. |

## Cómo desbloquear el gate, de menos a más permisivo
1. **Cerrar los cinco con decisión humana** y registrarla en `third_party/upstream-sources.json`
   (`manual_reviews` se elimina solo con la resolución escrita; los textos nuevos entran como
   `documents` con su SHA-256).
2. **Acotar el inventario a lo enviado**: los tres paquetes Android/Windows-GNU no están en ningún grafo
   soportado. Requiere cambiar el script para que el modo estricto mire solo los grafos de
   `review-evidence.json` (`target_graphs`); `htmlescape` y `tiktoken-rs` seguirían bloqueando.
3. **Publicar como preview**: quitar `--require-complete` de `release.yml` (línea 70); los avisos salen
   con estado «incomplete». Decisión de riesgo del propietario.
