# Selector del modelo de HiveLive

El campo de telemetría solo mostraba el modelo de la sesión. Se añade un selector
«Modelo de voz» con los modelos realtime de Gemini habilitados en HiveDB, cargados
al abrir HiveLive. La elección se confirma en el gateway antes de actualizar la UI.
Se guarda `active` en el catálogo original con una escritura atómica; se desactivan
las otras elecciones realtime de Gemini sin modificar los modelos de texto ni otros
proveedores. La próxima llamada resuelve la misma selección que muestra el catálogo.
No se inventan modelos ni se cambian credenciales.

El selector queda bloqueado mientras abre/está abierta una llamada y mientras guarda.
Los errores de carga/escritura se muestran con reintento; no se muestra éxito antes de
la confirmación ni se modifica una sesión de voz en curso. Las peticiones van en un
hilo de trabajo para no bloquear Rune. El catálogo se conserva al reiniciar la
telemetría de una llamada y se olvida al cerrar la sesión de usuario.

Se prefirió guardar la elección en HiveDB frente a una preferencia local o un
parámetro temporal del WebSocket para mantener una única fuente de configuración.

Validación: 34 pruebas del gateway aprobadas (tres TCP excluidas por el entorno),
incluida selección, rechazo de modelos inválidos/deshabilitados y persistencia tras
reabrir HiveDB; Clippy del gateway con `-D warnings` aprobado. Rune: tres pruebas de
HiveLive y trece de su estado aprobadas; comprobación y compilación nativa aprobadas.
No se hicieron llamadas a modelos ni se modificó el perfil del usuario.
