# LexPDF

Revisor de PDF de escritorio que funciona **sin internet**. Abre un PDF, pasa cada página
por LanguageTool en tu propio PC, te subraya los errores ortográficos y gramaticales
**sobre la página**, y te deja anotar el documento con notas nativas.

Sin cuentas, sin nube, sin telemetría. El PDF no sale del ordenador.


## Esto está vibe-coded

Conviene decirlo antes que nada: **LexPDF está hecho con asistencia de IA** (Claude, en
sesiones cortas, con un modo de trabajo llamado "Ponytail" que obliga a no over-engineer
nada). El resultado es un proyecto que funciona, pero con la textura típica de esa forma
de trabajar:

- **El código es legible y está documentado**, pero es *mucho* código en dos ficheros
  (`src/main.rs` y `ui/app.slint`). No hay capas, ni abstracciones, ni patrones de
  arquitectura: hay funciones y structs. Para un proyecto de un solo ejecutable, funciona.
- **Hay tests**, pero son 16 tests de las partes que duelen (coordenadas, subrayado,
  redondeo de notas, permisos, el chute del `RefCell` que tumbaba la app). No hay
  cobertura end-to-end.
- **Los bugs existen y los conoces por el camino.** Los que nos comimos están escritos
  en `AGENTS.md`: que un `TouchArea` declarado después del contenido se traga los clics,
  que `Text` es `@implicit_size` y por eso el texto se pegaba a la izquierda, que
  instanciar `Pdfium` dos veces se cuelga en `bind`, que un `RefMut` en el scrutinee de
  un `match` viva hasta el final y haga petar el `RefCell`... Ese fichero **es el diario
  de a bordo del proyecto** y probablemente lo más útil que hay en el repo.
- **El código no está auditado.** Úsalo con criterio: es ideal para documentos propios
  o de trabajo, pero si vas a meter un documento confidencial, lee antes la sección
  [Modelo de amenazas](#modelo-de-amenazas) y comprueba lo que te importe.

Si te mola el estilo, puedes seguir el rastro: cada commit es una mejora pequeña y
verificable, y los mensajes dicen qué se tocaba y por qué.

## Qué hace

- Abre PDFs (también **con contraseña**) y los renderiza página a página: navegación,
  zoom 50–300 % (editable y con rueda), scroll.
- Extrae el texto con coordenadas y lo revisa en segundo plano con **LanguageTool local**
  (sidecar Java): ortografía (categoría `TYPOS`) y sintaxis, con barra de progreso.
- **Subraya los errores en la propia página**: rojo = ortografía, morado = sintaxis.
  Al pinchar un error saltas a su página y se resalta en amarillo.
- Clasifica por tipo (`Ortografía`/`Sintaxis`) y gravedad (`grave`/`intermedia`/`mínima`),
  con dos desplegables para filtrar.
- **Omitir / restaurar** (en cascada si se solapa con otra clase) y agrupar errores
  repetidos (×5+) para no ver 40 veces la misma falta.
- **Resaltados + comentarios** de verdad: al comentar un error se escribe una
  anotación `Highlight` en el PDF con su comentario, y se ve en **cualquier
  lector** (no solo aquí), resaltando la palabra **entera**. Se borran desde la
  propia card o con el 🗑 de la barra.
- **Resaltar texto libre**: el botón 🖌 de la barra, arrastras el rectángulo sobre
  el texto y sale un comentario. Sale en amarillo (el color de un rotulador).
  En Ajustes puedes elegir **por área** (el rectángulo tal cual) o **por texto**.
  Con "por texto" sale una opción más: **por recuadro** (cada palabra en su
  propia caja) o **selección de texto** (una sola banda continua por línea,
  como al seleccionar texto en un PDF). **Por defecto es "selección de
  texto"**, y el panel te enseña el texto que has seleccionado de verdad
  («Seleccionado: …», **en negrita**), para que sepas qué vas a resaltar. El
  comentario se **borra** al elegir otro texto (la caja vuelve a su
  «Escriba aquí el comentario...»).
- **Notas adhesivas** (sticky) para lo que no es un error, en cualquier punto del
  visor. Al pasar el ratón por encima de un **resaltado** o de una nota sale un
  tooltip con su comentario (el resaltado entero es zona sensible, no solo el
  trocito del marcador).
- Todo eso vive en `{nombre}_anotado.pdf`; **el original no se toca nunca**.
- Tema claro/oscuro (`PDF_DARK=1` para arrancar en oscuro).
- Apertura directa por argumento: `cargo run -- doc.pdf`.

## Requisitos

- **Rust estable** + Cargo.
- **Java 17+** en el `PATH` (es el motor de revisión).
- Red **solo en el primer build**: se descarga LanguageTool (~240 MB) a `assets/lt/`,
  que va gitignoreado. Después, ni el build ni la app necesitan red.
- Linux: `libpdfium.so` ya está en `assets/pdfium/` y `build.rs` lo copia junto al
  ejecutable (o `PDFIUM_DYNAMIC_LIB_PATH=...`).
- Windows: igual, con `assets/pdfium/pdfium.dll`.

## Uso rápido

```bash
cargo run -- /ruta/doc.pdf      # abre directo, sin diálogo
cargo run                        # sin argumento: eliges el PDF
PDF_DARK=1 cargo run -- doc.pdf  # arranca en oscuro
cargo test --bin lexpdf          # 17 tests, rápido
cargo build                      # debug; ~400 MB por Slint, es normal
```

El flujo normal: abres el PDF → esperas a la barra → pinchas un error para ver el
detalle y saltar a su página → `Omitir`, `Comentar` u `Obs. gramatical` → la nota se
guarda sola en la copia de trabajo.

La primera vez que abres un documento te pregunta si quieres crear la copia
`_anotado.pdf` (donde van las notas). Si dices que no, puedes crear el botón "Crear
copia de trabajo" en cualquier momento; y si no hay copia, al guardar una nota te
vuelve a preguntar (el texto **no** se pierde).

## Stack

- **Rust 2021**, un solo binario en `src/main.rs`.
- UI declarativa con **Slint 1.18** en `ui/app.slint` (controles propios, nada de
  `std-widgets`).
- PDF con **PDFium** vía `pdfium-render` 0.8 (binarios en `assets/pdfium/`).
- Ortografía y sintaxis con **LanguageTool 6.x** (sidecar Java hijo) + `ureq` 3 sync.
- Iconos **Material Icons Outlined** (Apache-2.0), incrustados como `Path` SVG.
- `lopdf` para escribir los `/QuadPoints` de los resaltados (pdfium no puede).

## Estructura

```
Cargo.toml      # ureq (HTTP sync al sidecar) + serde_json (/v2/check)
build.rs        # compila ui/app.slint + copia libpdfium + descarga LT a assets/lt/
src/main.rs     # la app entera: render, extracción, escaneo, subrayado, notas
ui/app.slint    # ventana, sidebar, cards, desplegables, visor con zoom, temas
AGENTS.md       # el diario de a bordo: gotchas, decisiones y estado del proyecto
assets/pdfium/  # libpdfium.so + pdfium.dll + LICENSE.pdfium + licenses/
assets/icons/   # licencia Apache-2.0 de Material Icons
assets/lt/      # LanguageTool desempaquetado (gitignoreado, lo baja build.rs)
input/          # PDFs de prueba
index.html      # prototipo web anterior, solo referencia de diseño/UX
```

En runtime, LanguageTool se busca en: `$LT_HOME` → junto al ejecutable →
`CARGO_MANIFEST_DIR/assets/lt/`.

## Cómo funciona (por encima)

- `show()`: renderiza la página (1400 px · zoom), extrae palabras y pinta los subrayados
  por clase y el flash. El documento queda **abierto en memoria**: cambiar de página solo
  renderiza, no reabre el PDF.
- `start_scan()` + `scan_tick()` (timer de 250 ms, presupuesto de 80 ms por tick): encola
  todas las páginas desde la visible, extrae palabras por trozos en el hilo de la UI y
  lanza **un hilo HTTP por página** contra el sidecar (nunca Pdfium en hilos: se cuelga).
- `syntax_page()`: offsets en caracteres de LT → caja unión (`join_words`/`span_to_box`),
  clase por categoría y gravedad por regla.
- `State` central en `Rc<RefCell<State>>`: `all_errs` (todo el documento) + `view`
  (índices ya filtrados). Los callbacks de la UI reciben el índice de fila → hay que
  mapear por `view` antes de tocar `all_errs`.
- `annotate_open()`: sticky-notes nativas sobre la copia de trabajo, guardadas vía
  temporal + rename (Pdfium tiene el fichero mapeado).

## Modelo de amenazas

Qué sale de tu ordenador, exactamente:

- El **PDF nunca sale** del proceso. Lo único que viaja es el *texto* de cada página, al
  motor local por **loopback** (`127.0.0.1`), en un puerto libre elegido en cada
  ejecución para que ningún proceso ajeno lo intercepte.
- El **JVM del motor está encerrado**: el proxy apunta a un puerto muerto, así que
  LanguageTool no puede salir a Internet aunque lo intentara.
- `ureq` va **sin TLS**: no puede ni hablar https. Hay un *tripwire* que aborta el
  proceso si la URL se sale de loopback.
- La copia de trabajo y el historial se guardan en modo **0600** (solo tu usuario) y
  `~/.config/lexpdf` en 0700.
- La **contraseña** de un PDF protegido se pide una vez, vive solo en memoria y nunca se
  escribe en disco ni en el historial.

Y lo que **no** podemos prometerte:

- El texto de cada página está en la memoria del proceso Java. Si alguien compromete tu
  máquina, está comprometido.
- Tus notas se guardan **en claro dentro del PDF**. Cifrar el PDF no está en el alcance.
- Un PDF con contraseña de *permisos* (no de apertura) no admite notas: avisa el error.

## Límites conocidos

- Primer arranque del motor: ~4-10 s (calienta las reglas); en caliente ~80 ms por
  comprobación y ~650 MB de RAM.
- La extracción de texto es la unión simple de cajas por carácter, sin *shaping*
  tipográfico: en documentos con kerning raro el subrayado puede quedar un pelín desplazado.
- No se reemplaza texto dentro del PDF: las sugerencias son de referencia.
- El ratón se usa para el hover de las notas; en pantallas táctiles el tooltip no sale
  (se puede leer la nota abriendo el PDF en otro visor).

## Cómo participar

- **Reporta un bug** con el PDF (o un trocito) y los pasos: eso es lo que más ayuda.
- **Lee `AGENTS.md` antes de tocar el código**: está lleno de trampas ya pagadas.
- Si tocas la UI, recuerda las dos reglas de Slint que más caro salen: el `TouchArea`
  va *después* del contenido (si no, se come los clics) y los `Text` necesitan
  `width/height` explícitos para poder centrase.

## Ajustes

El botón ⚙ de la barra lateral (a la izquierda, visible siempre, incluso sin
PDF abierto) muestra una **capa dentro de la propia ventana**, encima de un velo
que bloquea todo lo demás hasta cerrarla; las capas se apilan: **Ajustes →
Acerca de → Licencias**. Contiene:

- **Color del resaltado**: por gravedad (rojo = ortografía, morado = sintaxis) o
  amarillo único. El texto libre siempre va en amarillo.
- **Resaltar texto libre**: por área o por texto. Si eliges "por texto", sale
  una sub-opción: por recuadro (una caja por palabra) o selección de texto (una
  banda continua por línea).
- **Copia de trabajo**: preguntar al abrir / crearla siempre / nunca. Ahí van a parar
  las notas y los resaltados.
- **Motor de revisión**: activado (el texto se revisa en local) o desactivado (no se
  envía ni una palabra a ningún proceso).
- **Ubicación del aviso**: si las pastillas de aviso salen arriba (por defecto)
  o abajo de la ventana.
- **Acerca de**: el logo, el nombre de la app y un botón con las **licencias
  enteras** de cada dependencia (Slint, PDFium, LanguageTool, lopdf, los iconos…)
  y el enlace a su repositorio. El texto de la licencia se desliza con la rueda.

Se guardan en `~/.config/lexpdf/settings.json` (0600, solo tu usuario).

## Licencia

**LexPDF** — **GPL-3.0-only** (ver `LICENSE`). La exige Slint, que va enlazado estáticamente.

Licencias de cada componente:

| Componente | Licencia |
|---|---|
| Slint (interfaz) | GPL-3.0-only (o licencia comercial de Slint) |
| PDFium (motor de render) | BSD-3-Clause |
| LanguageTool (sidecar) | LGPL-2.1 |
| pdfium-render, ureq, image, rfd, anyhow, serde | MIT o Apache-2.0 |
| lopdf | MIT |
| Material Icons Outlined | Apache-2.0 |

LexPDF — Copyright (C) 2026 Jose Cauich. Textos completos en `assets/licenses/`,
visibles desde la app (Ajustes → Acerca de → Licencias).

> LexPDF is free software: you can redistribute it and/or modify it under
> the terms of the GNU General Public License as published by the Free
> Software Foundation, either version 3 of the License, or (at your
> option) any later version.
>
> LexPDF is distributed in the hope that it will be useful, but WITHOUT
> ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
> FITNESS FOR A PARTICULAR PURPOSE.  See the GNU General Public License
> for more details.
>
> You should have received a copy of the GNU General Public License
> along with this program.  If not, see <https://www.gnu.org/licenses/>.
