# Rust frente a Hive TypeScript — medición del gateway

Reporte reproducible: `evals/bench-vs-ts.json` (crudo en `evals/bench-vs-ts.json.raw`). Lo genera `scripts/bench-vs-ts.py`.

```bash
# copia neutra: sin MCP, cron ni canales externos, y con Hive detenido (no arranques el TS sobre esta carpeta: mueve su clave al llavero)
hive-gateway-rust --prepare-test-home ~/.hive /ruta/copia --neutral
cargo build --release -p hive-gateway-rust
python3 scripts/bench-vs-ts.py --profile /ruta/copia --rust target/release/hive-gateway-rust --ts ../hive \
  --runs 10 --load-seconds 3 --settle 5 --out evals/bench-vs-ts.json
```

## Qué se midió (y qué no)
Solo lo que controla el gateway: **arranque, memoria y velocidad de rutas HTTP locales**. No se midió ni se afirma nada sobre la latencia, calidad o costo de una respuesta de modelo (es del proveedor), ni la concurrencia de turnos de agente, ni los canales.

## Reglas para que la comparación sea justa
Un número solo compara si los dos lados hacen **lo mismo**. El script lo impone:

1. **Mismos datos.** El TS re-siembra herramientas, skills y modelos al arrancar, así que sirve datos distintos a los de la copia original. Se arranca una vez y **ambos lados parten del perfil que deja esa ejecución**.
2. **«Listo» = una ruta de datos real responde** (`/api/agents` con sus agentes), no `/health`. El TS contesta `/health` mientras aún inicializa: medido así, parecía arrancar en 462 ms, cuando tarda unos 646 ms.
3. **Cada ruta se clasifica antes de calcular un cociente:** *comparable* (mismo número de elementos y respuesta de tamaño parecido, ±33 %), *carga distinta* (mismos elementos pero respuesta muy distinta: se da el rendimiento y los MB/s, sin cociente), *latido* (`/health`) y *no comparable* (hacen trabajo distinto: se da cada número solo).
4. Mismo perfil neutro y aislado, mismo puerto, uno detrás de otro alternando el orden, cada ejecución desde una copia nueva. Memoria = PSS del árbol de procesos. Carga = ApacheBench con 36 clientes en 6 procesos (keep-alive; un solo `ab` se satura cerca de 15 000 req/s), 3 s por ruta, 3 repeticiones, mediana.
5. **`/api/version` no se mide**: cada llamada del TS hace una petición HTTPS real al registro público de npm (sin caché) y someterla a carga sería abusar de un servicio ajeno; la de Rust devuelve un estado local fijo. No son comparables en ningún caso.

## Resultados (Linux x64, 16 núcleos; Rust 1.100 release; TS Hive 1.1.4 sobre Bun 1.4.4)

| | Rust | TS | Relación |
|---|---|---|---|
| Listo para servir (mediana de 10) | **39 ms** | 646 ms | ~17× más rápido |
| Memoria en reposo | **29,4 MB** | 195,6 MB | ~7× menos |
| Memoria bajo carga (pico) | **126 MB** | 318 MB | ~2,5× menos |
| Distribución | 39,3 MB (un binario) | 21,1 MB (`dist/hive.js`) + 107,3 MB (runtime Bun) | sin runtime frente a con él |

### Rutas comparables (mismo trabajo y respuesta de tamaño parecido)
| Ruta | Rust | TS | Relación | Respuesta (Rust / TS) |
|---|---|---|---|---|
| `/api/agents` | 16 573 req/s | 979 req/s | Rust 16,9× | 34,4 / 39,9 KB (12 elementos) |
| `/api/tools` | 9 092 req/s | 2 196 req/s | Rust 4,1× | 30,3 / 27,7 KB (99 elementos) |
| `/api/skills` | 10 300 req/s | 1 801 req/s | Rust 5,7× | 66,8 / 63,6 KB (26 elementos) |

### Latido de la capa HTTP
| Ruta | Rust | TS | Nota |
|---|---|---|---|
| `/health` | 48 880 req/s | 21 039 req/s | JSON pequeño y fijo en ambos (426 B / 190 B): mide la capa HTTP, no la aplicación. Rust 2,3×. |

### Mismos elementos, carga distinta (sin cociente)
| Ruta | Rust | TS |
|---|---|---|
| `/api/providers` (18 proveedores) | 2 039 req/s · 77,7 KB por respuesta · 158 MB/s | 439 req/s · 41,1 KB por respuesta · 18 MB/s |

Rust devuelve 1,9× más bytes por respuesta (todos los modelos de cada proveedor y más campos), así que sirve más datos por petición y aun así atiende más peticiones por segundo. No se resume en un solo cociente porque el trabajo por petición no es el mismo.

### No comparables (cada lado hace trabajo distinto)
| Ruta | Rust | TS | Por qué no se divide |
|---|---|---|---|
| `/api/setup/status` | 6 344 req/s | 10 485 req/s | El TS devuelve `configured` y `setupMode` sin comprobar el proveedor; Rust valida coordinador, proveedor, modelo, dirección y que la clave se pueda descifrar, y explica por qué no cuando falla. |

## Lo que dice y lo que no
- **Arranque, memoria y huella:** la ganancia clara y la que no depende de cómo se compare una ruta. La memoria bajo carga de Rust es mayor que en una medición anterior porque ahora atiende en paralelo de verdad (ver abajo); sigue siendo menos de la mitad que la del TS.
- **Rutas:** en las tres comparables Rust rinde de 4 a 17 veces más. En las demás no hay una conclusión honesta de «quién es más rápido»: hacen cosas distintas.
- El arranque del TS incluye el resembrado de herramientas, skills y modelos que Rust no hace: es lo que ve la persona, pero no es «el mismo trabajo». Se midió con el perfil ya sembrado, y el TS sigue resembrando los modelos en cada arranque.

## Mejoras que esta medición encontró en el gateway Rust (corregidas, con prueba)
Mirar con `gdb` el proceso real bajo carga dio cuatro hallazgos:

1. **Un candado global serializaba toda la API de gestión, incluidas las lecturas** (`activity_lock`, un `Mutex`): con 16 núcleos el gateway trabajaba como con uno. Ahora es un `RwLock`: las lecturas corren en paralelo y los cambios siguen siendo exclusivos (una lectura nunca ve un cambio a medias). `/api/agents` pasó de 4 672 a ~16 500 req/s, `/api/tools` de 2 402 a ~9 000 y `/api/skills` de 2 968 a ~10 000 (comparación de Rust consigo mismo, antes y después).
2. **La clave maestra se buscaba en el llavero del sistema (D-Bus) en cada lectura de un secreto** (~5 ms cada vez; cada turno de chat descifra varias). Ahora se conserva por perfil durante la vida del proceso.
3. **`/api/tools` reconstruía la lista de herramientas del runtime una vez por fila** (873 → ~2 400 req/s al corregirlo).
4. **`/api/setup/status` preparaba un turno de chat completo** solo para decir si hay configuración. Ahora hace una comprobación propia (`App::configured`) con el mismo veredicto y los mismos mensajes (lo comprueba una prueba con nueve configuraciones rotas): 192 → ~6 000 req/s.

## Pendiente
- Con 16 hilos el escalado ya no es lineal (la lectura de colecciones grandes comparte caché de la base de datos); no se investigó más.
- El gateway cierra la conexión tras cada respuesta (`Connection: close`) y lee la cabecera byte a byte. En local el coste fijo medido es ínfimo (40–48 000 req/s en rutas ligeras), así que no se tocó.

## Límites de esta medición
- Una sola máquina y un solo sistema; no hay macOS, Windows ni ARM.
- `ab` da la latencia en milisegundos enteros: se informa el rendimiento; la p99 es gruesa.
- Sin medir: CPU, crecimiento de memoria en horas, concurrencia de turnos con un modelo de latencia fija, canales y WebSocket.
