# LexPDF

Revisor de PDF de escritorio, **100% offline**. Abre PDFs, revisa ortografía y sintaxis en español e inglés en segundo plano, subraya errores sobre la página y permite omitirlos o inyectar notas nativas en el PDF.

Sin red, sin telemetría.

## Características

- Apertura de PDF con renderizado por página, navegación, zoom (50–300%, editable + rueda) y scroll.
- Extracción de texto con coordenadas (origen abajo-izquierda, puntos PDF).
- Revisión ES/EN con LanguageTool local (sidecar Java): ortografía (categoría TYPOS) + sintaxis, con barra de progreso única.
- Subrayado por clase quemado en el bitmap (rojo ortografía, morado sintaxis) + flash amarillo al seleccionar un error.
- Clasificación por tipo (`Ortografía`/`Sintaxis`) y gravedad (`grave`/`intermedia`/`mínima`), con 2 desplegables de filtro.
- Lista ordenada por aparición, con descripciones ES/EN y sugerencias LT.
- Omitir / restaurar (en cascada si hay solape con otra clase) + agrupado ×5 con comentar/rechazar/desagrupar.
- Notas adhesivas nativas acumulativas en `{nombre}_anotado.pdf` — jamás toca el original; nota libre en cualquier punto del visor.
- Tema claro / oscuro propio (`PDF_DARK=1` para arrancar en oscuro).
- Apertura directa por argumento: `cargo run -- doc.pdf` (ideal para demos).

## Stack

- **Rust 2021**, binario único en `src/main.rs`.
- UI declarativa con **Slint 1.10** en `ui/app.slint` (controles propios, sin `std-widgets`).
- PDF con **PDFium** vía `pdfium-render` 0.8 (binarios en `assets/pdfium/`).
- Sintaxis+ortografía con **LanguageTool 6.x** (sidecar Java hijo, reutiliza externo en `:8081`) + `ureq` 3 sync.

## Requisitos

- Rust estable + Cargo.
- Java 17+ en el `PATH` (sidecar de revisión).
- Red solo en el primer build (descarga LT ~240MB a `assets/lt/`, gitignoreado).
- Linux: `libpdfium.so` ya incluido en `assets/pdfium/`, `build.rs` lo copia junto al exe. Alternativa: `PDFIUM_DYNAMIC_LIB_PATH=...`.
- Windows: se usa `assets/pdfium/pdfium.dll` del mismo modo.

## Uso rápido

```bash
cargo run -- /ruta/doc.pdf
# o sin argumento: elige el PDF en el diálogo

PDF_DARK=1 cargo run -- doc.pdf
cargo test --bin lexpdf
cargo build   # debug ~400MB por Slint, normal
```

Flujo típico: abre PDF → espera la barra → clic en un error para ver el detalle y saltar a su página → `Omitir`, `Comentar` u `Obs. gramatical` → la nota se acumula en `{nombre}_anotado.pdf`.

## Estructura

```
Cargo.toml      # ureq (HTTP sync al sidecar) + serde_json (/v2/check)
build.rs        # compila ui/app.slint + copia libpdfium + descarga LT a assets/lt/
src/main.rs     # app completa: render, extracción, scan, subrayado, notas
ui/app.slint    # ventana, sidebar 400px, cards, desplegables, visor con zoom, temas
assets/pdfium/  # libpdfium.so + pdfium.dll + LICENSE.pdfium + licenses/ + VERSION.pdfium
assets/lt/      # LanguageTool desempaquetado (gitignoreado, lo baja build.rs)
input/          # PDFs de prueba
index.html      # prototipo web anterior, solo referencia de diseño/UX
```

En runtime LT se busca en: `$LT_HOME` → junto al exe → `CARGO_MANIFEST_DIR/assets/lt/`.

## Cómo funciona

- `show()`: renderiza la página (1400px·zoom), extrae palabras y pinta subrayados por clase + flash. No toca el escaneo.
- `start_scan()` + `scan_tick()` (Timer 250ms, presupuesto 80ms): encola todas las páginas desde la visible, extrae palabras por trozos en el hilo UI y lanza un hilo HTTP al sidecar por página (nunca Pdfium en hilos: se cuelga en `bind`).
- `syntax_page()`: offsets en chars de LT → box unión (`join_words`/`span_to_box`), clase por categoría, gravedad por regla.
- `State` central en `Rc<RefCell<State>>`: `all_errs` (todo el doc) + `view` (índices filtrados). Los handlers reciben índice de fila → mapear por `view`.
- `annotate*()`: sticky-notes nativas acumulativas sobre `{nombre}_anotado.pdf` (vía temporal+rename).

## Límites conocidos

- Primer arranque del sidecar: ~4-10s (calienta reglas); en caliente ~80ms/check, ~650MB RAM.
- Extracción = unión simple de boxes por carácter, sin shaping tipográfico.
- No hay reemplazo de texto en el PDF, solo sugerencias de referencia + notas.

## Licencia

GPL-3.0-only — ver `LICENSE` (la exige Slint, enlazado estático). `pdfium-render`: MIT/Apache. PDFium: BSD + third-party en `assets/pdfium/licenses/`. Binarios LT: LGPL como sidecar aparte (LanguageTool).
