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
index.html      # prototipo web anterior, solo referencia de diseño/UX
```

## Comandos

```bash
cargo build                      # debug (~400MB por Slint, normal)
cargo test --bin lexpdf   # 4 tests (coordenadas, subrayado, kinds, anotación real)
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
- `scan_queue/scan_pos`, `scan_id`, `flash`, `lt_ready/lt_dead`, `last_lang`, `tx`.

Flujos:

1. `show()` — renderiza la página (`PdfRenderConfig` 1400px·zoom) a `slint::Image`,
   extrae palabras y **quema subrayados por clase** (rojo ortografía, morado
   sintaxis) + **wash amarillo** (flash) en el bitmap. No toca el escaneo.
2. `start_scan()` — encola todas las páginas (desde la visible). **No usa hilos.**
3. `scan_tick()` (Timer 250ms, presupuesto 80ms/tick) — extrae palabras por
   página y lanza un hilo HTTP al sidecar LT; la barra (`scan-progress`,
   `scan-text` con %) mide páginas extraídas. Sin LT no avanza (salvo `lt_dead`,
   que vacía la cola). Los `Syn(gen)` re-subrayan si tocan la página visible.
4. Sidecar LT (hijo java o externo en :8081, `LtReady/LtFail` con `scan_id`):
   `syntax_page()` mapea offsets en chars → box unión (`join_words`/`span_to_box`),
   clase por categoría (`TYPOS`=Ortografía) y `sev` (GRAMMAR=grave…).
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
- Notas ya existentes: **no hay lista**. Cada nota de la página actual es una zona
  24×24 sobre su marcador con `Tooltip { text: ... }` (popup nativo de Slint, ~500 ms
  de retardo) → `show()` rellena `page-notes` con `pdf_to_image` y `note-total`.
- `Flickable + VerticalLayout + for` para la lista (no `ListView` de std-widgets).
- Cards: franja de color por gravedad + palabra + tipo/gravedad/pág +
  descripción ES/EN + sugerencias LT (`No hay sugerencias` si vacío) +
  Omitir/Restaurar (cascada por solape) + comentario + obs. gramatical
  (editor inline) + 2 desplegables `DropSel` Tipo/Gravedad + grupo ×5+.
- Extracción = unión simple de boxes por carácter, sin shaping tipográfico.
- No hay reemplazo de texto en el PDF (cirugía redact+reescribir, fuera del mínimo);
  las sugerencias son de referencia, como en `index.html`.

## Techos conocidos (no son bugs)

- **No crear segundo `Pdfium` en otro hilo**: se cuelga en `bind`. Por eso el
  escaneo es troceado en el hilo UI y los hilos de página son solo HTTP.
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
(icono `note_add`, panel sobre la página visible → `annotate_at`), notas acumulativas sobre
`_anotado.pdf` (guardado vía temporal+rename — Pdfium mapea el archivo), sidecar java hijo —reutiliza externo
 en :8081—, `LtReady/LtFail/Syn(gen)` con `scan_id`, hilos de página solo HTTP
 con palabras clonadas —nunca Pdfium en hilos—, offsets en chars → box unión
 vía `join_words`/`span_to_box`; notas existentes leídas (`read_notes` →
 marcadores ámbar + tooltip al pasar el ratón por encima, sin lista); historial 10 últimos en
 `~/.config/lexpdf/history.json` (errores/omitidos/notas + % revisado,
 sección Recientes → `open_doc`); `build.rs` descarga LT ~240MB a `assets/lt/`
 gitignoreado —requiere red en build y Java 17+ en runtime—; medido: arranque
-`~4s, ~80ms/check, ~650MB RAM). `cargo test` rápido (sin motores pesados en tests).
Licencia proyecto: GPL-3.0-only (la exige Slint enlazado; `pdfium-render` MIT/Apache es compatible). Pdfium: BSD + third-party en `assets/pdfium/licenses/`.
Windows: `installer/windows/` (Inno + `stage.ps1` + CI con SignPath opt-in);
java en `$JAVA_BIN` → `jre\` junto al exe → `PATH`.
Linux/AppImage: `installer/linux/` (`stage-appimage.sh` → AppDir + AppRun con
`LT_HOME`/`JAVA_BIN`/`PDFIUM_DYNAMIC_LIB_PATH`); releases vía `release.yml`
(con smoke test LT real y jlink incl. `jdk.httpserver`).
