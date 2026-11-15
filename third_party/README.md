# Inventario de avisos de terceros

`inventory.json` conserva las dependencias resueltas por Cargo.lock, expresiones declaradas, checksums de archivos .crate, fuentes Git y procedencia de los textos. Incluye conservadoramente dependencias de todos los targets y de build/dev; no identifica qué código está enlazado en cada binario. No selecciona ni reinterpreta ramas de licencias.

`notices/<sha256>.txt` contiene los documentos únicos encontrados en los archivos originales descargados por Cargo. `upstream/<sha256>.txt` contiene suplementos del repositorio upstream fijados a un commit. `upstream-sources.json` liga esos documentos a paquete, versión, fuente, checksum y commit; no hay descarga remota durante generación/check. Para r-efi el fichero AUTHORS empaquetado contiene permisos y atribución y está registrado explícitamente como evidencia.

Los avisos empaquetados se leen del archivo .crate cuyo SHA coincide con Cargo.lock, sin modificar ni confiar en copias locales editadas de sus fuentes. Los suplementos recuperan avisos de raíces de workspace que algunos crates excluyen al empaquetar. Se conservan además AUTHORS y avisos anidados de código vendorizado. Los textos no se sustituyen por una plantilla genérica ni se inventan copyright holders.

```bash
cargo fetch --locked
python3 scripts/third-party-notices.py
python3 scripts/third-party-notices.py --check
python3 scripts/third-party-notices.py --check --require-complete
```

La generación y el check son offline después de fetch. Check comprueba salidas, archivos, Cargo.lock y suplementos; `--require-complete` termina con código 2 si hay gaps/revisiones pendientes. Un cambio de dependencia exige revisar los suplementos vinculados: una entrada obsoleta o un hash distinto falla explícitamente. Los hashes se conservan entre plataformas mediante `.gitattributes`, manteniendo intactos los bytes originales de avisos.

Estado actual: **332 dependencias, 225 documentos únicos, 5 paquetes pendientes**. El estado `incomplete` aparece tanto en el catálogo como al principio de THIRD_PARTY_NOTICES.txt. La ausencia de gaps mecánicos tampoco equivaldría a una certificación de cumplimiento.

| Paquete | Pendiente |
| --- | --- |
| htmlescape 0.3.1 | No se encontró texto de aviso verificable en el paquete; no hay commit de origen en su publicación. |
| rustls-platform-verifier-android 0.1.1 | El paquete omite los avisos y no aporta commit VCS; comprobar la procedencia del componente distribuido. |
| winapi-i686-pc-windows-gnu 0.4.0 | Texto de licencia omitido, sin commit VCS en la publicación; preservar la atribución del artefacto exacto. |
| winapi-x86_64-pc-windows-gnu 0.4.0 | Mismo problema de procedencia del texto para este artefacto. |
| tiktoken-rs 0.12.1 | Incluye MIT del wrapper y MIT de OpenAI ligado al submódulo y a la fuente distribuida. Queda revisar atribución/procedencia de los assets del tokenizer. |

Los registros Android/Windows GNU pueden no estar presentes en un binario de la matriz soportada; no se eliminan de este inventario conservador. La revisión de los targets distribuidos debe distinguirlos antes de publicar. No se declara resuelta una atribución solo por tener una expresión SPDX en Cargo.toml.

La investigación adicional está en `review-evidence.json`. Los seis grafos normales/build filtrados de los bindings contienen htmlescape y tiktoken-rs y no contienen los otros tres pendientes. Es metadata, no prueba de enlace. El crate htmlescape coincide en 10 de 12 archivos con un commit upstream consultado, cuyo árbol carece de textos LICENSE. En tiktoken hay un submódulo OpenAI a commit concreto; cinco assets coinciden por SHA-256 y encoder.json tiene los mismos valores JSON con distintos bytes. Esa evidencia no cierra la revisión de assets ni cambia gates de publicación.

Para un aviso de submódulo, `via_submodule` conserva una respuesta del árbol GitHub y `.gitmodules`, ambas con SHA-256. El generador comprueba URL del árbol ligada al commit VCS del crate, ausencia de truncamiento, gitlink modo 160000 al commit del aviso, blob de `.gitmodules` y URL del repositorio hijo. También compara el blob Git de la fuente vendorizada dentro del archivo .crate verificado con su entrada en el árbol padre. El aviso original y ambos commits se incluyen en el catálogo/agregado. Un aviso hijo nunca sustituye un aviso faltante del propio paquete. Es evidencia fijada y revisable de procedencia, no una firma del proveedor ni prueba automática de derechos de assets. No se flexibilizó la regla de commit exacto de los suplementos normales.

Los suplementos exactos proceden de las raíces de jni-rs, jsonschema, napi-rs, tantivy, tiktoken-rs, simd, valuable y HiveDB. Cada texto tiene URL con commit completo y SHA-256 en el manifiesto; los Git de HiveDB se obtuvieron también desde el checkout del commit fijado. La copia de estos avisos no vende ni publica código de terceros.

El agregado y catálogo se incluyen en los previews npm/wheel. `package-native.mjs` registra hashes y estado y rechaza un paquete nativo público mientras haya pendientes. La publicación PyPI/crates y la matriz completa siguen pendientes; este inventario no habilita publicación por sí solo.
