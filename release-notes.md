## Novedades de la v0.2.6

- **Borrar ya no se equivoca de resaltado**: la ficha sabe qué anotación es la suya por página y posición, no por un índice que cambiaba al reescanear o abrir otro PDF. Antes podía borrar el comentario de otra palabra.
- **Encender el motor en Ajustes funciona**: al volver a activar la revisión se arranca el sidecar de LanguageTool y el escaneo prosigue. Antes la barra se quedaba en "Cargando diccionarios…" para siempre. Al apagarlo, el proceso Java se cierra.
- **Recientes abre el documento correcto**: la ruta viaja en cada fila. Si un fichero de la lista desaparecía, se abría el de al lado.
- **Un PDF que no se puede abrir deja la app limpia**: se muestra "Sin documento" en vez de la página anterior con la barra activa.
- **Escaneo con 4 hilos fijos** en vez de uno por página: un PDF de 1.000 páginas ya no deja 1.000 hilos esperando al motor.
- **Menos código muerto**: fuera el estado invisible que ya no se pinta y la detección "automática" ES+EN que ningún botón usaba.
- **Notas de versión en un solo sitio** (`release-notes.md`), para que las dos plataformas publiquen el mismo texto.

Sigue siendo **100% offline**: ni el PDF ni tus textos salen del equipo; solo el texto de cada página va al motor local por loopback.
