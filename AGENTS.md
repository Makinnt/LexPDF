# AGENTS.md — lexpdf

Corrector ortográfico de escritorio, **100% offline** (sin red ni telemetría).
Abre PDFs, extrae texto con coordenadas, revisa ortografía ES/EN en segundo plano,
subraya errores sobre la página y permite omitir + inyectar notas nativas.

Stack: **Rust 2021**, Slint (UI declarativa), `pdfium-render` 0.8 (PDFium),
LanguageTool 6.x como sidecar Java local (ES+EN reales: ortografía por
categoría TYPOS + sintaxis) + `ureq` 3 sync.
`zspell` y `nlprule` se probaron y descartaron (ES débil en ambos).

## Estructura

```
Cargo.toml      # ureq (HTTP sync al sidecar) + serde_json (/v2/check)
build.rs        # compila ui/app.slint + copia libpdfium junto al exe + descarga LT a assets/lt/
src/main.rs     # TODO el código Rust (un solo binario a propósito)
ui/app.slint    # TODO el UI (controles propios, sin std-widgets)
assets/pdfium/  # libpdfium.so + pdfium.dll + LICENSE.pdfium + licenses/ + VERSION.pdfium
assets/icons/   # licencia Apache-2.0 de Material Icons (glifos incrustados en ui/app.slint)
assets/lt/      # LanguageTool desempaquetado (gitignoreado, lo baja build.rs)
input/          # PDFs reales de prueba (El-viejo-y-el-mar.pdf)
```

## Comandos

```bash
cargo build                      # debug (~400MB por Slint, normal)
cargo test --bin lexpdf   # 25 tests (coordenadas, subrayado, kinds, anotación real)
cargo run -- /ruta/doc.pdf       # argv[1] abre directo, sin diálogo (útil para demos)
PDF_DARK=1 cargo run -- doc.pdf  # arranca en tema oscuro
PDFIUM_DYNAMIC_LIB_PATH=...      # alternativa a tener la lib junto al exe
```

`build.rs` copia `assets/pdfium/{libpdfium.so,pdfium.dll}` (según SO) y
descarga+descomprime LT a `assets/lt/` (gitignoreado, ~240MB zip).
En runtime LT se busca en: `$LT_HOME` → junto al exe → `assets/lt/`.
Java 17+ requerido (sidecar). Los `.tgz`/`.zip` originales **no** se commitean.

## Arquitectura lógica (`src/main.rs`)

Estado central: `struct State` en `Rc<RefCell<State>>` (hilo UI).

- `pdfium, path, page, total` — documento actual (se **reabre por página**,
  evita líos de lifetimes `Pdfium/PdfDocument`; cachear si va lento).
- `words: Vec<Word>` — palabras de la página visible (`text + x0,y0,x1,y1` en
  puntos PDF, origen abajo-izq).
- `all_errs: Vec<Misspelling>` — hallazgos de TODO el doc
  (`word, sug, page, box, dismissed, kind, sev`).
- `view: Vec<usize>` — índices a `all_errs` tras los filtros. Los handlers
  de UI reciben índice de **fila** → mapear por `view` antes de tocar `all_errs`.
- `scan_queue/scan_pos`, `scan_id`, `flash`, `lt_ready/lt_dead/lt_spawned`,
  `last_lang`, `tx` (eventos a la UI), `job_tx` (páginas a los 4 hilos LT).

Flujos:

1. `show()` — renderiza la página (`PdfRenderConfig` 1400px·zoom) a `slint::Image`,
   extrae palabras y **quema subrayados por clase** (rojo ortografía, morado
   sintaxis) + **wash amarillo** (flash) en el bitmap. No toca el escaneo.
2. `start_scan()` — encola todas las páginas (desde la visible). **No usa hilos.**
3. `scan_tick()` (Timer 250ms, presupuesto 80ms/tick) — extrae palabras por
   página y las manda por `job_tx` a **4 hilos trabajadores fijos** (HTTP al
   sidecar LT); la barra (`scan-progress`, `scan-text` con %) mide páginas
   extraídas. Sin LT no avanza (salvo `lt_dead`, que vacía la cola). Los
   `Syn(gen)` re-subrayan si tocan la página visible.
4. Sidecar LT (`start_lt()`: hijo java en un puerto libre, o servidor externo;
   `LtReady/LtFail` con `scan_id`): `syntax_page()` mapea offsets en chars → box
   unión (`join_words`/`span_to_box`), clase por categoría (`TYPOS`=Ortografía)
   y `sev` (GRAMMAR=grave…). `start_lt` es idempotente (`lt_spawned`); al apagar
   el motor en Ajustes se mata el hijo, y al encenderlo se vuelve a llamar.
5. `SpellEvent = LtReady | LtFail | Syn(gen, hits)`. `gen == scan_id` invalida
   respuestas viejas. Cambio de idioma se detecta en el Timer (`lang-idx !=
   last_lang` → `start_scan`). **Nunca Pdfium en hilos** (se cuelga en `bind`).
6. `annotate()` — sticky-note nativa (`create_text_annotation` + `set_bounds`),
   acumulativa sobre `{nombre}_anotado.pdf` (vía temporal+rename). **Jamás toca el original.**
7. Tipos y severidad: `kind` Ortografía|Sintaxis, `sev` mínima|intermedia|grave.

## UI (`ui/app.slint`)

- Paleta propia `c-bg/c-fg/...` según `is-dark`. **Todo `Text` lleva color explícito**:
  el texto por defecto de Slint sigue el tema del SO (blanco en GNOME oscuro →
  invisible sobre sidebar blanco). Controles 100% propios (`MiniBtn`,
  `Rectangle+Text+TouchArea`); `std-widgets` se eliminó porque salía en blanco.
- **GOTCHA TouchArea**: (1) un `TouchArea` declarado después del contenido queda
  **encima y se traga los clics** (esto rompió Omitir/Nota una vez); los botones
  van en filas separadas sin cobertura exterior. (2) Todo `TouchArea` lleva
  `width/height: parent` explícito.
- `TextInput` con `accepted =>` guarda con Enter si hay texto. No existe
  `multi-line` en esta versión de Slint (solo una línea).
- **GOTCHA `Text` dentro de `Rectangle`**: `Text` es `@implicit_size`, así que sin
  `width/height` explícitos su caja se ajusta al texto y `horizontal-alignment:
  center` **no hace nada** (la etiqueta queda pegada a la izquierda/arriba de la
  pastilla). Todo texto de botón lleva `width: parent.width; height: parent.height;`.
- Iconos = **Material Icons Outlined 24px** (Apache-2.0) como `Path { commands: ... }`
  (path SVG tal cual, `viewBox 0 0 24 24`; `fill: root.c-fg` para que siga el tema).
  Componentes `Icon*` al tope de `ui/app.slint`; licencia en `assets/icons/`.
- **Anotaciones**: `Note` (sticky, botón "Nota") y `Highlight` (resaltado + comentario).
  Los resaltados se crean con pdfium (rect + `/Contents`) y **lopdf parchea el
  fichero** para añadir `/QuadPoints` y `/CA 0.4`: pdfium-render no expone
  `set_quad_points` y sin ellos **el resaltado no se ve en ningún lector**.
  Gotchas de lopdf: las páginas van numeradas desde 1, pdfium guarda las
  anotaciones *inline* en `/Annots` (hay que reescribir el array) y
  `Object::get` choca con el prelude de pdfium (usar `Object::Dictionary(d)`).
  El parcheo se escribe a un temporal y se renombra (pdfium tiene el fichero
  mapeado: reescribirlo in situ lo rompe).
  **GOTCHA `/QuadPoints` = orden del SPEC (UL, UR, LL, LR)**: con el orden
  inverso (LL, LR, UR, UL) pdfium pinta **27 px (un punto)** en vez de la
  palabra entera (1662 px) — medido contando píxeles. El test lo comprueba.
- **Herramientas** (barra superior): 0 normal · 1 nota · 2 resaltar (arrastrar) ·
  3 borrar. El borrado se puede hacer desde la barra (pinchar el marcador) o
  desde la card del error (botón "Borrar", solo si tiene anotación); el aviso
  al entrar en modo borrar recuerda que se sale pulsando el icono otra vez.
  El campo de zoom va con `text <=> root.zoom-label` (en un solo sentido se
  queda con el texto viejo al cambiar el zoom con rueda o lupas) y hace
  `clear-focus()` al aceptar.
  **GOTCHA**: en modo resaltar hay que poner `interactive: false` al `Flickable`
  de la página, si no el scroll se come el arrastre del rectángulo.
  El resaltado libre puede ser **por área** (el rectángulo) o **por texto**
  (`free_sel=1`). Con `free_sel=1` hay una sub-opción (`free_text`): 0 = por
  recuadro (`words_in_rect()`, una anotación por palabra) · 1 = selección real
  de texto (`words_in_lines()`, una anotación continua por línea). El ajuste
  nuevo es `free_text` en `settings.json` (con `#[serde(default)]` para que los
  ficheros de ajustes viejos sigan leyéndose). Por defecto `free_text=1`
  (selección de texto). El panel muestra el texto seleccionado de verdad
  (`text_in_rect()`, una línea, se corta a 80 caracteres). El texto del
  comentario se recuerda: `free-note-text` no se borra al empezar otra
  selección (solo al abrir la app).
  **GOTCHA layout**: un `Text` con `wrap` y **alto implícito** dentro del
  `VerticalLayout` del panel hace que el layout reparta el espacio y el panel
  se descuadre (el input y los botones acaban fuera de la ventana): hay que
  darle `height:` explícito.
- **Ajustes** (capa de la ventana principal, no ventana aparte): color del
  resaltado, copia de trabajo (preguntar/siempre/nunca), motor de revisión on/off
  y botón "Acerca de". El ⚙ vive en la CABECERA DE LA BARRA LATERAL (visible
  sin documento; se quitó de la toolbar del documento). Añadir sin documento es
  trivial ahora porque `on_open_settings` ya no crea nada: solo rellena los
  props `p-*` y pone `ovl = 1`.
  Se guardan en `~/.config/lexpdf/settings.json` (0600).
  **GOTCHA layout**: el contenido va en un `Flickable` y la fila
  Guardar/Cerrar queda **fija al pie** (fuera del Flickable): si los botones
  van dentro del contenido, con el panel corto se salen de la ventana.
- **Capas apiladas Ajustes > Acerca de > Licencias** (todas DENTRO de la
  ventana principal): Slint 1.18 no tiene ventanas modales. AppWindow lleva
  `ovl` (0 nada · 1 Ajustes · 2 Acerca · 3 Licencias); cada capa es un `if
  (root.ovl >= N)` con velo `@radial-gradient` + `TouchArea` que se come los
  clics de lo de abajo + panel centrado. Cierre en cascada: Licencias y Acerca
  tienen su botón "Cerrar" (→ `ovl = N-1`) y el de Ajustes está junto a
  "Guardar" (`ovl = 0`). `parent` NO existe en la raíz de un componente: el
  alto de cada panel se pone al instanciar (`height: min(860px, parent.height
  - 48px)`). El `AboutPanel` muestra `root.version` vía `ver <=> root.version`
  y su texto es `"versión " + root.ver`; el autor va hardcodeado en Slint.
  **GOTCHA tema**: los componentes de capa no ven la paleta de la AppWindow →
  la paleta vive en el **global `Pal`** (visible desde cualquier componente);
  la AppWindow aliasa sus `c-*` a `Pal.c-*` y enlaza `is-dark <=> Pal.is-dark`.
  Si metes un color nuevo en una capa, usa `Pal.*` (no lo escribas fijo o se
  queda oscuro en tema claro).
- **Licencias** (capa, ya no ventana): lista de dependencias, el texto REAL de
  cada repo (bajado una vez a `assets/licenses/<dep>-<lic>.txt` y commiteado:
  la app sigue offline; nada de textos genéricos compartidos — el MIT de ureq
  lleva su copyright y el de lopdf el suyo; el test
  `los_textos_de_licencia_son_los_correctos` + `every_listed_license_file_exists`
  lo garantizan) y el repo (clicable → `xdg-open`/`open`/`cmd start`). OJO con
  los repos reales: pdfium-render=ajrcarey, lopdf=J-F-Liu, rfd=PolyMeilex.
  Donde la dependencia es dual (MIT **o** Apache-2.0) se **elige MIT** y se
  lista solo ese texto: no vale dejar "MIT o Apache-2.0" sin elegir.
  Los datos los alimenta la función Rust `lic_select()` y el
  callback `open-licenses()` de AppWindow. `asset_file()` los busca junto al
  exe (`licenses/`), en el AppImage (`../share/lexpdf/licenses/`) o en el repo
  del proyecto. **GOTCHA packaging**: el `release.yml` de Windows tenía que
  copiar `assets/licenses` él mismo (el `stage.ps1` no lo llama el CI) y el
  `.iss` lleva `licenses\*` a `{app}\licenses`: si falta, TODAS las entradas
  muestran "no se encontró", incluida la GPL de la app.
  **GOTCHA**: el texto de la licencia necesita un alto explícito
  (`cur-h`, estimado en Rust con `license_height()`): si el `Flickable` calcula
  el alto del contenido solo, el texto se sale de la ventana y no se desliza.
- **GOTCHA slint#407 (cards)**: `Flickable` no calcula `content-height`
  cuando el contenido es un `for`; el pase `flickable.rs` lo deriva del
  **`min-height` del layout hijo** (y `layout.rs` deriva `min-height` de un
  `height` local, así que fijarlo a un número equivale a mentirle al scroll).
  Solución: **no poner `height` ni `min-height` al `VerticalLayout`** — su
  `min-height` automática es la suma de las cards y acierta sola. Las cards
  miden **alto natural** y llevan `vertical-stretch: 0` (sin él, con pocos
  resultados se estiran hasta llenar el Flickable). Antes había una estimación
  en Rust (`errors-h`/`card_h()`, +30%): se eliminó porque en Windows se
  quedaba corta (última card inalcanzable).
- **GOTCHA botones de la card**: `visible: false` **y también un `if`** dejan
  la columna reservada (el botón que queda se estira a medias → hueco raro) y
  con 3-4 `MiniBtn` en una fila el layout los encoge y sale "Añadir comen…".
  La card usa 2x2 fijo (Omitir+Añadir comentario / Obs. gramatical+Borrar) y
  el "Borrar" sin anotación se pinta con `opacity: 0.4` y no hace nada.
  Todos los MiniBtn de la card (y el de Agrupar/Desagrupar) llevan
  `width: 169px` (media card menos spacing): con solo `stretch: 1` el base es
  el ancho PREFERIDO del texto y las columnas salían desiguales.
  En el pie de Ajustes, "Guardar" necesita `width` explícito (90px): sin él,
  el Rectangle se queda en su ancho preferido (~0, su Text usa parent.width)
  y el botón salía aplastado.
  **GOTCHA ancho**: un layout dentro de un `Rectangle` NO hereda su ancho
  (se queda en el preferido y TODO sale elidido: "Añadir come…", "…"). La
  sidebar mide 400 fijos, así que la card va a **`width: 360px`**, el
  `HorizontalLayout` de dentro a **360px** y el `VerticalLayout` a **355px**
  (5px de la franja). Con `width: parent.width` + `Flickable` hay binding
  loop (`content-width`), por eso son números.
  Alturas con holgura para las fuentes de Windows: palabra 24px, meta 19px,
  leyenda 36px (2 líneas), sugerencia 19px, `MiniBtn` 25px (con 22 se
  cortaba la parte de arriba).
- **Toasts/avisos**: la pastilla usa `toast-pos` (0 arriba por defecto ·
  1 abajo, seleccionable en Ajustes, campo `Settings.toast_pos`). La ✕ es el
  glifo `IconClose` de Material Icons: el carácter "✕" en Text NO existe en
  las fuentes base de Windows y no se dibuja.
- **Icono de ventana/corner**: ya está en `AppWindow`:
  `icon: @image-url("../assets/logo.png")` (el backend winit llama
  set_window_icon + set_taskbar_icon). Windows no lo toma del recurso del exe
  por su cuenta (winit registra la clase con `hIcon: 0`), así que **sin esa
  propiedad la esquina sale en blanco**. El `.ico` de `winresource` en
  build.rs solo sirve para Explorer/accesos directos.
- **Borrar se busca en `doc_notes`** (página + rect, helper `anno_rect`), no en
  un mapa paralelo: el índice de `all_errs` se reusa al reescanear o abrir otro
  PDF y el "Borrar" de la card acababa borrando la anotación de otra palabra.
  `doc_notes` se limpia al abrir y se mantiene al borrar, así que es la única
  fuente de verdad.
- **GOTCHA foco automático**: el editor de la card va en un bloque `if` (no
  `visible:`), así los elementos se crean al abrirlo y el `TextInput` puede
  hacer `init => { self.focus(); }` (se escribe sin tocar la caja). El
  placeholder se pinta con un `Text` detrás (Slint 1.18 no tiene
  `placeholder-text`).
- **GOTCHA `Tooltip`**: el `Tooltip` tiene que ir **dentro** del `TouchArea`
  (si cuelga del `Rectangle` padre, el hover lo tiene el hijo y el tooltip no
  sale nunca). El tooltip es una ventana emergente aparte: para verlo hay que
  capturar la pantalla entera, no la ventana de la app. (En las notas ya
  existentes se usa un chip propio en vez del `Tooltip`.)
- Notas ya existentes: **no hay lista**. Cada nota de la página actual es una zona
  sensible: el **resaltado usa su rectángulo entero** (así el comentario sale al
  pasar por encima del texto resaltado) y la **nota pegajosa una cajita 24×24**
  sobre su marcador → `show()` rellena `page-notes` con `pdf_to_image` y
  `note-total`.
  **El comentario sale en un CHIP PROPIO encima del resaltado**, no con el
  `Tooltip` de Slint: el `Tooltip` nativo siempre sale debajo del puntero y no
  se puede colocar (su `offset` no es público). El chip se dibuja dentro de la
  imagen de la página (se mueve con el scroll) y aparece con
  `if (ta.has-hover)` (el `TouchArea` de la nota). Ojo: `has-hover` es del
  `TouchArea`, no del `Rectangle`; en Slint 1.18 no hay `MouseArea` ni
  `pointer-event`. El texto va **entero** (Rust ya no lo corta a 45 chars) y
  el chip hace `wrap` a 340px con alto automático; su `x`/`y` se limitan al
  área visible con `root.view-x`/`root.view-y`/`root.view-w`, si no se sale
  de la ventana cuando la página es más ancha que el visor.
  **GOTCHA**: el `TouchArea` de esas zonas solo está activo en modo normal y
  borrar (`enabled: root.tool-mode == 0 || root.tool-mode == 3`): si está
  activo siempre, se come el arrastre del resaltado libre y el clic de la nota.
  (En Slint 1.18 no hay `MouseArea`.)
- **Diálogo propio** (copia de trabajo / contraseña) **centrado con degradado**
  sobre el resto: `dlg-open/dlg-kind/dlg-msg/dlg-pass` + `dlg-answer(bool)`.
  Gotchas de Slint aprendidos aquí:
  - El velo **no puede** ser un hijo directo del `Window` con `width: parent.width`:
    Slint ve un bucle de layout (`root.layoutinfo-h`, "may panic at runtime") y
    además reparte el espacio con el layout principal. Solución: un `Rectangle`
    wrapper como **único hijo del root** (el velo va dentro, con posición
    absoluta) y el tamaño del velo desde Rust (`win-w`/`win-h`), sin depender de
    `parent.width`.
  - El degradado es `@radial-gradient(circle, #000000b0, #00000040)`.
- **GOTCHA `RefCell`**: un `RefMut` creado en el scrutinee de un `match` vive
  hasta el final del `match`; si dentro de las ramas se vuelve a prestar, el
  `RefCell` peta ("RefCell already borrowed") y la app se cae. Por eso existe
  `with_state(c, f)`: presta, ejecuta y **suelta el préstamo antes de devolver**.
  Todo lo que toque el estado dentro de un `match` va con él.
  **Ojo también con `let mut s = st.borrow_mut();` + `notify(&ui, &st, ...)`
  en la misma función**: `notify` vuelve a prestar → peta. Suelta el préstamo
  (o mete el `notify` en un bloque) antes de avisar.
- Si no hay copia de trabajo, el panel de nota **no se cierra** al reintentar
  (el texto se conserva) y hay un botón "Crear copia de trabajo" en la barra
  lateral para crearla cuando quieras.
- `Flickable + VerticalLayout + for` para la lista (no `ListView` de std-widgets).
- Cards: franja de color por gravedad + palabra + tipo/gravedad/pág +
  descripción ES/EN + sugerencias LT (`No hay sugerencias` si vacío) +
  Omitir/Restaurar (cascada por solape) + "Añadir comentario" + obs. gramatical
  (editor inline) + 2 desplegables `DropSel` Tipo/Gravedad + grupo ×5+
  ("Desagrupar"/"Agrupar", `MiniBtn` con `width: 100px` y
  `horizontal-stretch: 0`: al ser el único de su fila se estiraba entera).
- Extracción = unión simple de boxes por carácter, sin shaping tipográfico.
- No hay reemplazo de texto en el PDF (cirugía redact+reescribir, fuera del mínimo);
  las sugerencias son solo de referencia.

## Techos conocidos (no son bugs)

- **No crear segundo `Pdfium` en otro hilo**: se cuelga en `bind`. Por eso el
  escaneo es troceado en el hilo UI y los hilos de página son solo HTTP.
  El `Pdfium` se filtra a 'static (`Box::leak`) y se comparte con
  `thread_local!`: una sola instancia viva en todo el proceso.
- Guardar sobre el `_anotado.pdf` abierto lo mapea Pdfium (SIGTRAP): se guarda
  vía temporal+rename (`save_doc`).
- En Slint `visible: false` sigue reservando layout: para mostrar/ocultar se
  usan bloques `if` (placeholder `Sin documento`, toolbar, desplegables).

## Estado

Fase 1 (ventana+render) · Fase 2 (texto+coords) · Fase 3 (corrector fondo) ·
Fase 4 (subrayado, omitir, notas nativas) · Extras (tema claro/oscuro, progreso
con % izq→der verde, tipos con color, filtro, flash amarillo 5s, descripciones ES/EN).
Fase 5 (revisión 100% LT: `kind`=Ortografía|Sintaxis + `sev` mínima|intermedia|grave
(categoría LT; TYPOS=ortografía), 2 desplegables propios `DropSel`
Tipo+Gravedad —inline, sin std-widgets—, omitir en cascada por solape de boxes
+ limpieza de flash, barra única vía `refresh_progress`, subrayado por clase (rojo
ortografía, morado sintaxis), zoom ×1.25 (0.5–3.0) + nota libre con botón `Nota`
(icono `note_add`, panel sobre la página visible → `annotate_open`), notas acumulativas sobre
`_anotado.pdf` (guardado vía temporal+rename — Pdfium mapea el archivo), sidecar java hijo en
 un puerto libre por ejecución (JVM encerrado: el proxy apunta a un puerto muerto
 → el LT no sale a Internet), `LtReady/LtFail/Syn(gen)` con `scan_id`, hilos de página solo HTTP
 con palabras clonadas —nunca Pdfium en hilos—, offsets en chars → box unión
 vía `join_words`/`span_to_box`; notas existentes leídas (`notes_of` →
 marcadores ámbar + tooltip al pasar el ratón por encima, sin lista); historial 10 últimos en
 `~/.config/lexpdf/history.json` (errores/omitidos/notas + % revisado,
 sección Recientes → `open_doc`); `build.rs` descarga LT ~240MB a `assets/lt/`
 gitignoreado —requiere red en build y Java 17+ en runtime—; medido: arranque
-`~4s, ~80ms/check, ~650MB RAM). `cargo test` rápido (sin motores pesados en tests).
Release actual: **v0.2.6 marcada como prerelease "(experimental)"**: exe de Windows
y AppImage. Las notas de release viven en `release-notes.md` (los dos jobs del
CI las publican con `body_path`; antes estaban duplicadas y se desincronizaron).
Fase 6 = los 12 hallazgos de la auditoría (borrar por `doc_notes`, `start_lt`
para re-encender el motor, ruta en `HisRow`, `has_doc` a false al fallar la
apertura, 4 hilos fijos con `job_tx` en vez de uno por página, fuera el modo
`auto` de `syntax_page` y el estado que nadie pintaba).
Validado en Windows con 1984.pdf (24/10/2026): la última card YA se ve entera.
**GOTCHA cards cortadas (causa raíz)**: la sidebar tiene `alignment: start`
(los `vertical-stretch` se IGNORAN con alignment) y el `Flickable` de
resultados tenía `height: 420px` FIJO → con historial + textos de estado el
420px terminaba POR DEBAJO del borde de la ventana y esa banda nunca se veía
(el scroll ocurre dentro del Flickable). Fix: quitar `alignment: start`,
quitar el alto fijo y dar `vertical-stretch: 1` al Flickable (come el resto);
los `Text` con wrap y sin alto fijo de la sidebar llevan `vertical-stretch: 0`
para no repartirse el espacio con él (en Slint solo crecen los de `max > pref`;
los de alto fijo nunca crecen). Pendiente de validar en Windows: card única
demasiado alta y leyenda "Añadir comentario" también en el resaltado libre.
Licencia proyecto: GPL-3.0-only (la exige Slint enlazado; `pdfium-render` MIT/Apache es compatible). Pdfium: BSD + third-party en `assets/pdfium/licenses/`.

## Modelo de amenazas (qué sale del PC)

- El **PDF nunca sale** del proceso. Lo único que viaja es el *texto* de cada
  página, al sidecar LanguageTool por **loopback** (`127.0.0.1`, puerto libre
  elegido en cada ejecución → ningún proceso ajeno puede interceptarlo).
- El **JVM del LT está encerrado**: `-Dhttp.proxyHost=127.0.0.1 -Dhttp.proxyPort=1`
  (+https) → cualquier salida HTTP del LT muere en un puerto muerto.
  `http.nonProxyHosts` deja el loopback directo.
- `ureq` va **sin TLS**: no puede ni hablar https. El tripwire
  `is_loopback_url` (test) aborta el proceso si la URL se sale de loopback.
- Ficheros con datos sensibles: la copia de trabajo y el historial van **0600**
  (y `~/.config/lexpdf` en 0700).
- **Salvedades honestas**: el texto de cada página está en la memoria del
  proceso java (si alguien compromete la máquina, está comprometido); las
  notas se guardan **en claro dentro del PDF** (cifrar el PDF no está en el
  alcance); un PDF con contraseña de *permisos* no admite notas.
- La **contraseña** del PDF se pide una vez, vive solo en memoria (`&'static
  filtrado`) y nunca se escribe en disco ni en el historial.

## Copia de trabajo (`_anotado`)

- Al abrir un documento: si existe `{nombre}_anotado.pdf` se usa (nunca se crea
  un `_anotado_anotado`: `working_path()` no duplica el sufijo). Si no existe, se
  pregunta en un diálogo de la barra lateral ("Copia de trabajo") salvo que en
  Ajustes esté en "crear siempre" o "nunca".
- Las notas y los resaltados se **guardan al momento** en esa copia (temp + rename,
  0600) y se ven al instante: el documento queda abierto en memoria (`State.doc`),
  las anotaciones se aplican sobre él y se repinta la página.
- Antes de guardar se compara `mtime`+tamaño del fichero: si cambió por fuera
  (otra app lo editó) se reabre para **no pisar** esa edición.
- El **historial guarda siempre la ruta original** → al reabrir desde
  "Recientes" se reutiliza la misma copia de trabajo.
Windows: `installer/windows/` (Inno + `stage.ps1` + CI con SignPath opt-in);
java en `$JAVA_BIN` → `jre\` junto al exe → `PATH`.
Linux/AppImage: `installer/linux/` (`stage-appimage.sh` → AppDir + AppRun con
`LT_HOME`/`JAVA_BIN`/`PDFIUM_DYNAMIC_LIB_PATH`); releases vía `release.yml`
(con smoke test LT real y jlink incl. `jdk.httpserver`).

**GOTCHA release (re-tag)**: re-pushear el mismo tag deja la release existente
en **draft**, y el edit de `softprops/action-gh-release` falla contra ese
estado (el job Linux muere en "Release con AppImage"; Windows pasa porque
llega después). Por eso `release.yml` **borra la release previa**
(`gh release delete "$GITHUB_REF_NAME" --yes || true`) antes de publicar.
Ciclo experimental sin bump: `git push origin :refs/tags/vX` → `git tag -f` →
push, y el paso de release lleva `prerelease: true` +
`name: Release vX (experimental)`. Se **verifica por API** (no hay token para
leer logs): `releases` debe traer los dos assets y `prerelease: true`.
OJO: si borras el tag y no recreas la release, GitHub la deja en draft y no
aparece en la página de Releases (ni como "Latest", por ser prerelease).

**Ramas**: los fixes visuales se hacen en `experimental` (se re-etiqueta el
mismo `vX`); `main` va **por detrás** a propósito (commit del fix + `git
revert`, sin reescribir historia). No mover los fixes a `main` hasta que el
usuario valide el exe.
