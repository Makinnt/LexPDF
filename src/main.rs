// SPDX-License-Identifier: GPL-3.0-only
//
// LexPDF — corrector ortográfico de escritorio para PDFs, 100% offline.
// Copyright (C) 2026 Jose Cauich
//
// This file is part of LexPDF.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    sync::mpsc::{Sender, channel},
};

use anyhow::Context;
use pdfium_render::prelude::*;
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, Timer, TimerMode, VecModel};

slint::include_modules!();

#[derive(Clone, Debug)]
struct Word {
    text: String,
    // coords PDF en puntos, origen abajo-izquierda (igual que Pdfium)
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
}

struct State {
    // ponytail: Pdfium filtrado a 'static (Box::leak) para poder guardar el
    // PdfDocument en el estado; si no, PdfDocument<'a> tomaría prestado el
    // Pdfium y no se podría tener sin struct autoreferente.
    pdfium: &'static Pdfium,
    doc: Option<PdfDocument<'static>>, // documento abierto (no se reabre por página)
    doc_from: Option<PathBuf>,           // de dónde se abrió
    doc_stamp: Option<(i64, u64)>,       // (mtime, tamaño) al abrir: si cambia, se reabre
    path: Option<PathBuf>,               // el documento que abrió el usuario (historial)
    working: Option<PathBuf>,            // copia de trabajo "{nombre}_anotado.pdf"
    settings: Settings,                  // ajustes del usuario (ventana aparte)
    // idx de all_errs -> (página, rect) de la anotación. OJO: no se guarda el
    // índice de doc_notes porque al borrar una nota los demás se desplazan y
    // el "Borrar" de la card acababa borrando la anotación equivocada.
    err_anno: HashMap<usize, (u32, [f32; 4])>,
    free_rects: Option<(u32, Vec<[f32; 4]>)>, // resaltado libre pendiente: pág + rects
    password: Option<&'static str>,      // solo en memoria, nunca en disco
    dlg_pending: Option<Pending>,        // acción pendiente de un diálogo
    page: u32,
    total: u32,
    words: Vec<Word>,
    page_w: f32,
    page_h: f32,
    img_w: u32,
    img_h: u32,
    all_errs: Vec<Misspelling>, // hallazgos de todo el doc (LT: ortografía + sintaxis)
    doc_notes: Vec<DocNote>, // notas nativas ya existentes en el PDF
    view: Vec<usize>, // índices a all_errs tras aplicar los filtros
    view_group: Vec<Option<String>>, // paralela a view: Some(word) si la fila es grupo colapsado
    expanded: HashSet<String>, // palabras desagrupadas por el usuario (grupo 5+)
    flash: Option<(usize, std::time::Instant)>, // error resaltado en amarillo
    scan_id: u64,           // generación del escaneo vigente; invalida Syn viejos
    scan_queue: Vec<u32>,   // páginas pendientes (orden desde la visible)
    scan_pos: usize,
    popup_until: Option<std::time::Instant>, // toast visible hasta este instante
    zoom: f32,                               // 0.5 = ~700px de ancho (cabe); pasos ×1.25
    free_pt: Option<(u32, f32, f32)>, // nota libre pendiente: (página, x, y en puntos PDF)
    lt_ready: bool, // sidecar LanguageTool respondiendo en localhost:8081
    lt_dead: bool,  // LT no va a arrancar (sin java): el scan avanza sin sintaxis
    pending: u32, // hilos HTTP en vuelo (la barra sigue hasta que vuelven)
    jobs_total: u32, // hilos lanzados en este scan (tramo 50→100 de la barra)
    boot_start: std::time::Instant, // arranque del motor (relleno temporal 0→99)
    sec_bytes: u64, // texto enviado al motor (métrica: siempre loopback)
    syn_received: bool, // llegó al menos un Syn (si no, "LT no devolvió ninguno")
    lt_child: Option<std::process::Child>, // hijo java propio
    tx: Sender<SpellMsg>,
    last_lang: i32,
}

/// Qué hacer cuando el usuario responde a un diálogo.
enum Pending {
    CreateCopy,                       // crear la copia de trabajo y seguir
    CreateCopyThenNote(AnnotateReq),  // crear la copia y luego anotar
    OpenWithPassword(PathBuf),        // reintentar la apertura con contraseña
    ClearHistory,                     // limpiar el historial de recientes
}

/// Anotación a crear (se aplica al documento abierto).
#[derive(Clone)]
enum AnnotateReq {
    /// Resaltado + comentario sobre la palabra de uno o varios hallazgos.
    Highlight(Vec<usize>, String),
    /// Nota adhesiva libre (sticky note) en un punto.
    Note(u32, f32, f32, String),
    /// Resaltado libre sobre uno o varios rectángulos (página, rects, texto).
    HighlightRects(u32, Vec<[f32; 4]>, String),
}

/// Tipo de anotación nativa del PDF.
#[derive(Clone, Copy, Debug, PartialEq)]
enum AnnoKind {
    Note,      // sticky note (Text)
    Highlight, // resaltado (Highlight) con comentario
}

#[derive(Clone, Debug)]
struct DocNote {
    page: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    text: String,
    kind: AnnoKind,
}

#[derive(Clone, Debug)]
struct Misspelling {
    word: String,
    sug: Vec<String>,
    page: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    dismissed: bool,
    kind: String, // Ortografía | Sintaxis (por categoría LT: TYPOS vs resto)
    sev: String,  // mínima | intermedia | grave
}

/// Clase por categoría LT: TYPOS (incluye MORFOLOGIK de ES/EN) = ortografía.
fn kind_of_lt(cat: &str) -> &'static str {
    if cat == "TYPOS" { "Ortografía" } else { "Sintaxis" }
}

/// Gravedad desde la categoría de regla LT (rule.category.id):
/// gramática = grave, puntuación = intermedia, estilo = mínima;
/// TYPOS = intermedia (como la falta clara de antes).
fn sev_of_lt(cat: &str) -> &'static str {
    match cat {
        "GRAMMAR" | "SEMANTICS" => "grave",
        "TYPOS" | "PUNCTUATION" | "CASING" => "intermedia",
        _ => "mínima", // STYLE, TYPOGRAPHY, REDUNDANCY, etc.
    }
}

/// Peor gravedad de un grupo (grave > intermedia > mínima).
fn worst_sev<'a, I>(sevs: I) -> &'static str
where
    I: Iterator<Item = &'a str> + Clone,
{
    for s in ["grave", "intermedia", "mínima"] {
        if sevs.clone().any(|x| x == s) {
            return s;
        }
    }
    "mínima"
}

fn kind_bar(sev: &str) -> slint::Color {
    match sev {
        "grave" => slint::Color::from_rgb_u8(211, 47, 47),
        "intermedia" => slint::Color::from_rgb_u8(245, 124, 0),
        _ => slint::Color::from_rgb_u8(25, 118, 210),
    }
}

fn desc_for(kind: &str, en: bool) -> &'static str {
    match (kind, en) {
        ("Sintaxis", false) => "Posible error de sintaxis",
        ("Sintaxis", true) => "Possible grammar issue",
        (_, false) => "Posible error de ortografía",
        (_, true) => "Possible spelling mistake",
    }
}

enum SpellEvent {
    LtReady(Option<std::process::Child>), // hijo java propio
    LtFail(String), // java ausente o LT no arranca: sin revisión hasta que haya servidor
    Syn(u64, Vec<Misspelling>), // (gen, hallazgos LT de una página: ortografía + sintaxis)
}
type SpellMsg = SpellEvent;

/// Qué `java` usar: $JAVA_BIN → `jre\bin\java.exe` junto al exe (instalador
/// Windows) → `java` del PATH. Devuelve el comando listo para Command.
fn java_cmd() -> std::process::Command {
    if let Ok(j) = std::env::var("JAVA_BIN") {
        return std::process::Command::new(j);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            for c in [d.join("jre/bin/java"), d.join("jre/bin/java.exe")] {
                if c.is_file() {
                    return std::process::Command::new(c);
                }
            }
        }
    }
    std::process::Command::new("java")
}

/// Dónde vive LanguageTool desempaquetado: $LT_HOME → junto al exe → assets/lt.
fn lt_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("LT_HOME") {
        let j = PathBuf::from(p).join("languagetool-server.jar");
        if j.is_file() {
            return j.parent().map(|d| d.to_path_buf());
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            for c in [d.join("lt/languagetool-server.jar"), d.join("languagetool-server.jar")] {
                if c.is_file() {
                    return c.parent().map(|d| d.to_path_buf());
                }
            }
        }
    }
    let m = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/lt/languagetool-server.jar");
    if m.is_file() {
        return m.parent().map(|d| d.to_path_buf());
    }
    None
}

fn lt_url(path: &str) -> String {
    format!("http://127.0.0.1:{}{path}", LT_PORT.load(std::sync::atomic::Ordering::Relaxed))
}

/// Puerto local del sidecar: se elige libre en cada ejecución para que ningún
/// proceso ajeno pueda interceptar el texto (antes era 8081 fijo).
static LT_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// Pide al SO un puerto libre en loopback. Puro salvo por la llamada al SO.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(8081)
}

/// ¿El nombre es el de una copia de trabajo ("X_anotado.pdf")?
fn is_working_name(name: &str) -> bool {
    name.len() > "_anotado.pdf".len() && name.ends_with("_anotado.pdf")
}

/// Ruta del fichero de trabajo de `path`. Nunca duplica el sufijo:
/// "doc.pdf" -> "doc_anotado.pdf"; "doc_anotado.pdf" -> "doc_anotado.pdf".
fn working_path(path: &std::path::Path) -> PathBuf {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if is_working_name(name) {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("doc");
    path.with_file_name(format!("{stem}_anotado.pdf"))
}

/// (mtime en segundos, tamaño) del fichero: para detectar cambios externos.
fn file_stamp(path: &std::path::Path) -> Option<(i64, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((mtime, md.len()))
}

/// Pdfium() filtrado a 'static: el PdfDocument lo toma prestado y así puede
/// vivir en el estado. Fuga deliberada (el proceso termina igual).
fn leak_pdfium(p: Pdfium) -> &'static Pdfium {
    Box::leak(Box::new(p))
}

// Una instancia de Pdfium por hilo: crear un segundo `bind` se cuelga
// (ver AGENTS.md). El estado la comparte; los hilos de página no la tocan.
thread_local! {
    static PDFIUM: &'static Pdfium = leak_pdfium(bind_pdfium());
}

/// La contraseña se filtra a 'static por lo mismo (PdfDocument la toma
/// prestada). Solo vive en memoria: nunca se escribe a disco ni al historial.
fn leak_secret(s: &str) -> &'static str {
    let b: Box<str> = s.to_owned().into_boxed_str();
    Box::leak(b)
}

/// Tripwire: el texto del PDF solo puede viajar a loopback. Puro y testeable.
/// Ojo con `localhost.evil.com`: se compara el host exacto, no prefijo.
fn is_loopback_url(url: &str) -> bool {
    let after = url.strip_prefix("http://").unwrap_or("");
    let host = after.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// ¿Hay un servidor LT respondiendo? (sirve para reutilizar uno ya abierto).
fn lt_alive() -> bool {    ureq::get(&lt_url("/v2/languages"))
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(3)))
        .build()
        .call()
        .is_ok()
}

/// Un chequeo /v2/check crudo: devuelve (offset_chars, len_chars, reemplazos<=3, categoría).
fn lt_check(text: &str, lang: &str) -> Vec<(usize, usize, Vec<String>, String)> {
    let url = lt_url("/v2/check");
    if !is_loopback_url(&url) {
        // Desvío del texto fuera de la máquina: corte abrupto ANTES de enviar nada.
        std::process::abort();
    }
    let body = match ureq::post(&url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(60)))
        .build()
        .send_form([("language", lang), ("text", text)])
    {
        Ok(mut r) => match r.body_mut().read_to_string() {
            Ok(b) => b,
            Err(_) => return vec![],
        },
        Err(_) => return vec![],
    };
    let v: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let mut out = vec![];
    if let Some(ms) = v.get("matches").and_then(|m| m.as_array()) {
        for m in ms.iter().take(40) {
            let (o, l) = (m.get("offset").and_then(|x| x.as_u64()), m.get("length").and_then(|x| x.as_u64()));
            let (Some(o), Some(l)) = (o, l) else { continue };
            let mut rep: Vec<String> = m
                .get("replacements")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|x| x.get("value").and_then(|v| v.as_str()).map(|s| s.to_string())).take(3).collect())
                .unwrap_or_default();
            rep.retain(|r| !r.is_empty());
            if rep.is_empty() || l == 0 {
                continue;
            }
            let cat = m
                .get("rule")
                .and_then(|r| r.get("category"))
                .and_then(|c| c.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            out.push((o as usize, l as usize, rep, cat));
        }
    }
    out
}

/// Une palabras con un espacio y anota el rango (en chars, como la API de LT)
/// de cada una. Puro y testeable.
fn join_words(words: &[Word]) -> (String, Vec<std::ops::Range<usize>>) {
    let mut text = String::new();
    let mut ranges = Vec::with_capacity(words.len());
    for w in words {
        if !text.is_empty() {
            text.push(' ');
        }
        let s = text.chars().count();
        text.push_str(&w.text);
        ranges.push(s..s + w.text.chars().count());
    }
    (text, ranges)
}

/// Mapea un span de bytes al box unión + frase de las palabras solapadas.
fn span_to_box(words: &[Word], ranges: &[std::ops::Range<usize>], bs: usize, be: usize) -> Option<(f32, f32, f32, f32, String)> {
    let mut hit: Option<(f32, f32, f32, f32, Vec<&str>)> = None;
    for (w, r) in words.iter().zip(ranges.iter()) {
        if r.start < be && bs < r.end {
            hit = Some(match hit {
                None => (w.x0, w.y0, w.x1, w.y1, vec![w.text.as_str()]),
                Some((x0, y0, x1, y1, mut ws)) => {
                    ws.push(w.text.as_str());
                    (x0.min(w.x0), y0.min(w.y0), x1.max(w.x1), y1.max(w.y1), ws)
                }
            });
        }
    }
    hit.map(|(x0, y0, x1, y1, ws)| (x0, y0, x1, y1, ws.join(" ")))
}

/// Pasa LanguageTool sobre el texto de una página y devuelve TODOS los
/// hallazgos (ortografía si categoría TYPOS, sintaxis el resto).
/// Corre en hilo fondo (HTTP sync); nunca en el Timer.
/// mode: 0 auto (es+en sin solapes), 1 es, 2 en.
fn syntax_page(words: &[Word], page: u32, mode: i32) -> Vec<Misspelling> {
    let (text, ranges) = join_words(words);
    if text.is_empty() {
        return vec![];
    }
    let langs: &[&str] = match mode {
        1 => &["es-ES"],
        2 => &["en-US"],
        _ => &["es-ES", "en-US"],
    };
    // (char_start, char_end, sugs, categoría); en auto el primer idioma que marca gana el solape
    let mut spans: Vec<(usize, usize, Vec<String>, String)> = vec![];
    for lang in langs {
        for (o, l, rep, cat) in lt_check(&text, lang) {
            let be = o + l;
            if be > text.chars().count() {
                continue;
            }
            if spans.iter().any(|(a, b, _, _)| *a < be && o < *b) {
                continue;
            }
            spans.push((o, be, rep, cat));
        }
    }
    spans.sort_by_key(|(bs, _, _, _)| *bs);
    let mut out = vec![];
    for (bs, be, rep, cat) in spans {
        if let Some((x0, y0, x1, y1, phrase)) = span_to_box(words, &ranges, bs, be) {
            out.push(Misspelling { word: phrase, sug: rep, page, x0, y0, x1, y1, dismissed: false, kind: kind_of_lt(&cat).into(), sev: sev_of_lt(&cat).into() });
        }
    }
    out
}

/// Presta el estado, ejecuta `f` y **suelta el préstamo antes de devolver**.
/// Motivo: un `RefMut` creado en el scrutinee de un `match` vive hasta el
/// final del `match`; si dentro de las ramas se vuelve a prestar, el
/// `RefCell` peta ("RefCell already borrowed") y la app se cae.
fn with_state<T, R>(c: &Rc<RefCell<T>>, f: impl FnOnce(&mut T) -> R) -> R {
    f(&mut c.borrow_mut())
}

/// Toast de 4s para mensajes transitorios; el `status` queda para info persistente.
fn notify(ui: &AppWindow, st: &Rc<RefCell<State>>, msg: String) {
    ui.set_popup_text(msg.into());
    st.borrow_mut().popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
}

/// ¿Se pisan dos boxes de la misma página? (p. ej. ortografía "de" y sintaxis "de de")
fn boxes_overlap(a: &Misspelling, b: &Misspelling) -> bool {
    a.page == b.page && a.x0 < b.x1 && b.x0 < a.x1 && a.y0 < b.y1 && b.y0 < a.y1
}

/// Orden de aparición: página asc, luego de arriba abajo (y1 desc),
/// luego de izquierda a derecha. Puro y testeable.
fn err_order(a: &Misspelling, b: &Misspelling) -> std::cmp::Ordering {
    a.page
        .cmp(&b.page)
        .then(b.y1.total_cmp(&a.y1))
        .then(a.x0.total_cmp(&b.x0))
}

fn push_errs(ui: &AppWindow, st: &Rc<RefCell<State>>) {
    let en = ui.get_lang_idx() == 1;
    let (all, expanded, err_anno) = {
        let s = st.borrow();
        (s.all_errs.clone(), s.expanded.clone(), s.err_anno.clone())
    };
    // índices filtrados en orden de aparición
    // tipo: 0 todas · 1 ortografía · 2 sintaxis | sev: 0 todas · 1 grave · 2 intermedia · 3 mínima
    let (tfilter, sfilter) = (ui.get_type_filter(), ui.get_sev_filter());
    let mut filt: Vec<usize> = vec![];
    for (gi, e) in all.iter().enumerate() {
        if tfilter == 1 && e.kind != "Ortografía"
            || tfilter == 2 && e.kind != "Sintaxis"
            || sfilter == 1 && e.sev != "grave"
            || sfilter == 2 && e.sev != "intermedia"
            || sfilter == 3 && e.sev != "mínima"
        {
            continue;
        }
        filt.push(gi);
    }
    // orden de aparición en el documento (no orden de llegada de los hilos)
    filt.sort_by(|a, b| err_order(&all[*a], &all[*b]));
    // conteo por palabra para agrupar 5+
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for gi in &filt {
        *counts.entry(all[*gi].word.as_str()).or_insert(0) += 1;
    }
    let mut view = vec![];
    let mut view_group: Vec<Option<String>> = vec![];
    let mut rows = vec![];
    let mut grouped_done: HashSet<&str> = HashSet::new();
    for gi in filt {
        let e = &all[gi];
        let n = counts.get(e.word.as_str()).copied().unwrap_or(1);
        let grouped = n >= 5 && !expanded.contains(&e.word);
        if grouped {
            if !grouped_done.insert(e.word.as_str()) {
                continue; // una sola card por grupo
            }
            // primera ocurrencia como lead; sug del cache o del primer miembro con sugs
            let lead = gi;
            let sug = if !e.sug.is_empty() {
                e.sug.clone()
            } else {
                all.iter().find(|x| x.word == e.word && !x.sug.is_empty()).map(|x| x.sug.clone()).unwrap_or_default()
            };
            let dimmed = all.iter().filter(|x| x.word == e.word).all(|x| x.dismissed);
            // gravedad del grupo = la peor de sus miembros
            let gsev = worst_sev(all.iter().filter(|x| x.word == e.word).map(|x| x.sev.as_str()));
            view.push(lead);
            view_group.push(Some(e.word.clone()));
            let gdesc = desc_for(&e.kind, en);
            let gsug = if sug.is_empty() {
                (if en { "No suggestions" } else { "No hay sugerencias" }).to_string()
            } else {
                format!("→ {}", sug.join(", "))
            };
            rows.push(ErrRow {
                word: format!("{} (×{n})", e.word).into(),
                sug: gsug.clone().into(),
                page: e.page as i32,
                dimmed,
                kind: e.kind.clone().into(),
                sev: gsev.into(),
                bar: kind_bar(gsev),
                desc: gdesc.clone().into(),
                count: n as i32,
                is_group: true,
                has_anno: all.iter().enumerate().any(|(k, x)| x.word == e.word && err_anno.contains_key(&k)),
            });
        } else {
            view.push(gi);
            view_group.push(None);
            rows.push(ErrRow {
                word: e.word.clone().into(),
                sug: if e.sug.is_empty() {
                    (if en { "No suggestions" } else { "No hay sugerencias" }).into()
                } else {
                    format!("→ {}", e.sug.join(", ")).into()
                },
                page: e.page as i32,
                dimmed: e.dismissed,
                kind: e.kind.clone().into(),
                sev: e.sev.clone().into(),
                bar: kind_bar(&e.sev),
                desc: desc_for(&e.kind, en).into(),
                count: n as i32,
                is_group: false,
                has_anno: err_anno.contains_key(&gi),
            });
        }
    }
    st.borrow_mut().view = view;
    st.borrow_mut().view_group = view_group;
    ui.set_errors(ModelRc::new(VecModel::from_slice(&rows)));
    if rows.is_empty() {
        // ventana en blanco con causa: sin motor no hay nada que listar
        let s = st.borrow();
        if s.lt_dead {
            ui.set_spell_status("Sin motor: instala Java 17+ o coloca lt/ junto al programa.".into());
        } else if !s.scan_queue.is_empty() && s.scan_pos >= s.scan_queue.len() {
            if s.lt_ready && !s.syn_received {
                ui.set_spell_status("Sin errores (LT no devolvió ninguno).".into());
            } else {
                ui.set_spell_status("Sin errores.".into());
            }
        }
        return;
    }
    let graves = all.iter().filter(|e| e.sev == "grave").count();
    ui.set_spell_status(format!("{} errores ({} grave)", rows.len(), graves).into());
    let s = st.borrow();
    if s.lt_ready {
        ui.set_lt_status(sec_line(s.sec_bytes / 1024).into());
    }
}

/// Prepara el escaneo TODO el documento; el Timer lo avanza por trozos.
/// Orden: desde la página visible (resultados útiles primero).
/// ponytail: troceado en hilo UI en vez de segundo hilo con otro Pdfium (se colgaba en bind)
fn start_scan(ui: &AppWindow, st: &mut State) {
    let total = st.total;
    if st.path.is_none() || total == 0 {
        return;
    }
    st.scan_id += 1;
    st.all_errs.clear();
    st.view.clear();
    st.view_group.clear();
    st.scan_queue = (st.page..total).chain(0..st.page).collect();
    st.scan_pos = 0;
    st.pending = 0;
    st.jobs_total = 0;
    st.sec_bytes = 0;
    st.last_lang = ui.get_lang_idx();
    ui.set_errors(ModelRc::new(VecModel::from_slice(&[])));
    ui.set_scanning(true);
    ui.set_scan_progress(0);
    ui.set_scan_text("Escaneando…".into());
    ui.set_spell_status("…".into());
}

/// Avanza la extracción con presupuesto de 80ms por tick y lanza el chequeo
/// LT en fondo (un hilo por página, solo HTTP). Sin servidor no avanza:
/// sin motor no hay nada que mostrar.
/// Devuelve true si llegaron hits de la página visible (hay que re-subrayar).
fn scan_tick(ui: &AppWindow, st: &Rc<RefCell<State>>) -> bool {
    {
        let s = st.borrow();
        if s.path.is_none() {
            return false;
        }
        if !s.lt_ready && !s.lt_dead {
            // motor arrancando: relleno por tiempo (máx 90s de espera), solo izq→der
            ui.set_scanning(true);
            ui.set_scan_progress(boot_pct(s.boot_start.elapsed().as_secs()));
            ui.set_scan_text("Cargando diccionarios…".into());
            return false;
        }
        if s.scan_pos >= s.scan_queue.len() {
            // cola vacía pero hilos en vuelo: % directo con las respuestas
            let (pct, pend) = (progress_state(s.scan_queue.len() as u32, s.scan_queue.len() as u32, s.jobs_total, s.pending).0 as i32, s.pending);
            drop(s);
            if pend > 0 {
                ui.set_scan_progress(pct);
                ui.set_scanning(true);
                ui.set_scan_text(format!("Revisando… {} pág. pendientes", pend).into());
            }
            return false;
        }
    }
    // sin LT solo queda esperar al sidecar; no se consume cola para no perder páginas.
    // Si LT murió (lt_dead), se consume igual para que la barra termine (lista vacía).
    let dead = st.borrow().lt_dead;
    let mode = ui.get_lang_idx() + 1; // UI 0=ES,1=EN → LT 1=es,2=en
    let t = std::time::Instant::now();
    let mut jobs: Vec<(Vec<Word>, u32, i32, u64, Sender<SpellMsg>)> = vec![];
    {
        let mut s = st.borrow_mut();
        while s.scan_pos < s.scan_queue.len() && t.elapsed() < std::time::Duration::from_millis(80) {
            let i = s.scan_queue[s.scan_pos];
            s.scan_pos += 1;
            // el doc sigue abierto: no se reabre por página (solo se renderiza)
            let words = match s.doc.as_ref() {
                Some(doc) => match doc.pages().get(i as u16) {
                    Ok(pg) => extract_words(&pg),
                    Err(_) => vec![],
                },
                None => vec![],
            };
            if !dead {
                jobs.push((words, i, mode, s.scan_id, s.tx.clone()));
            }
        }
        refresh_progress(ui, &s);
    }
    let launched = !jobs.is_empty();
    if launched {
        st.borrow_mut().pending += jobs.len() as u32;
        st.borrow_mut().jobs_total += jobs.len() as u32;
        st.borrow_mut().sec_bytes += jobs.iter().map(|(w, _, _, _, _)| w.iter().map(|x| x.text.len() as u64).sum::<u64>()).sum::<u64>();
    }
    for (words, pg, mode, gen, tx) in jobs {
        std::thread::spawn(move || {
            let hits = syntax_page(&words, pg, mode);
            // siempre responde (aunque vacío) para cerrar el pendiente
            let _ = tx.send(SpellEvent::Syn(gen, hits));
        });
    }
    if launched {
        push_errs(ui, st);
    }
    false
}

/// Barra única en dos tramos reales: 0→50 extrayendo, 50→100 respondiendo.
/// % directo, sin animaciones: lo que ves es lo que hay.
fn refresh_progress(ui: &AppWindow, s: &State) {
    let (len, done) = (s.scan_queue.len() as u32, s.scan_pos.min(s.scan_queue.len()) as u32);
    let (pct, finished) = progress_state(len, done, s.jobs_total, s.pending);
    ui.set_scan_progress(pct as i32);
    if finished {
        ui.set_scanning(false);
        ui.set_scan_text("".into());
    } else {
        ui.set_scan_text(format!("{pct}% · pág. {done} de {len}…").into());
    }
}

/// Relleno del arranque por tiempo: 0→99 en ~90s (tope de la espera), nunca en bucle.
/// Puro y testeable.
fn boot_pct(elapsed_secs: u64) -> i32 {
    (elapsed_secs * 100 / 90).min(99) as i32
}

/// (porcentaje, terminada): terminada solo con cola vacía Y sin hilos en vuelo.
/// Pura para poder testearla sin UI.
fn progress_state(len: u32, done: u32, jobs: u32, pending: u32) -> (u32, bool) {
    if len == 0 {
        return (100, false);
    }
    let done = done.min(len);
    if done < len {
        return (done * 50 / len, false); // tramo extracción
    }
    if pending == 0 {
        return (100, true);
    }
    if jobs == 0 {
        return (100, true);
    }
    (50 + (jobs - pending) * 50 / jobs, false) // tramo respuestas
}

/// Agrupa chars de Pdfium en palabras con su bounding box unión.
fn extract_words(page: &PdfPage) -> Vec<Word> {
    let text = match page.text() {
        Ok(t) => t,
        Err(_) => return vec![],
    };
    let mut words = vec![];
    let mut cur = String::new();
    let mut x0 = f32::MAX;
    let mut y0 = f32::MAX;
    let mut x1 = f32::MIN;
    let mut y1 = f32::MIN;
    let mut flush = |cur: &mut String, words: &mut Vec<Word>, x0: &mut f32, y0: &mut f32, x1: &mut f32, y1: &mut f32| {
        let t = cur.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-').to_string();
        if t.chars().any(|c| c.is_alphabetic()) {
            words.push(Word { text: t, x0: *x0, y0: *y0, x1: *x1, y1: *y1 });
        }
        cur.clear();
        *x0 = f32::MAX; *y0 = f32::MAX; *x1 = f32::MIN; *y1 = f32::MIN;
    };
    for ch in text.chars().iter() {
        let c = match ch.unicode_char() {
            Some(c) => c,
            None => continue,
        };
        if c.is_whitespace() {
            if !cur.is_empty() {
                flush(&mut cur, &mut words, &mut x0, &mut y0, &mut x1, &mut y1);
            }
            continue;
        }
        let r = ch.loose_bounds().or_else(|_| ch.tight_bounds());
        let r = match r {
            Ok(r) => r,
            Err(_) => continue,
        };
        // ponytail: unión simple de boxes, sin shaping tipográfico; refinar si el subrayado se desalinea
        x0 = x0.min(r.left().value);
        y0 = y0.min(r.bottom().value);
        x1 = x1.max(r.right().value);
        y1 = y1.max(r.top().value);
        cur.push(c);
    }
    if !cur.is_empty() {
        flush(&mut cur, &mut words, &mut x0, &mut y0, &mut x1, &mut y1);
    }
    words
}

/// Subrayado quemado en el bitmap (siempre alineado, sin mates de layout).
/// Dibuja 3px al pie del box, mezcla 65% del color dado sobre el fondo.
/// Ortografía = rojo #d32f2f, sintaxis = morado #6a1b9a.
fn underline(img: &mut image::RgbaImage, x: f32, y: f32, w: f32, col: (u8, u8, u8)) {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let (x0, y0) = (x.round() as i64, y.round() as i64);
    for dx in 0..(w.round() as i64).max(4) {
        for dy in 0..3i64 {
            let (px, py) = (x0 + dx, y0 + dy);
            if px < 0 || py < 0 || px >= iw || py >= ih {
                continue;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            p[0] = (p[0] as u16 * 35 / 100 + col.0 as u16 * 65 / 100) as u8;
            p[1] = (p[1] as u16 * 35 / 100 + col.1 as u16 * 65 / 100) as u8;
            p[2] = (p[2] as u16 * 35 / 100 + col.2 as u16 * 65 / 100) as u8;
        }
    }
}

fn under_color(kind: &str) -> (u8, u8, u8) {
    if kind == "Sintaxis" { (106, 27, 154) } else { (211, 47, 47) }
}

fn wash(img: &mut image::RgbaImage, x: f32, y: f32, w: f32, h: f32) {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    for dx in 0..(w.round() as i64).max(4) {
        for dy in 0..(h.round() as i64).max(1) {
            let (px, py) = (x.round() as i64 + dx, y.round() as i64 + dy);
            if px < 0 || py < 0 || px >= iw || py >= ih {
                continue;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            p[0] = (p[0] as u16 * 45 / 100 + 255 * 55 / 100) as u8;
            p[1] = (p[1] as u16 * 45 / 100 + 235 * 55 / 100) as u8;
            p[2] = (p[2] as u16 * 45 / 100 + 59 * 55 / 100) as u8;
        }
    }
}
fn pdf_to_image(x0: f32, y0: f32, x1: f32, y1: f32, pw: f32, ph: f32, iw: u32, ih: u32) -> (f32, f32, f32, f32) {
    let sx = iw as f32 / pw;
    let sy = ih as f32 / ph;
    let x = x0 * sx;
    let w = (x1 - x0) * sx;
    let y = (ph - y1) * sy;
    let h = (y1 - y0) * sy;
    (x, y, w, h)
}

/// Guarda evitando truncar un archivo que Pdfium tiene mapeado
/// (cuando base == destino): vía temporal + rename atómico.
fn save_doc(doc: &PdfDocument, out: &PathBuf) -> anyhow::Result<()> {
    let tmp = out.with_extension("tmp.pdf");
    let r: anyhow::Result<()> = (|| {
        doc.save_to_file(&tmp).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        std::fs::rename(&tmp, out)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp); // sin restos de la copia íntegra
    }
    r
}

/// Métrica visible: KB de texto revisado (siempre en loopback, ver tripwire).
fn sec_line(kb: u64) -> String {
    format!("Motor: listo (local, sin red · {kb} KB revisados sin salir del PC).")
}

/// Lee las sticky-notes (Text) ya existentes en un fichero (solo tests).
#[cfg(test)]
fn read_notes(pdfium: &Pdfium, path: &PathBuf) -> Vec<DocNote> {
    let Ok(doc) = pdfium.load_pdf_from_file(path, None) else { return vec![] };
    notes_of(&doc)
}

/// Las anotaciones de un documento abierto (sticky notes y resaltados).
fn notes_of(doc: &PdfDocument) -> Vec<DocNote> {
    let mut out = vec![];
    let pages = doc.pages();
    for i in 0..pages.len() {
        let Ok(pg) = pages.get(i) else { continue };
        for ann in pg.annotations().iter() {
            let kind = match ann.annotation_type() {
                PdfPageAnnotationType::Text => AnnoKind::Note,
                PdfPageAnnotationType::Highlight => AnnoKind::Highlight,
                _ => continue,
            };
            let Some(text) = ann.contents() else { continue };
            if text.trim().is_empty() {
                continue;
            }
            let (x0, y0, x1, y1) = match ann.bounds() {
                Ok(b) => (b.left().value, b.bottom().value, b.right().value, b.top().value),
                Err(_) => continue,
            };
            out.push(DocNote { page: i as u32, x0, y0, x1, y1, text, kind });
        }
    }
    out
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct HisEntry {
    path: String,
    pages: u32,
    errors: usize,
    resolved: usize, // omitidos
    notes: usize,    // notas nativas en el PDF
    mtime: i64,
}

/// `~/.config/lexpdf/history.json` (10 últimos).
fn history_file() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".config/lexpdf/history.json"))
}

/// Una entrada manipulada no tumba el resto: valida campo por campo.
/// Pura y testeable.
fn sanitize_history(v: Vec<serde_json::Value>) -> Vec<HisEntry> {
    let num = |e: &serde_json::Value, k: &str| e.get(k).and_then(|x| x.as_u64());
    v.into_iter()
        .filter_map(|e| {
            let path = e.get("path").and_then(|x| x.as_str())?;
            if path.is_empty() || path.len() > 1024 || path.contains('\0') {
                return None;
            }
            Some(HisEntry {
                path: path.to_string(),
                pages: num(&e, "pages").filter(|&n| n > 0 && n < 100_000)? as u32,
                errors: num(&e, "errors").filter(|&n| n < 1_000_000)? as usize,
                resolved: num(&e, "resolved").filter(|&n| n < 1_000_000)? as usize,
                notes: num(&e, "notes").filter(|&n| n < 1_000_000)? as usize,
                mtime: e.get("mtime").and_then(|x| x.as_i64()).filter(|&t| t >= 0)?,
            })
        })
        .collect()
}

fn load_history() -> Vec<HisEntry> {
    let Some(f) = history_file() else { return vec![] };
    let Ok(b) = std::fs::read_to_string(&f) else { return vec![] };
    let raw: Vec<serde_json::Value> = serde_json::from_str(&b).unwrap_or_default();
    let mut v = sanitize_history(raw);
    v.retain(|e| PathBuf::from(&e.path).is_file());
    v.truncate(10);
    v
}

/// Guarda la foto de progreso del documento actual al frente (máx 10).
fn touch_history(st: &State) {
    let Some(f) = history_file() else { return };
    let Some(path) = st.path.clone() else { return };
    let mut v: Vec<HisEntry> = std::fs::read_to_string(&f)
        .ok()
        .and_then(|b| serde_json::from_str::<Vec<serde_json::Value>>(&b).ok())
        .map(sanitize_history)
        .unwrap_or_default();
    v.retain(|e| e.path != path.to_string_lossy());
    v.insert(0, HisEntry {
        path: path.to_string_lossy().into_owned(),
        pages: st.total,
        errors: st.all_errs.len(),
        resolved: st.all_errs.iter().filter(|e| e.dismissed).count(),
        notes: st.doc_notes.len(),
        mtime: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    });
    v.truncate(10);
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
        // el historial guarda nombres de ficheros: solo su usuario debe leerlo
        #[cfg(unix)]
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    let _ = std::fs::write(&f, serde_json::to_string(&v).unwrap_or_default());
    #[cfg(unix)]
    let _ = std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600));
}

fn push_history(ui: &AppWindow) {
    let rows: Vec<HisRow> = load_history()
        .iter()
        .map(|e| {
            let name = PathBuf::from(&e.path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .into();
            let pct = if e.errors == 0 { 100 } else { e.resolved * 100 / e.errors };
            HisRow {
                name,
                info: format!("{} págs · {} errores · {} notas · {}%", e.pages, e.errors, e.notes, pct).into(),
            }
        })
        .collect();
    ui.set_history(ModelRc::new(VecModel::from_slice(&rows)));
}

/// Ajustes del usuario (ventana aparte). Se guardan en
/// `~/.config/lexpdf/settings.json` (0600, como el historial).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)] // si falta un campo (ajustes viejos) se usa el de por defecto
struct Settings {
    /// 0 = color por gravedad (rojo/morado) · 1 = amarillo único
    hl_color: u8,
    /// 0 = preguntar al abrir · 1 = crear siempre · 2 = nunca
    copy_mode: u8,
    /// false = no se envía texto a ningún motor (sin revisión)
    motor: bool,
    /// resaltado libre: 0 = por área (rectángulo), 1 = por texto
    free_sel: u8,
    /// con free_sel=1: 0 = por recuadro (una caja por palabra), 1 = selección
    /// real de texto (una banda continua por línea)
    free_text: u8,
    /// avisos (toast): 0 = arriba · 1 = abajo
    toast_pos: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { hl_color: 0, copy_mode: 0, motor: true, free_sel: 0, free_text: 1, toast_pos: 0 }
    }
}

fn settings_file() -> Option<PathBuf> {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()?;
    Some(PathBuf::from(home).join(".config/lexpdf/settings.json"))
}

fn load_settings() -> Settings {
    let Some(f) = settings_file() else { return Settings::default() };
    let Ok(b) = std::fs::read_to_string(&f) else { return Settings::default() };
    serde_json::from_str(&b).unwrap_or_default()
}

fn save_settings(s: &Settings) {
    let Some(f) = settings_file() else { return };
    if let Some(dir) = f.parent() {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        let _ = std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    }
    let _ = std::fs::write(&f, serde_json::to_string(s).unwrap_or_default());
    #[cfg(unix)]
    let _ = std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o600));
}

/// Una dependencia y su licencia (ventana "Licencias").
#[derive(Clone)]
struct LicInfo {
    name: &'static str,
    lic: &'static str,
    repo: &'static str,
    files: &'static [&'static str],
}

/// Todo lo que lleva dentro la app, con su licencia y su repo.
/// `files` son nombres de `assets/licenses/` (se copian junto al programa).
fn license_entries() -> Vec<LicInfo> {
    vec![
        LicInfo { name: "LexPDF (esta app)", lic: "GPL-3.0-only", repo: "https://github.com/Makinnt/LexPDF", files: &["GPL3-LexPDF.txt"] },
        LicInfo { name: "Slint (interfaz)", lic: "GPL-3.0-only", repo: "https://github.com/slint-ui/slint", files: &["GPL3-Slint.txt"] },
        LicInfo { name: "PDFium (motor de render)", lic: "BSD-3-Clause", repo: "https://github.com/pdfium/pdfium", files: &["BSD3-PDFium.txt"] },
        LicInfo { name: "pdfium-render", lic: "MIT", repo: "https://github.com/ajrcarey/pdfium-render", files: &["MIT-pdfium-render.txt"] },
        LicInfo { name: "LanguageTool (motor de revisión)", lic: "LGPL-2.1", repo: "https://github.com/languagetool-org/languagetool", files: &["LGPL21-LT.txt"] },
        LicInfo { name: "lopdf", lic: "MIT", repo: "https://github.com/J-F-Liu/lopdf", files: &["MIT-lopdf.txt"] },
        LicInfo { name: "ureq (HTTP al sidecar)", lic: "MIT", repo: "https://github.com/algesten/ureq", files: &["MIT-ureq.txt"] },
        LicInfo { name: "image", lic: "MIT", repo: "https://github.com/image-rs/image", files: &["MIT-image.txt"] },
        LicInfo { name: "rfd (diálogo de archivos)", lic: "MIT", repo: "https://github.com/PolyMeilex/rfd", files: &["MIT-rfd.txt"] },
        LicInfo { name: "anyhow", lic: "MIT", repo: "https://github.com/dtolnay/anyhow", files: &["MIT-anyhow.txt"] },
        LicInfo { name: "serde / serde_json", lic: "MIT", repo: "https://github.com/serde-rs/serde", files: &["MIT-serde.txt"] },
        LicInfo { name: "Material Icons Outlined", lic: "Apache-2.0", repo: "https://github.com/google/material-design-icons", files: &["Apache2-icons.txt"] },
    ]
}

/// Busca un fichero de `assets/licenses/`: junto al programa (empaquetado),
/// en el AppImage (usr/share/lexpdf) o en el repo (desarrollo).
fn asset_file(name: &str) -> Option<PathBuf> {
    let mut cands: Vec<PathBuf> = vec![];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join("licenses").join(name));
            cands.push(d.join("assets/licenses").join(name));
            cands.push(d.join("../share/lexpdf/licenses").join(name));
        }
    }
    cands.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/licenses").join(name));
    cands.into_iter().find(|c| c.is_file())
}

/// Alto estimado del texto de una licencia (px) a 11px de fuente en una
fn license_height(text: &str) -> i32 {
    let chars = text.chars().count() as f32;
    let lineas = text.lines().count() as f32;
    (((chars / 90.0 + lineas) * 14.0) as i32 + 40).min(60000)
}

/// Texto completo de una licencia (varios ficheros = licencia dual).
fn license_text(files: &[&str]) -> String {
    let mut out = String::new();
    for f in files {
        match asset_file(f) {
            Some(p) => match std::fs::read_to_string(&p) {
                Ok(t) => out.push_str(&format!("===== {} =====\n\n{}", f, t)),
                Err(e) => out.push_str(&format!("(no se pudo leer {f}: {e})\n")),
            },
            None => out.push_str(&format!("(no se encontró {f})\n")),
        }
        out.push_str("\n\n");
    }
    out
}

/// Abre una URL en el navegador del sistema (best effort).
fn open_link(url: &str) {
    let mut c = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg("start").arg("").arg(url);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    c.stdout(std::process::Stdio::null());
    c.stderr(std::process::Stdio::null());
    let _ = c.spawn();
}

/// Rellena la capa de licencias con la entrada i de la lista.
fn lic_select(ui: &AppWindow, i: i32, entries: &[LicInfo]) {
    let Some(e) = entries.get(i as usize) else { return };
    ui.set_p_lic_sel(i);
    ui.set_p_lic_name(e.name.into());
    ui.set_p_lic_lic(e.lic.into());
    ui.set_p_lic_repo(e.repo.into());
    let t = license_text(e.files);
    ui.set_p_lic_h(license_height(&t));
    ui.set_p_lic_text(t.into());
}

/// Parchea los resaltados que se han quedado sin `/QuadPoints`: pdfium no puede
/// ponerlos (sin ellos no se dibuja el resaltado en ningún lector) ni la
/// transparencia. lopdf mete ambas cosas en el fichero ya guardado.
///
/// Repasa **todas** las anotaciones Highlight del documento (no solo las
/// nuevas): si al guardar con pdfium se pierden los quad points de los
/// resaltados que ya estaban, aquí se recuperan (si no, se ve el comentario
/// pero el resaltado disappears o al revés).
/// Gotchas: lopdf numera las páginas desde 1 y pdfium guarda las anotaciones
/// *inline* en el array /Annots (hay que reescribir el array).
fn patch_highlights(path: &std::path::Path) -> anyhow::Result<usize> {
    let mut doc = lopdf::Document::load(path)?;
    let pages = doc.get_pages();
    let mut patched = 0;
    for page_id in pages.values() {
        let page_obj = doc.get_object(*page_id)?;
        let Some(lopdf::Object::Array(list)) = lget(&page_obj, b"Annots") else {
            continue;
        };
        let mut new_list = Vec::with_capacity(list.len());
        for a in list {
            let is_ref = matches!(a, lopdf::Object::Reference(_));
            let mut obj = match &a {
                lopdf::Object::Reference(id) => doc.get_object(*id)?.clone(),
                other => other.clone(),
            };
            let is_hl = lget(&obj, b"Subtype") == Some(lopdf::Object::Name(b"Highlight".to_vec()));
            let sin_quads = lget(&obj, b"QuadPoints").is_none();
            if is_hl && sin_quads {
                if let Some(lopdf::Object::Array(v)) = lget(&obj, b"Rect") {
                    let nums: Vec<f32> = v.iter().filter_map(|o| o.as_float().ok()).collect();
                    if nums.len() == 4 {
                        let (x0, y0, x1, y1) = (nums[0], nums[1], nums[2], nums[3]);
                        // ORDEN DEL SPEC: UL, UR, LL, LR. Con el orden inverso
                        // pdfium pinta 27 px (un punto) en vez de la palabra
                        // entera: comprobado midiendo pixeles.
                        let qp = lopdf::Object::Array(vec![
                            lopdf::Object::Real(x0), lopdf::Object::Real(y1),
                            lopdf::Object::Real(x1), lopdf::Object::Real(y1),
                            lopdf::Object::Real(x0), lopdf::Object::Real(y0),
                            lopdf::Object::Real(x1), lopdf::Object::Real(y0),
                        ]);
                        if let lopdf::Object::Dictionary(d) = &mut obj {
                            d.set(b"QuadPoints", qp);
                            d.set(b"CA", lopdf::Object::Real(0.4));
                        }
                        if is_ref {
                            let id = match &a { lopdf::Object::Reference(i) => *i, _ => unreachable!() };
                            *doc.get_object_mut(id)? = obj.clone();
                        }
                        patched += 1;
                    }
                }
            }
            new_list.push(if is_ref { a } else { obj });
        }
        if let lopdf::Object::Dictionary(d) = doc.get_object_mut(*page_id)? {
            d.set(b"Annots", lopdf::Object::Array(new_list));
        }
    }
    if patched > 0 {
        // lopdf reescribe el fichero: se escribe a un temporal y se renombra
        // (si no, pdfium lo tiene mapeado y se rompe)
        let tmp = path.with_extension("patch.pdf");
        doc.save(&tmp)?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(patched)
}

/// `Object::get` de lopdf choca con el trait del prelude de pdfium: se accede
/// al diccionario directamente.
fn lget(o: &lopdf::Object, key: &[u8]) -> Option<lopdf::Object> {
    match o {
        lopdf::Object::Dictionary(d) => d.get(key).ok().cloned(),
        _ => None,
    }
}

/// ¿Los dos rects son el mismo (con margen)?
fn same_rect(a: &[f32], b: &[f32; 4]) -> bool {
    a.len() == 4
        && (a[0] - b[0]).abs() < 0.5
        && (a[1] - b[1]).abs() < 0.5
        && (a[2] - b[2]).abs() < 0.5
        && (a[3] - b[3]).abs() < 0.5
}

/// Solo PDFs: extensión + magia `%PDF-` (la extensión sola se falsifica
/// renombrando; el filtro del diálogo no cubre argv ni historial).
/// Puro y testeable.
fn is_pdf(path: &PathBuf) -> bool {
    let ext_ok = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf"));
    if !ext_ok {
        return false;
    }
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut magic = [0u8; 5];
    use std::io::Read;
    f.read_exact(&mut magic).is_ok_and(|_| &magic == b"%PDF-")
}

/// Abre un documento por ruta (diálogo, argv o historial): misma tubería.
/// La copia de trabajo "{nombre}_anotado.pdf" es la fuente de verdad si ya
/// existe (nunca se crea un "_anotado_anotado").
fn open_doc(ui: &AppWindow, st: &Rc<RefCell<State>>, path: PathBuf) {
    if !is_pdf(&path) {
        ui.set_popup_text("Solo se admiten documentos PDF.".into());
        st.borrow_mut().popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        return;
    }
    let wp = working_path(&path);
    let working = if wp.is_file() { Some(wp) } else { None };
    {
        let mut s = st.borrow_mut();
        s.path = Some(path.clone());
        s.working = working;
        s.doc = None;
        s.doc_from = None;
        s.doc_stamp = None;
        s.doc_notes.clear();
        s.all_errs.clear();
        s.view.clear();
        s.view_group.clear();
        s.flash = None;
        s.free_pt = None;
    }
    ui.set_has_working(st.borrow().working.is_some());
    let src = st.borrow().render_from().unwrap_or_else(|| path.clone());
    let abierto = with_state(st, |s| open_doc_file(s, &src));
    match abierto {
        Ok(()) => {}
        Err(e) if is_password_error(&e) => {
            // PDF protegido: se pide la contraseña (solo en memoria)
            st.borrow_mut().dlg_pending = Some(Pending::OpenWithPassword(path.clone()));
            show_dlg(
                ui,
                1,
                "PDF con contraseña",
                "Este documento está protegido. Escribe la contraseña para abrirlo (no se guarda en ningún sitio).",
            );
            return;
        }
        Err(e) => {
            notify(ui, st, format!("No se pudo abrir: {e:?}"));
            return;
        }
    }
    finish_open(ui, st, &path);
}

/// Tras abrir el documento: notas, vista, escaneo e historial.
fn finish_open(ui: &AppWindow, st: &Rc<RefCell<State>>, path: &PathBuf) {
    {
        let mut s = st.borrow_mut();
        if let Some(doc) = s.doc.as_ref() {
            s.doc_notes = notes_of(doc);
        }
    }
    show(ui, &mut st.borrow_mut(), 0);
    start_scan(ui, &mut st.borrow_mut());
    touch_history(&st.borrow());
    push_history(ui);
    // sin copia de trabajo: se pregunta si se quiere crear (las notas van allí)
    let mode = st.borrow().settings.copy_mode;
    if st.borrow().working.is_none() {
        if mode == 1 {
            // "crear siempre": se hace sin preguntar
            if let Err(e) = make_working(ui, st) {
                notify(ui, st, format!("No se pudo crear la copia: {e:#}"));
            }
            return;
        }
        if mode == 0 {
            let wp = working_path(path);
            let name = wp.file_name().and_then(|n| n.to_str()).unwrap_or("documento_anotado.pdf");
            st.borrow_mut().dlg_pending = Some(Pending::CreateCopy);
            show_dlg(
                ui,
                0,
                "Copia de trabajo",
                &format!(
                    "¿Crear la copia \"{name}\"? Tus notas se guardarán ahí; el original no se toca."
                ),
            );
        }
    }
}

/// Muestra un diálogo propio (0 = confirmar, 1 = contraseña).
fn show_dlg(ui: &AppWindow, kind: i32, title: &str, msg: &str) {
    ui.set_dlg_kind(kind);
    ui.set_dlg_title(title.into());
    ui.set_dlg_msg(msg.into());
    ui.set_dlg_pass("".into());
    ui.set_dlg_open(true);
}

/// Crea la copia de trabajo y reabre el documento desde ella.
fn make_working(ui: &AppWindow, st: &Rc<RefCell<State>>) -> anyhow::Result<()> {
    let out = create_working_copy(&mut st.borrow_mut())?;
    st.borrow_mut().working = Some(out);
    let src = st.borrow().render_from().context("sin documento")?;
    open_doc_file(&mut st.borrow_mut(), &src).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ui.set_has_working(true);
    Ok(())
}

/// Resultado de intentar guardar una nota (decide si el panel se cierra).
#[derive(PartialEq)]
enum NoteResult {
    Saved,   // guardada y visible
    Pending, // hay un diálogo en marcha (falta la copia de trabajo)
    Failed,  // error: se puede reintentar
}

/// Aplica una anotación y la deja visible al momento. Si no hay copia de

/// Aplica una anotación y la deja visible al momento. Si no hay copia de
/// trabajo, se pregunta antes de crearla (y se devuelve `Pending`).
fn apply_note(ui: &AppWindow, st: &Rc<RefCell<State>>, req: AnnotateReq) -> NoteResult {
    if st.borrow().working.is_none() {
        st.borrow_mut().dlg_pending = Some(Pending::CreateCopyThenNote(req));
        show_dlg(
            ui,
            0,
            "Copia de trabajo",
            "Para guardar la nota hace falta una copia \"_anotado.pdf\". El original no se toca. ¿Se crea?",
        );
        return NoteResult::Pending;
    }
    let res = with_state(st, |s| annotate_open(s, &req));
    match res {
        Ok(added) => {
            {
                // se apunta qué anotación corresponde a qué error (para "Borrar")
                let mut s = st.borrow_mut();
                let first = s.doc_notes.len();
                s.doc_notes.extend(added);
                if let AnnotateReq::Highlight(idxs, _) = req {
                    let claves: Vec<(usize, u32, [f32; 4])> = idxs
                        .iter()
                        .enumerate()
                        .filter_map(|(k, gi)| {
                            s.doc_notes.get(first + k).map(|n| (*gi, n.page, [n.x0, n.y0, n.x1, n.y1]))
                        })
                        .collect();
                    for (gi, page, rect) in claves {
                        s.err_anno.insert(gi, (page, rect));
                    }
                }
            }
            let pg = st.borrow().page;
            show(ui, &mut st.borrow_mut(), pg); // la nota se ve al momento
            push_page_notes(ui, &st.borrow());
            push_errs(ui, st); // la card muestra "Borrar"
            touch_history(&st.borrow());
            push_history(ui);
            notify(ui, st, "Nota guardada en la copia de trabajo".into());
            NoteResult::Saved
        }
        Err(e) => {
            notify(ui, st, format!("Error al anotar: {e:#}"));
            NoteResult::Failed
        }
    }
}

/// Palabras de la página que toca el rectángulo (resaltado por texto).
/// Puro y testeable.
fn words_in_rect(words: &[Word], r: [f32; 4]) -> Vec<[f32; 4]> {
    let (x0, y0, x1, y1) = (r[0], r[1], r[2], r[3]);
    words
        .iter()
        .filter(|w| w.x0 < x1 && x0 < w.x1 && w.y0 < y1 && y0 < w.y1)
        .map(|w| [w.x0, w.y0, w.x1, w.y1])
        .collect()
}

/// Texto de las palabras que caen dentro del rectángulo, en orden de lectura
/// (línea por línea, de izquierda a derecha): lo que el usuario ha selecciónado
/// de verdad. Puro y testeable.
fn text_in_rect(words: &[Word], r: [f32; 4]) -> String {
    let (x0, y0, x1, y1) = (r[0], r[1], r[2], r[3]);
    let mut hits: Vec<&Word> = words
        .iter()
        .filter(|w| w.x0 < x1 && x0 < w.x1 && w.y0 < y1 && y0 < w.y1)
        .collect();
    hits.sort_by(|a, b| b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0)));
    hits.iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ")
}

/// Palabras del rectángulo agrupadas por línea: cada línea sale como un
/// rectángulo **continuo** (como al seleccionar texto en un PDF, sin dejar
/// huecos entre palabras). Puro y testeable.
fn words_in_lines(words: &[Word], r: [f32; 4]) -> Vec<[f32; 4]> {
    let (x0, y0, x1, y1) = (r[0], r[1], r[2], r[3]);
    let mut hits: Vec<&Word> = words
        .iter()
        .filter(|w| w.x0 < x1 && x0 < w.x1 && w.y0 < y1 && y0 < w.y1)
        .collect();
    if hits.is_empty() {
        return vec![];
    }
    // de arriba abajo y de izquierda a derecha
    hits.sort_by(|a, b| b.y1.total_cmp(&a.y1).then(a.x0.total_cmp(&b.x0)));
    let mut lines: Vec<[f32; 4]> = vec![];
    let mut cur: Option<[f32; 4]> = None;
    for w in hits {
        let misma = match cur {
            Some(c) => {
                let solape = (c[3].min(w.y1) - c[1].max(w.y0)).max(0.0);
                let alto = (w.y1 - w.y0).max(0.1);
                solape >= alto * 0.5
            }
            None => false,
        };
        if misma {
            let mut c = cur.unwrap();
            c[0] = c[0].min(w.x0);
            c[1] = c[1].min(w.y0);
            c[2] = c[2].max(w.x1);
            c[3] = c[3].max(w.y1);
            cur = Some(c);
        } else {
            if let Some(c) = cur {
                lines.push(c);
            }
            cur = Some([w.x0, w.y0, w.x1, w.y1]);
        }
    }
    if let Some(c) = cur {
        lines.push(c);
    }
    lines
}

/// Texto de la anotación a partir del hallazgo (o el que escribió el usuario).
fn note_text(e: &Misspelling, text: &str) -> String {
    if text.trim().is_empty() {
        format!("Revisar: {} (sugerencias: {})", e.word, e.sug.join(", "))
    } else {
        text.to_string()
    }
}

/// Abre el PDF (con la contraseña si la hay) y lo deja abierto en el estado.
/// Devuelve el error de pdfium tal cual (para detectar PasswordError).
fn open_doc_file(st: &mut State, path: &std::path::Path) -> Result<(), PdfiumError> {
    let doc = st.pdfium.load_pdf_from_file(path, st.password)?;
    st.doc = Some(doc);
    st.doc_from = Some(path.to_path_buf());
    st.doc_stamp = file_stamp(path);
    Ok(())
}

/// ¿El pdfium pide contraseña?
fn is_password_error(e: &PdfiumError) -> bool {
    matches!(
        e,
        PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError)
    )
}

/// Convierte un rectángulo de la imagen (px, origen arriba-izq) a puntos PDF
/// (origen abajo-izq). Puro y testeable.
fn px_rect_to_pdf(
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    page_w: f32,
    page_h: f32,
    img_w: f32,
    img_h: f32,
) -> (f32, f32, f32, f32) {
    let to_x = |v: f32| (v / img_w * page_w).clamp(0.0, page_w);
    let to_y = |v: f32| (page_h - v / img_h * page_h).clamp(0.0, page_h);
    (to_x(x0.min(x1)), to_y(y0.max(y1)), to_x(x0.max(x1)), to_y(y0.min(y1)))
}

/// Copia el documento a su fichero de trabajo "{nombre}_anotado.pdf"
/// (0600, atómico). Nunca toca el original.
fn copy_to_working(src: &std::path::Path) -> anyhow::Result<PathBuf> {
    let out = working_path(src);
    let tmp = out.with_extension("tmp.pdf");
    let r: anyhow::Result<()> = (|| {
        std::fs::copy(src, &tmp)?;
        #[cfg(unix)]
        std::fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
        std::fs::rename(&tmp, &out)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r?;
    Ok(out)
}

/// Crea la copia de trabajo del documento actual.
fn create_working_copy(st: &mut State) -> anyhow::Result<PathBuf> {
    let src = st.path.clone().context("sin documento")?;
    copy_to_working(&src)
}

/// Aplica la anotación al documento abierto y la guarda en la copia de
/// trabajo. Devuelve las notas creadas (para el tooltip).
/// - reabre el doc si la copia cambió por fuera (no pisar ediciones ajenas)
/// - guarda vía temporal + rename (Pdfium tiene el fichero mapeado)
/// - los resaltados se parchean después con lopdf (pdfium no pone QuadPoints)
fn annotate_open(st: &mut State, req: &AnnotateReq) -> anyhow::Result<Vec<DocNote>> {
    let out = st.working.clone().context("sin copia de trabajo")?;
    let stale = st.doc_from.as_deref() != Some(out.as_path()) || st.doc_stamp != file_stamp(&out);
    if stale {
        open_doc_file(st, &out).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let mut added: Vec<DocNote> = vec![];
    let mut hl_rects: Vec<(u32, [f32; 4])> = vec![];
    {
        let color_mode = st.settings.hl_color;
        let doc = st.doc.as_mut().context("sin documento abierto")?;
        let pages = doc.pages_mut();
        match req {
            // resaltado + comentario sobre la palabra de uno o varios errores
            AnnotateReq::Highlight(idxs, text) => {
                anyhow::ensure!(!idxs.is_empty(), "grupo vacío");
                for idx in idxs {
                    let e = st.all_errs.get(*idx).cloned().context("hallazgo no válido")?;
                    let t = note_text(&e, text);
                    let mut pg = pages.get(e.page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    let mut ann = pg.annotations_mut().create_highlight_annotation().map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_bounds(PdfRect::new_from_values(e.y0, e.x0, e.y1, e.x1)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_stroke_color(hl_color(color_mode, &e.kind)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_contents(&t).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    hl_rects.push((e.page, [e.x0, e.y0, e.x1, e.y1]));
                    added.push(DocNote { page: e.page, x0: e.x0, y0: e.y0, x1: e.x1, y1: e.y1, text: t, kind: AnnoKind::Highlight });
                }
            }
            // nota adhesiva libre (sticky note)
            AnnotateReq::Note(page, x, y, text) => {
                anyhow::ensure!(!text.trim().is_empty(), "escribe la nota primero");
                let mut pg = pages.get(*page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                let mut ann = pg.annotations_mut().create_text_annotation(text).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                ann.set_bounds(PdfRect::new_from_values((*y - 6.0).max(0.0), (*x - 6.0).max(0.0), *y + 6.0, *x + 6.0))
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                added.push(DocNote { page: *page, x0: *x, y0: *y, x1: *x, y1: *y, text: text.clone(), kind: AnnoKind::Note });
            }
            // resaltado libre sobre uno o varios rectángulos (área o palabras)
            AnnotateReq::HighlightRects(page, rects, text) => {
                anyhow::ensure!(!text.trim().is_empty(), "escribe el comentario primero");
                anyhow::ensure!(!rects.is_empty(), "no hay nada que resaltar");
                let mut pg = pages.get(*page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                for r in rects {
                    let mut ann = pg.annotations_mut().create_highlight_annotation().map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_bounds(PdfRect::new_from_values(r[1], r[0], r[3], r[2])).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_stroke_color(hl_color(color_mode, "")).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    ann.set_contents(text).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                    hl_rects.push((*page, *r));
                    added.push(DocNote { page: *page, x0: r[0], y0: r[1], x1: r[2], y1: r[3], text: text.clone(), kind: AnnoKind::Highlight });
                }
            }
        }
    }
    let doc = st.doc.as_ref().context("sin documento abierto")?;
    save_doc(doc, &out)?;
    // los resaltados necesitan /QuadPoints: los pone lopdf sobre el fichero
    if !hl_rects.is_empty() {
        if let Err(e) = patch_highlights(&out) {
            anyhow::bail!("no se pudo pintar el resaltado: {e:#}");
        }
        // el fichero cambió: se reabre para que el visor muestre el resaltado
        open_doc_file(st, &out).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    Ok(added)
}

/// Color del resaltado: por gravedad (rojo/morado) o amarillo único.
/// El texto libre no tiene gravedad -> siempre amarillo (el clásico rotulador).
fn hl_color(mode: u8, kind: &str) -> PdfColor {
    if mode == 1 {
        return PdfColor::new(255, 214, 0, 255);
    }
    match kind {
        "Sintaxis" => PdfColor::new(106, 27, 154, 255),
        "Ortografía" => PdfColor::new(211, 47, 47, 255),
        _ => PdfColor::new(255, 214, 0, 255),
    }
}

/// Borra una anotación del PDF (y la quita de la lista en memoria).
/// Borra **todas** las anotaciones que correspondan a esa nota (si el usuario
/// comenta la misma palabra dos veces, se borran las dos y no queda ningún
/// resaltado suelto).
/// Borra las anotaciones de una página que están en ese rectángulo (las que
/// sean: si el usuario comenta la misma palabra dos veces, se van las dos y no
/// queda ningún resaltado suelto).
fn delete_annos_at(st: &mut State, page: u32, rect: [f32; 4], kind: AnnoKind) -> anyhow::Result<usize> {
    let out = st.working.clone().context("sin copia de trabajo")?;
    let stale = st.doc_from.as_deref() != Some(out.as_path()) || st.doc_stamp != file_stamp(&out);
    if stale {
        open_doc_file(st, &out).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    let doc = st.doc.as_mut().context("sin documento abierto")?;
    let mut pages = doc.pages_mut();
    let mut pg = pages.get(page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // se buscan las anotaciones que coinciden (tipo + rect)
    let mut targets: Vec<usize> = vec![];
    {
        let anns = pg.annotations();
        for (i, a) in anns.iter().enumerate() {
            let kind_ok = match (a.annotation_type(), kind) {
                (PdfPageAnnotationType::Text, AnnoKind::Note) => true,
                (PdfPageAnnotationType::Highlight, AnnoKind::Highlight) => true,
                _ => false,
            };
            if !kind_ok {
                continue;
            }
            let rect_ok = a.bounds().map(|b| {
                let (l, bo, r, t) = (b.left().value, b.bottom().value, b.right().value, b.top().value);
                if kind == AnnoKind::Note {
                    // las notas pegajosas se guardan como punto, pero en el PDF
                    // son una cajita de 12x12: se busca la que lo contiene
                    l <= rect[0] + 0.5 && r >= rect[0] - 0.5 && bo <= rect[1] + 0.5 && t >= rect[1] - 0.5
                } else {
                    (l - rect[0]).abs() < 0.5
                        && (bo - rect[1]).abs() < 0.5
                        && (r - rect[2]).abs() < 0.5
                        && (t - rect[3]).abs() < 0.5
                }
            }).unwrap_or(false);
            if rect_ok {
                targets.push(i);
            }
        }
    }
    if targets.is_empty() {
        anyhow::bail!("no se encontró la anotación en el PDF");
    }
    {
        let mut anns = pg.annotations_mut();
        // de mayor a menor: al borrar, los índices se mueven
        for i in targets.iter().rev() {
            let a = anns.get(*i)?;
            anns.delete_annotation(a)?;
        }
    }
    drop(pg);
    drop(pages);
    let doc = st.doc.as_ref().context("sin documento abierto")?;
    save_doc(doc, &out)?;
    // pdfium puede perder los /QuadPoints al guardar: se reparan
    patch_highlights(&out)?;
    // y se quitan de la lista en memoria (las de esa zona)
    st.doc_notes.retain(|n| {
        if n.page != page || n.kind != kind {
            return true;
        }
        match kind {
            AnnoKind::Note => {
                !(n.x0 <= rect[0] + 0.5 && n.x1 >= rect[0] - 0.5 && n.y0 <= rect[1] + 0.5 && n.y1 >= rect[1] - 0.5)
            }
            AnnoKind::Highlight => {
                !((n.x0 - rect[0]).abs() < 0.5 && (n.y0 - rect[1]).abs() < 0.5
                    && (n.x1 - rect[2]).abs() < 0.5 && (n.y1 - rect[3]).abs() < 0.5)
            }
        }
    });
    Ok(targets.len())
}

/// Borra una anotación del PDF (y la quita de la lista en memoria).
fn delete_anno(st: &mut State, idx: usize) -> anyhow::Result<()> {
    let n = st.doc_notes.get(idx).cloned().context("anotación no válida")?;
    delete_annos_at(st, n.page, [n.x0, n.y0, n.x1, n.y1], n.kind)?;
    Ok(())
}

fn show(ui: &AppWindow, st: &mut State, page: u32) {
    let Some(path) = st.path.clone() else { return };
    // el doc se mantiene abierto: si no lo está (o cambió por fuera), se abre
    let need_open = st.doc.is_none() || st.doc_from.as_deref() != st.render_from().as_deref();
    if need_open {
        let src = st.render_from().unwrap_or_else(|| path.clone());
        if let Err(e) = open_doc_file(st, &src) {
            ui.set_popup_text(format!("Error: {e:?}").into());
            st.popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
            return;
        }
    }
    let s: &State = st;
    let res: anyhow::Result<(Image, u32, Vec<Word>, f32, f32, u32, u32)> = (|| {
        let doc = s.doc.as_ref().context("sin documento abierto")?;
        let pages = doc.pages();
        let total = pages.len() as u32;
        let pg = pages.get(page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (pw, ph) = (pg.width().value, pg.height().value);
        let config = PdfRenderConfig::new().set_target_width((1400.0 * s.zoom) as i32);
        let mut rgba = pg.render_with_config(&config).map_err(|e| anyhow::anyhow!("{e:?}"))?
            .as_image().as_rgba8().context("pdfium no devolvió RGBA")?.clone();
        let (iw, ih) = (rgba.width(), rgba.height());
        for h in s.all_errs.iter().filter(|e| e.page == page && !e.dismissed) {
            let (x, y, w, hh) = pdf_to_image(h.x0, h.y0, h.x1, h.y1, pw, ph, iw, ih);
            underline(&mut rgba, x, y + hh - 3.0, w, under_color(&h.kind));
        }
        // las anotaciones (resaltados y sticky) las dibuja pdfium: nuestro
        // marcador cuadrado tapaba el resaltado real (parecía un punto)
        // flash amarillo 5s del error clicado
        if let Some((fi, t)) = &s.flash {
            if t.elapsed() < std::time::Duration::from_secs(5) {
                if let Some(h) = s.all_errs.get(*fi) {
                    if h.page == page && !h.dismissed {
                        let (x, y, w, hh) = pdf_to_image(h.x0, h.y0, h.x1, h.y1, pw, ph, iw, ih);
                        wash(&mut rgba, x, y, w, hh);
                    }
                }
            }
        }
        let img = Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), iw, ih));
        let words = extract_words(&pg);
        Ok((img, total, words, pw, ph, iw, ih))
    })();
    match res {
        Ok((img, total, words, pw, ph, iw, ih)) => {
            st.page = page;
            st.total = total;
            st.page_w = pw;
            st.page_h = ph;
            st.img_w = iw;
            st.img_h = ih;
            ui.set_page_image(img);
            ui.set_has_doc(true);
            ui.set_page_label(format!("Pág {} de {}", page + 1, total).into());
            ui.set_word_count(words.len() as i32);
            let sample: Vec<&str> = words.iter().take(12).map(|w| w.text.as_str()).collect();
            ui.set_word_sample(sample.join(" ").into());
            ui.set_status(format!("{} — {} págs. · {} palabras en pág. {}", path.display(), total, words.len(), page + 1).into());
            ui.set_img_w(iw as i32);
            ui.set_img_h(ih as i32);
            st.words = words;
            push_page_notes(ui, st);
        }
        Err(e) => {
            ui.set_popup_text(format!("Error: {e:#}").into());
            st.popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        }
    }
}

/// Notas de la página visible (el Slint pone el tooltip al pasar el ratón).
fn push_page_notes(ui: &AppWindow, st: &State) {
    let (pw, ph, iw, ih, page) = (st.page_w, st.page_h, st.img_w, st.img_h, st.page);
    if iw == 0 || ih == 0 {
        return;
    }
    let pn: Vec<NoteRow> = st
        .doc_notes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.page == page)
        .map(|(i, n)| {
            let (x, y, w, h) = pdf_to_image(n.x0, n.y0, n.x1, n.y1, pw, ph, iw, ih);
            // la nota pegajosa es un punto: cajita de 24x24 encima. El
            // resaltado usa su rectángulo entero, para que el tooltip salga
            // al pasar por encima del texto resaltado.
            let (x, y, w, h) = if w < 4.0 || h < 4.0 { (x - 12.0, y - 12.0, 24.0, 24.0) } else { (x, y, w.max(24.0), h.max(12.0)) };
            // el chip hace wrap, asi que el texto va entero
            NoteRow { text: n.text.clone().into(), page: n.page as i32, x, y, w, h, idx: i as i32 }
        })
        .collect();
    ui.set_page_notes(ModelRc::new(VecModel::from_slice(&pn)));
    ui.set_note_total(st.doc_notes.len() as i32);
}

impl State {
    /// De dónde se lee y se renderiza: la copia de trabajo si existe.
    fn render_from(&self) -> Option<PathBuf> {
        self.working.clone().or_else(|| self.path.clone())
    }
}

fn load_pdfium() -> &'static Pdfium {
    PDFIUM.with(|p| *p)
}

fn bind_pdfium() -> Pdfium {
    let lib = if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else {
        "libpdfium.so"
    };
    let mut cands = vec![];
    if let Ok(p) = std::env::var("PDFIUM_DYNAMIC_LIB_PATH") {
        cands.push(PathBuf::from(p).join(lib));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            cands.push(d.join(lib));
        }
    }
    cands.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("assets/pdfium")
            .join(lib),
    );
    for c in cands {
        if c.is_file() {
            if let Ok(b) = Pdfium::bind_to_library(&c) {
                return Pdfium::new(b);
            }
        }
    }
    Pdfium::default()
}

fn main() -> anyhow::Result<()> {
    let ui = AppWindow::new()?;
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_toast_pos(load_settings().toast_pos as i32);
    if std::env::var("PDF_DARK").is_ok() {
        ui.set_is_dark(true);
    }
    let (tx, rx) = channel::<SpellMsg>();
    let st = Rc::new(RefCell::new(State {
        pdfium: load_pdfium(),
        doc: None,
        doc_from: None,
        doc_stamp: None,
        path: None,
        working: None,
        settings: load_settings(),
        err_anno: HashMap::new(),
        free_rects: None,
        password: None,
        dlg_pending: None,
        page: 0,
        total: 0,
        words: vec![],
        page_w: 0.0,
        page_h: 0.0,
        img_w: 0,
        img_h: 0,
        all_errs: vec![],
        doc_notes: vec![],
        view: vec![],
        view_group: vec![],
        expanded: HashSet::new(),
        flash: None,
        scan_id: 0,
        scan_queue: vec![],
        scan_pos: 0,
        popup_until: None,
        zoom: 0.5,
        free_pt: None,
        lt_ready: false,
        pending: 0, jobs_total: 0, sec_bytes: 0, boot_start: std::time::Instant::now(),
        syn_received: false,
        lt_dead: false,
        lt_child: None,
        tx,
        last_lang: 0,
    }));

    // recoge respuestas de los hilos + detecta cambio de idioma
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, std::time::Duration::from_millis(250), {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let lang_changed = st.borrow().path.is_some() && ui.get_lang_idx() != st.borrow().last_lang;
            if lang_changed {
                start_scan(&ui, &mut st.borrow_mut());
            }
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    SpellEvent::LtReady(child) => {
                        st.borrow_mut().lt_ready = true;
                        st.borrow_mut().lt_child = child;
                        ui.set_lt_status("Motor: listo (local, sin red).".into());
                    }
                    SpellEvent::LtFail(msg) => {
                        st.borrow_mut().lt_dead = true;
                        ui.set_lt_status(format!("Motor: no disponible ({msg}).").into());
                        notify(&ui, &st, format!("Sin revisión: {msg}."));
                    }
                    SpellEvent::Syn(gen, hits) => {
                        if gen != st.borrow().scan_id {
                            continue;
                        }
                        st.borrow_mut().syn_received = true;
                        let touched_cur = {
                            let mut s = st.borrow_mut();
                            s.pending = s.pending.saturating_sub(1);
                            let cur = s.page;
                            let t = hits.iter().any(|h| h.page == cur);
                            s.all_errs.extend(hits);
                            t
                        };
                        push_errs(&ui, &st);
                        if touched_cur {
                            let pg = st.borrow().page;
                            show(&ui, &mut st.borrow_mut(), pg);
                        }
                        // cierra la barra solo cuando no queda nada en vuelo
                        let s = st.borrow();
                        if s.scan_pos >= s.scan_queue.len() && s.pending == 0 {
                            ui.set_scanning(false);
                            ui.set_scan_text("".into());
                        }
                    }
                }
            }
            if scan_tick(&ui, &st) {
                let pg = st.borrow().page;
                show(&ui, &mut st.borrow_mut(), pg);
            }
            // apaga el flash amarillo a los 5s
            let flash_out = st.borrow().flash.is_some_and(|(_, t)| t.elapsed() > std::time::Duration::from_secs(5));
            if flash_out {
                st.borrow_mut().flash = None;
                let pg = st.borrow().page;
                show(&ui, &mut st.borrow_mut(), pg);
            }
            // auto-cierra el toast
            let popup_out = st.borrow().popup_until.is_some_and(|t| std::time::Instant::now() >= t);
            if popup_out {
                st.borrow_mut().popup_until = None;
                ui.set_popup_text("".into());
            }
        }
    });

    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_open_pdf(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let Some(file) = rfd::FileDialog::new()
                .add_filter("PDF", &["pdf"])
                .pick_file()
            else {
                return;
            };
            ui.set_status("Cargando…".into());
            open_doc(&ui, &st, file);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_prev_page(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let cur = st.borrow().page;
            if cur > 0 {
                show(&ui, &mut st.borrow_mut(), cur - 1);
                ui.set_view_x(0.0);
                ui.set_view_y(0.0);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_next_page(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (cur, total) = (st.borrow().page, st.borrow().total);
            if cur + 1 < total {
                show(&ui, &mut st.borrow_mut(), cur + 1);
                ui.set_view_x(0.0);
                ui.set_view_y(0.0);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_zoom_in(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let z = (st.borrow().zoom * 1.25).min(3.0);
            st.borrow_mut().zoom = z;
            let pg = st.borrow().page;
            show(&ui, &mut st.borrow_mut(), pg);
            ui.set_view_x(0.0);
            ui.set_view_y(0.0);
            ui.set_zoom_label(format!("{}%", (z * 100.0).round() as u32).into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_zoom_out(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let z = (st.borrow().zoom / 1.25).max(0.5);
            st.borrow_mut().zoom = z;
            let pg = st.borrow().page;
            show(&ui, &mut st.borrow_mut(), pg);
            ui.set_view_x(0.0);
            ui.set_view_y(0.0);
            ui.set_zoom_label(format!("{}%", (z * 100.0).round() as u32).into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_zoom_text(move |t| {
            let Some(ui) = ui_weak.upgrade() else { return };
            // acepta "150", "150%" o "1.5x": número inicial = porcentaje
            let num: String = t.chars().take_while(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
            if let Ok(v) = num.replace(',', ".").parse::<f32>() {
                if v.is_finite() {
                    st.borrow_mut().zoom = (v / 100.0).clamp(0.5, 3.0);
                }
            }
            let pg = st.borrow().page;
            show(&ui, &mut st.borrow_mut(), pg);
            let z = st.borrow().zoom;
            ui.set_zoom_label(format!("{}%", (z * 100.0).round() as u32).into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_free_note_new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_tool_mode() == 1 {
                ui.set_tool_mode(0);
                ui.set_free_note_on(false);
                return;
            }
            let mut s = st.borrow_mut();
            s.free_pt = None; // armado: el toque en el doc fija el punto
            s.free_rects = None;
            ui.set_free_sel_text("".into());
            drop(s);
            ui.set_tool_mode(1);
            ui.set_free_note_text("".into());
            ui.set_free_note_on(false);
            notify(&ui, &st, "Toca el punto del documento para la nota".into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_page_clicked(move |mx, my| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_tool_mode() != 1 {
                return;
            }
            let s = st.borrow();
            if s.img_w == 0 || s.img_h == 0 {
                return;
            }
            // px de imagen 1:1 → puntos PDF (origen abajo-izq)
            let x = (mx / s.img_w as f32 * s.page_w).clamp(0.0, s.page_w);
            let y = (s.page_h - my / s.img_h as f32 * s.page_h).clamp(0.0, s.page_h);
            let pg = s.page;
            drop(s);
            st.borrow_mut().free_pt = Some((pg, x, y));
            ui.set_free_note_on(true);
            notify(&ui, &st, "Punto marcado: escribe y pulsa Guardar".into());
        });
    }
    {
        // resaltado libre: rectángulo arrastrado sobre la página
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_page_rect(move |x0, y0, x1, y1| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_tool_mode() != 2 {
                return;
            }
            let s = st.borrow();
            if s.img_w == 0 || s.img_h == 0 {
                return;
            }
            let (pw, ph, iw, ih) = (s.page_w, s.page_h, s.img_w as f32, s.img_h as f32);
            let pg = s.page;
            let por_palabras = s.settings.free_sel == 1;
            let por_recuadro = s.settings.free_text == 0; // 0 = caja por palabra
            let words = s.words.clone();
            drop(s);
            // px de imagen → puntos PDF (y invertido)
            let (ax, ay, bx, by) = px_rect_to_pdf(x0, y0, x1, y1, pw, ph, iw, ih);
            if (bx - ax).abs() < 1.0 || (by - ay).abs() < 1.0 {
                notify(&ui, &st, "Arrastra un rectángulo (no un clic)".into());
                return;
            }
            // por texto: se resaltan las palabras que toque el rectángulo,
            // agrupadas por línea (selección continua, sin huecos).
            // Si no pilla ninguna, el área tal cual.
            let (rects, msg): (Vec<[f32; 4]>, String) = if por_palabras {
                let hits = words_in_rect(&words, [ax, ay, bx, by]);
                if hits.is_empty() {
                    (vec![[ax, ay, bx, by]], "Ahí no hay texto: se resalta el área".to_string())
                } else if por_recuadro {
                    let n = hits.len();
                    (
                        hits,
                        format!(
                            "{n} palabra{} en su propia caja: escribe el comentario y guarda",
                            if n == 1 { "" } else { "s" }
                        ),
                    )
                } else {
                    let n = hits.len();
                    let lineas = words_in_lines(&words, [ax, ay, bx, by]);
                    let nl = lineas.len();
                    (
                        lineas,
                        format!(
                            "{n} palabra{} en {nl} línea{}: escribe el comentario y guarda",
                            if n == 1 { "" } else { "s" },
                            if nl == 1 { "" } else { "s" }
                        ),
                    )
                }
            } else {
                (vec![[ax, ay, bx, by]], "Zona marcada: escribe el comentario y guarda".to_string())
            };
            st.borrow_mut().free_rects = Some((pg, rects.clone()));
            // el texto de verdad que se ha seleccionado (vacío si el área
            // no pilla palabras). Se corta: el panel solo enseña una línea.
            let sel = text_in_rect(&words, [ax, ay, bx, by]);
            let sel = if sel.chars().count() > 80 {
                let mut t: String = sel.chars().take(79).collect();
                t.push('…');
                t
            } else {
                sel
            };
            ui.set_free_sel_text(sel.into());
            // el comentario anterior se borra: cada texto nuevo, comentario nuevo
            ui.set_free_note_text("".into());
            ui.set_free_note_on(true);
            notify(&ui, &st, msg.into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_free_note_save(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let text = ui.get_free_note_text().to_string();
            let mode = ui.get_tool_mode();
            let pt = st.borrow().free_pt;
            let rects = st.borrow().free_rects.clone();
            let res = if mode == 2 {
                match rects {
                    Some((pg, rs)) => apply_note(&ui, &st, AnnotateReq::HighlightRects(pg, rs, text)),
                    None => {
                        notify(&ui, &st, "Arrastra un rectángulo sobre el documento primero".into());
                        NoteResult::Failed
                    }
                }
            } else {
                match pt {
                    Some((pg, x, y)) => apply_note(&ui, &st, AnnotateReq::Note(pg, x, y, text)),
                    None => {
                        notify(&ui, &st, "Toca primero el punto del PDF".into());
                        NoteResult::Failed
                    }
                }
            };
            // el panel solo se cierra si la nota quedó guardada: si no, el
            // texto se pierde y el usuario tiene que reescribirla
            if res == NoteResult::Saved {
                ui.set_free_note_on(false);
                ui.set_tool_mode(0);
                ui.set_free_sel_text("".into());
                let mut s = st.borrow_mut();
                s.free_pt = None;
                s.free_rects = None;
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_free_note_cancel(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_free_note_on(false);
                ui.set_tool_mode(0);
                ui.set_free_sel_text("".into());
            }
            let mut s = st.borrow_mut();
            s.free_pt = None;
            s.free_rects = None;
        });
    }
    {
        // modo resaltar (texto libre): se arrastra un rectángulo sobre la página
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_highlight_new(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_tool_mode() == 2 {
                ui.set_tool_mode(0);
                ui.set_free_note_on(false);
                ui.set_free_sel_text("".into());
                return;
            }
            ui.set_tool_mode(2);
            ui.set_free_note_on(false);
            ui.set_free_sel_text("".into());
            {
                let mut s = st.borrow_mut();
                s.free_pt = None;
                s.free_rects = None;
            }
            notify(&ui, &st, "Arrastra sobre el texto que quieras resaltar".into());
        });
    }
    {
        // modo borrar: pinchar en un marcador lo borra
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_delete_mode(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            if ui.get_tool_mode() == 3 {
                ui.set_tool_mode(0);
                return;
            }
            ui.set_tool_mode(3);
            ui.set_free_note_on(false);
            notify(&ui, &st, "Pincha un marcador para borrarlo · vuelve a pulsar el icono para salir".into());
        });
    }
    {
        // borrar una anotación (desde el marcador o desde la card)
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_delete_note(move |idx| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let res = with_state(&st, |s| delete_anno(s, idx as usize));
            match res {
                Ok(()) => {
                    let pg = st.borrow().page;
                    show(&ui, &mut st.borrow_mut(), pg);
                    push_page_notes(&ui, &st.borrow());
                    st.borrow_mut().err_anno.clear();
                    push_errs(&ui, &st);
                    notify(&ui, &st, "Anotación borrada".into());
                }
                Err(e) => notify(&ui, &st, format!("No se pudo borrar: {e:#}")),
            }
        });
    }
    {
        // borrar el resaltado de un error concreto (desde su card)
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_delete_anno(move |row| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let gi = match st.borrow().view.get(row as usize) {
                Some(g) => *g,
                None => return,
            };
            // si es un grupo, se borran los resaltados de todos sus miembros
            let group = st.borrow().view_group.get(row as usize).cloned().flatten();
            // un solo préstamo: si se presta dos veces nested, el RefCell peta
            let targets: Vec<(u32, [f32; 4])> = {
                let s = st.borrow();
                match &group {
                    Some(w) => s
                        .all_errs
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| &e.word == w)
                        .filter_map(|(k, _)| s.err_anno.get(&k).copied())
                        .collect(),
                    None => s.err_anno.get(&gi).copied().into_iter().collect(),
                }
            };
            if targets.is_empty() {
                return;
            }
            let mut errs = 0;
            for (page, rect) in targets {
                let r = with_state(&st, |s| delete_annos_at(s, page, rect, AnnoKind::Highlight));
                if let Err(e) = r {
                    errs += 1;
                    notify(&ui, &st, format!("No se pudo borrar: {e:#}"));
                }
            }
            st.borrow_mut().err_anno.clear();
            let pg = st.borrow().page;
            show(&ui, &mut st.borrow_mut(), pg);
            push_page_notes(&ui, &st.borrow());
            push_errs(&ui, &st);
            if errs == 0 {
                notify(&ui, &st, "Anotación borrada".into());
            }
        });
    }
    {
        // Ajustes/Acerca de/Licencias ya no son ventanas aparte: son capas de la
        // ventana principal (AppWindow.ovl) sobre un velo (app.slint). Aquí solo
        // se alimentan con los datos de la sesión.
        let ui_weak = ui.as_weak();
        let ui_open = ui_weak.clone();
        let st = st.clone();
        let st_open = st.clone();
        ui.on_open_settings(move || {
            let Some(ui) = ui_open.upgrade() else { return };
            if ui.get_ovl() == 0 {
                // si se abre desde cero, se recargan los valores guardados
                // (si venía de Acerca de atrás, se conservan los no aplicados)
                let s = st_open.borrow().settings;
                ui.set_p_hl_color(s.hl_color as i32);
                ui.set_p_copy_mode(s.copy_mode as i32);
                ui.set_p_motor_on(s.motor);
                ui.set_p_free_sel(s.free_sel as i32);
                ui.set_p_free_text(s.free_text as i32);
                ui.set_p_toast_pos(s.toast_pos as i32);
            }
            ui.set_ovl(1);
        });
        let lic_weak = ui_weak.clone();
        ui.on_open_licenses(move || {
            let Some(ui) = lic_weak.upgrade() else { return };
            let entries = license_entries();
            let rows: Vec<LicRow> = entries
                .iter()
                .map(|e| LicRow { name: e.name.into(), lic: e.lic.into() })
                .collect();
            ui.set_p_lic_items(ModelRc::new(VecModel::from_slice(&rows)));
            lic_select(&ui, 0, &entries);
            ui.set_ovl(3);
        });
        let apply_weak = ui_weak.clone();
        let apply_st = st.clone();
        ui.on_p_apply(move |hl, copy, motor, fsel, ftext, tpos| {
            let Some(ui) = apply_weak.upgrade() else { return };
            let s2 = Settings {
                hl_color: hl as u8,
                copy_mode: copy as u8,
                motor,
                free_sel: fsel as u8,
                free_text: ftext as u8,
                toast_pos: tpos as u8,
            };
            save_settings(&s2);
            {
                let mut stt = apply_st.borrow_mut();
                stt.settings = s2;
                stt.lt_dead = !s2.motor;
            }
            ui.set_set_hl_color(hl);
            ui.set_set_copy_mode(copy);
            ui.set_set_motor(motor);
            ui.set_toast_pos(tpos);
            notify(&ui, &apply_st, "Ajustes guardados".into());
        });
        let sel_weak = ui_weak.clone();
        ui.on_p_lic_select(move |i| {
            let Some(ui) = sel_weak.upgrade() else { return };
            let entries = license_entries();
            lic_select(&ui, i, &entries);
        });
        ui.on_p_lic_link(move |url| open_link(&url));
    }

    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_error_clicked(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let gi = match st.borrow().view.get(i as usize) {
                Some(g) => *g,
                None => return,
            };
            let (pg, detail) = match st.borrow().all_errs.get(gi) {
                Some(e) => {
                    let sugs = if e.sug.is_empty() { "".into() } else { format!(" → {}", e.sug.join(", ")) };
                    (e.page, format!("{}{} · {} {} · pág {}", e.word, sugs, e.kind, e.sev, e.page + 1))
                }
                None => return,
            };
            st.borrow_mut().flash = Some((gi, std::time::Instant::now()));
            let cur = st.borrow().page;
            if pg != cur {
                show(&ui, &mut st.borrow_mut(), pg); // salto + flash amarillo
            } else {
                show(&ui, &mut st.borrow_mut(), cur);
            }
            // lleva el visor al error: content-x/y es offset del contenido
            // (negativo). En vertical siempre; en horizontal solo si está
            // fuera de vista, para conservar el margen izquierdo.
            {
                let s = st.borrow();
                if let Some(e) = s.all_errs.get(gi) {
                    // sin tamaño de visor medido no se centra (evita saltos fuera de vista)
                    if s.img_w > 0 && s.img_h > 0 {
                        let (x, y, w, h) = pdf_to_image(e.x0, e.y0, e.x1, e.y1, s.page_w, s.page_h, s.img_w, s.img_h);
                        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
                        let (vw, vh) = (ui.get_view_w(), ui.get_view_h());
                        if vw > 0.0 && vh > 0.0 {
                            let oy = -((cy - vh / 2.0).clamp(0.0, (s.img_h as f32 - vh).max(0.0)));
                            ui.set_view_y(oy);
                            let ox_cur = ui.get_view_x();
                            let vis0 = -ox_cur;
                            if cx < vis0 || cx > vis0 + vw {
                                let ox = -((cx - vw / 2.0).clamp(0.0, (s.img_w as f32 - vw).max(0.0)));
                                ui.set_view_x(ox);
                            }
                        }
                    }
                }
            }
            notify(&ui, &st, detail); // LT ya trae reemplazos; sin hilos aquí
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_theme_toggle(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_is_dark(!ui.get_is_dark());
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_popup_dismiss(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_popup_text("".into());
            }
            st.borrow_mut().popup_until = None;
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_omit_clicked(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let pg = {
                let mut s = st.borrow_mut();
                let gi = match s.view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                let group = s.view_group.get(i as usize).cloned().flatten();
                // índices objetivo: todo el grupo o la ocurrencia sola
                let (targets, dismissing) = if let Some(w) = group {
                    let all_off = s.all_errs.iter().filter(|e| e.word == w).all(|e| e.dismissed);
                    let idxs: Vec<usize> = s.all_errs.iter().enumerate()
                        .filter(|(_, e)| e.word == w).map(|(j, _)| j).collect();
                    (idxs, !all_off)
                } else {
                    let to = !s.all_errs.get(gi).map(|e| e.dismissed).unwrap_or(true);
                    (vec![gi], to)
                };
                for j in &targets {
                    if let Some(e) = s.all_errs.get_mut(*j) {
                        e.dismissed = dismissing;
                    }
                }
                // al omitir, apaga también lo solapado (otra clase sobre la misma zona);
                // al restaurar, solo el objetivo para no resucitar omisiones ajenas
                if dismissing {
                    let refs: Vec<Misspelling> = targets.iter().filter_map(|j| s.all_errs.get(*j).cloned()).collect();
                    for e in s.all_errs.iter_mut() {
                        if !e.dismissed && refs.iter().any(|r| boxes_overlap(r, e)) {
                            e.dismissed = true;
                        }
                    }
                }
                // si el flash quedó sobre algo omitido, se limpia (era wash amarillo fantasma)
                if let Some((fi, _)) = s.flash {
                    if s.all_errs.get(fi).map(|e| e.dismissed).unwrap_or(false) {
                        s.flash = None;
                    }
                }
                s.page
            };
            push_errs(&ui, &st);
            show(&ui, &mut st.borrow_mut(), pg); // refresca subrayados
            touch_history(&st.borrow());
            push_history(&ui);
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_note_clicked(move |i| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_note(if ui.get_selected_note() == i { -1 } else { i });
                ui.set_note_text("".into());
            }
        });
    }
    {
        // "Cancelar" en el editor del comentario: se cierra sin guardar
        let ui_weak = ui.as_weak();
        ui.on_note_cancel(move |_i| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_selected_note(-1);
                ui.set_note_text("".into());
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_save_note(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let text = ui.get_note_text().to_string();
            let group = st.borrow().view_group.get(i as usize).cloned().flatten();
            let req = if let Some(w) = group {
                let idxs: Vec<usize> = st.borrow().all_errs.iter().enumerate()
                    .filter(|(_, e)| e.word == w).map(|(gi, _)| gi).collect();
                AnnotateReq::Highlight(idxs, text)
            } else {
                let gi = match st.borrow().view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                AnnotateReq::Highlight(vec![gi], text)
            };
            let res = apply_note(&ui, &st, req);
            // el editor se cierra solo si la nota se guardó (si no, se pierde)
            if res == NoteResult::Saved {
                ui.set_selected_note(-1);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_grammar_clicked(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let (word, sug, group) = {
                let s = st.borrow();
                let gi = match s.view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                let e = match s.all_errs.get(gi) {
                    Some(e) => e,
                    None => return,
                };
                (e.word.clone(), e.sug.clone(), s.view_group.get(i as usize).cloned().flatten())
            };
            let en = ui.get_lang_idx() == 1;
            let sugs = if sug.is_empty() { if en { "no suggestions".into() } else { "sin sugerencias".into() } } else { sug.join(", ") };
            let text = if en {
                format!("Grammar observation: '{word}' -> {sugs}")
            } else {
                format!("Observación de error gramatical: '{word}' -> {sugs}")
            };
            let res = if let Some(w) = group {
                let idxs: Vec<usize> = st.borrow().all_errs.iter().enumerate()
                    .filter(|(_, e)| e.word == w).map(|(gi, _)| gi).collect();
                AnnotateReq::Highlight(idxs, text)
            } else {
                let gi = match st.borrow().view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                AnnotateReq::Highlight(vec![gi], text)
            };
            apply_note(&ui, &st, res);
        });
    }    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_dlg_answer(move |yes| {
            let Some(ui) = ui_weak.upgrade() else { return };
            ui.set_dlg_open(false);
            let pending = st.borrow_mut().dlg_pending.take();
            match pending {
                Some(Pending::OpenWithPassword(path)) => {
                    if !yes {
                        return;
                    }
                    let pass = ui.get_dlg_pass().to_string();
                    if pass.is_empty() {
                        notify(&ui, &st, "Contraseña vacía".into());
                        return;
                    }
                    st.borrow_mut().password = Some(leak_secret(&pass));
                    let src = st.borrow().render_from().unwrap_or_else(|| path.clone());
                    let abierto = with_state(&st, |s| open_doc_file(s, &src));
                    match abierto {
                        Ok(()) => finish_open(&ui, &st, &path),
                        Err(e) if is_password_error(&e) => {
                            show_dlg(&ui, 1, "PDF con contraseña", "Contraseña incorrecta. Inténtalo otra vez.");
                            st.borrow_mut().dlg_pending = Some(Pending::OpenWithPassword(path));
                        }
                        Err(e) => notify(&ui, &st, format!("No se pudo abrir: {e:?}")),
                    }
                }
                Some(Pending::CreateCopy) => {
                    if !yes {
                        notify(&ui, &st, "Sin copia de trabajo no se pueden guardar notas".into());
                        return;
                    }
                    match make_working(&ui, &st) {
                        Ok(()) => {
                            let pg = st.borrow().page;
                            show(&ui, &mut st.borrow_mut(), pg);
                            push_page_notes(&ui, &st.borrow());
                            notify(&ui, &st, "Copia de trabajo lista: las notas se guardarán ahí".into());
                        }
                        Err(e) => notify(&ui, &st, format!("No se pudo crear la copia: {e:#}")),
                    }
                }
                Some(Pending::CreateCopyThenNote(req)) => {                    if !yes {
                        notify(&ui, &st, "Sin copia de trabajo no se puede guardar la nota".into());
                        return;
                    }
                    if let Err(e) = make_working(&ui, &st) {
                        notify(&ui, &st, format!("No se pudo crear la copia: {e:#}"));
                        return;
                    }
                    let res = apply_note(&ui, &st, req);
                    if res == NoteResult::Saved {
                        // la nota se guardó al final: se cierra lo que estaba abierto
                        ui.set_selected_note(-1);
                        ui.set_free_note_on(false);
                        ui.set_tool_mode(0);
                        let mut s = st.borrow_mut();
                        s.free_pt = None;
                        s.free_rects = None;
                    }
                }
                Some(Pending::ClearHistory) => {
                    if yes {
                        if let Some(f) = history_file() {
                            let _ = std::fs::remove_file(&f);
                        }
                        push_history(&ui);
                        notify(&ui, &st, "Historial limpio".into());
                    }
                }
                None => {}
            }
        });
    }
    {
        // botón persistente: crear la copia de trabajo cuando haga falta
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_make_working(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            match make_working(&ui, &st) {
                Ok(()) => {
                    let pg = st.borrow().page;
                    show(&ui, &mut st.borrow_mut(), pg);
                    push_page_notes(&ui, &st.borrow());
                    notify(&ui, &st, "Copia de trabajo lista: las notas se guardarán ahí".into());
                }
                Err(e) => notify(&ui, &st, format!("No se pudo crear la copia: {e:#}")),
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_group_toggle(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let word = {
                let s = st.borrow();
                if let Some(Some(w)) = s.view_group.get(i as usize).cloned() {
                    Some(w) // colapsado -> expandir
                } else {
                    let gi = match s.view.get(i as usize) {
                        Some(g) => *g,
                        None => return,
                    };
                    s.all_errs.get(gi).map(|e| e.word.clone()) // individual -> colapsar
                }
            };
            if let Some(w) = word {
                let mut s = st.borrow_mut();
                if s.expanded.contains(&w) { s.expanded.remove(&w); } else { s.expanded.insert(w); }
            }
            ui.set_selected_note(-1);
            push_errs(&ui, &st);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_history_open(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let path = load_history().get(i as usize).map(|e| PathBuf::from(&e.path));
            match path {
                Some(p) if p.is_file() => open_doc(&ui, &st, p),
                _ => notify(&ui, &st, "Ese archivo ya no existe.".into()),
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_clear_history(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            show_dlg(&ui, 2, "Limpiar historial",
                "Se borrará la lista de documentos recientes. El original y las copias de trabajo no se tocan.");
            st.borrow_mut().dlg_pending = Some(Pending::ClearHistory);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_type_set(move |f| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_type_filter(f);
                push_errs(&ui, &st);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_sev_set(move |f| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_sev_filter(f);
                push_errs(&ui, &st);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        ui.on_lang_set(move |i| {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_lang_idx(i);
            }
        });
    }

    // sidecar LanguageTool: SOLO el hijo propio, en un puerto libre elegido al
    // arrancar (así ningún proceso ajeno puede interceptar el texto) y con el
    // JVM encerrado: cualquier salida HTTP del LT va a un puerto muerto.
    // ponytail: el Child viaja por el canal al hilo UI; los hilos de página solo hacen HTTP
    {
        let tx = st.borrow().tx.clone();
        // con el motor desactivado (ajustes) no se arranca nada: el texto no
        // sale del proceso y no hay revisión
        if !st.borrow().settings.motor {
            st.borrow_mut().lt_dead = true;
            ui.set_lt_status("Motor desactivado (no se envía texto a ningún proceso).".into());
        } else {
        std::thread::spawn(move || {
            let Some(dir) = lt_dir() else {
                let _ = tx.send(SpellEvent::LtFail("no se encontró assets/lt (¿falló la descarga en build?)".into()));
                return;
            };
            let jar = dir.join("languagetool-server.jar");
            // hasta 3 intentos: si el puerto elegido lo captura alguien, otro
            for _intento in 0..3 {
            // puerto libre en loopback para este hijo
            let port = free_port();
            LT_PORT.store(port, std::sync::atomic::Ordering::Relaxed);
            let mut cmd = java_cmd();
            #[cfg(windows)]
            {
                // sin ventana de consola del hijo Java en Windows
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            let child = cmd
                .args(["-Xms256m", "-Xmx1g", "-Dfile.encoding=UTF-8"])
                // encerrado: el proxy del JVM apunta a un puerto muerto, así que
                // el LT no puede salir a Internet (loopback queda directo)
                .args(["-Dhttp.proxyHost=127.0.0.1", "-Dhttp.proxyPort=1"])
                .args(["-Dhttps.proxyHost=127.0.0.1", "-Dhttps.proxyPort=1"])
                .args(["-Dhttp.nonProxyHosts=127.0.0.1|localhost|[::1]"])
                .args(["-cp"])
                .arg(&jar)
                .arg("org.languagetool.server.HTTPServer")
                .args(["--port", &port.to_string()])
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(_) => {
                    let _ = tx.send(SpellEvent::LtFail("java no encontrado (se necesita Java 17+)".into()));
                    return;
                }
            };
            // espera hasta 90s a que caliente reglas
            let mut ok = false;
            for _ in 0..45 {
                std::thread::sleep(std::time::Duration::from_secs(2));
                if lt_alive() {
                    ok = true;
                    break;
                }
                // si el hijo murió, no tiene sentido seguir esperando
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    _ => {}
                }
            }
            if ok {
                let _ = tx.send(SpellEvent::LtReady(Some(child)));
                return;
            }
            let _ = child.kill();
            } // fin de los intentos
            let _ = tx.send(SpellEvent::LtFail("el servidor LT no respondió a tiempo".into()));
        });
        }
    }

    // ponytail: abrir por argv evita el diálogo para pruebas/demos; quitar si molesta
    if let Some(arg) = std::env::args().nth(1) {
        let p = PathBuf::from(&arg);
        if p.is_file() {
            open_doc(&ui, &st, p);
        }
    }
    push_history(&ui);

    ui.run()?;
    // apaga el sidecar propio (si se reutilizó uno externo, no se toca)
    if let Some(mut c) = st.borrow_mut().lt_child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Fixture para anotar: temp_dir se limpia solo; se restaura desde input/.
    fn fixture_pdf() -> PathBuf {
        let tmp = std::env::temp_dir().join("doc3.pdf");
        if !tmp.is_file() {
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("input/El-viejo-y-el-mar.pdf");
            std::fs::copy(&src, &tmp).expect("ni input/El-viejo-y-el-mar.pdf existe");
        }
        tmp
    }
    #[test]
    fn pdf_to_image_flip_y() {
        // pág 100x200pts -> img 1000x2000px; box inferior-izq (10,20)-(30,40)
        let (x, y, w, h) = pdf_to_image(10.0, 20.0, 30.0, 40.0, 100.0, 200.0, 1000, 2000);
        assert!((x - 100.0).abs() < 0.01 && (w - 200.0).abs() < 0.01);
        assert!((y - 1600.0).abs() < 0.01 && (h - 200.0).abs() < 0.01);
    }
    #[test]
    fn sev() {
        assert_eq!(kind_of_lt("TYPOS"), "Ortografía");
        assert_eq!(kind_of_lt("GRAMMAR"), "Sintaxis");
        assert_eq!(sev_of_lt("TYPOS"), "intermedia");
        assert_eq!(sev_of_lt("GRAMMAR"), "grave");
        assert_eq!(sev_of_lt("PUNCTUATION"), "intermedia");
        assert_eq!(sev_of_lt("STYLE"), "mínima");
        assert_eq!(sev_of_lt(""), "mínima");
        assert_eq!(worst_sev(["mínima", "grave", "intermedia"].into_iter()), "grave");
        assert_eq!(worst_sev(["mínima"].into_iter()), "mínima");
    }
    #[test]
    fn order() {
        let m = |page, x0, y1| Misspelling { word: "x".into(), sug: vec![], page, x0, y0: 0.0, x1: x0 + 5.0, y1, dismissed: false, kind: "Ortografía".into(), sev: "intermedia".into() };
        let mut v = vec![m(1, 0.0, 10.0), m(0, 50.0, 100.0), m(0, 10.0, 100.0), m(0, 10.0, 50.0)];
        v.sort_by(err_order);
        assert_eq!(v.iter().map(|e| (e.page, e.x0, e.y1)).collect::<Vec<_>>(),
            vec![(0, 10.0, 100.0), (0, 50.0, 100.0), (0, 10.0, 50.0), (1, 0.0, 10.0)]);
    }
    #[test]
    fn overlap() {
        let m = |page, x0, x1| Misspelling { word: "x".into(), sug: vec![], page, x0, y0: 0.0, x1, y1: 5.0, dismissed: false, kind: "Ortografía".into(), sev: "intermedia".into() };
        assert!(boxes_overlap(&m(0, 0.0, 10.0), &m(0, 5.0, 15.0)));
        assert!(!boxes_overlap(&m(0, 0.0, 10.0), &m(0, 10.0, 20.0)));
        assert!(!boxes_overlap(&m(0, 0.0, 10.0), &m(1, 0.0, 10.0)));
    }
    #[test]
    fn boot_fills_without_looping() {
        assert_eq!(boot_pct(0), 0);
        assert_eq!(boot_pct(45), 50);
        assert_eq!(boot_pct(900), 99); // tope: nunca 100 ni bucle
    }
    #[test]
    fn progress_waits_for_threads() {
        assert_eq!(progress_state(10, 3, 0, 0), (15, false)); // extracción: mitad del tramo
        assert_eq!(progress_state(10, 10, 4, 2), (75, false)); // 2 de 4 respuestas
        assert_eq!(progress_state(10, 10, 4, 0), (100, true));
        assert_eq!(progress_state(10, 10, 0, 0), (100, true)); // sin motor: cierra
    }
    #[test]
    fn only_pdfs_pass() {
        let dir = std::env::temp_dir();
        let good = dir.join("lexpdf_t.pdf");
        let fake = dir.join("lexpdf_t.pdf"); // se reescribe abajo
        let exe = dir.join("lexpdf_t.exe");
        std::fs::write(&good, b"%PDF-1.7 resto").unwrap();
        assert!(is_pdf(&good));
        std::fs::write(&fake, b"MZ\x90\x00disfrazado").unwrap();
        assert!(!is_pdf(&fake)); // extensión pdf pero magia EXE
        std::fs::write(&exe, b"%PDF-1.7 con ext mala").unwrap();
        assert!(!is_pdf(&exe));
        let upper = dir.join("lexpdf_t.PDF");
        std::fs::write(&upper, b"%PDF-1.4").unwrap();
        assert!(is_pdf(&upper));
        for p in [&good, &fake, &exe, &upper] {
            let _ = std::fs::remove_file(p);
        }
    }
    #[test]
    fn history_tamper_drops_one() {
        let good = serde_json::json!({"path": "/tmp/a.pdf", "pages": 3, "errors": 1, "resolved": 0, "notes": 0, "mtime": 1});
        let bad_type = serde_json::json!({"path": "/tmp/b.pdf", "pages": "tres", "errors": 1, "resolved": 0, "notes": 0, "mtime": 1});
        let bad_range = serde_json::json!({"path": "/tmp/c.pdf", "pages": 0, "errors": 1, "resolved": 0, "notes": 0, "mtime": 1});
        let bad_path = serde_json::json!({"path": "", "pages": 3, "errors": 1, "resolved": 0, "notes": 0, "mtime": 1});
        let v = sanitize_history(vec![good, bad_type, bad_range, bad_path]);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].path, "/tmp/a.pdf");
    }
    #[test]
    fn loopback_tripwire() {        assert!(is_loopback_url(&lt_url("/v2/check")));
        assert!(is_loopback_url("http://127.0.0.1:8081/v2/check"));
        assert!(!is_loopback_url("http://192.168.1.1:8081/v2/check"));
        assert!(!is_loopback_url("http://localhost.evil.com/v2/check"));
        assert!(!is_loopback_url("https://localhost:8081/v2/check"));
        assert!(sec_line(12).contains("sin salir del PC"));
    }
    #[test]
    fn span_mapping() {
        let ws = vec![
            Word { text: "She".into(), x0: 0.0, y0: 0.0, x1: 10.0, y1: 5.0 },
            Word { text: "was".into(), x0: 11.0, y0: 0.0, x1: 20.0, y1: 5.0 },
            Word { text: "not".into(), x0: 21.0, y0: 0.0, x1: 30.0, y1: 5.0 },
        ];
        let (t, r) = join_words(&ws);
        assert_eq!(t, "She was not");
        assert_eq!(r.len(), 3);
        let b = span_to_box(&ws, &r, 4, 11).unwrap(); // "was not" parcial
        assert_eq!(b.4, "was not");
        assert!((b.0 - 11.0).abs() < 0.01 && (b.2 - 30.0).abs() < 0.01);
        assert!(span_to_box(&ws, &r, 50, 60).is_none());
    }
    #[test]
    fn lt_parse_and_map() {
        // respuesta /v2/check enlatada (offsets en chars): sin servidor
        let body = r#"{"matches":[
            {"offset":22,"length":6,"replacements":[{"value":"que"}]},
            {"offset":0,"length":0,"replacements":[{"value":"x"}]},
            {"offset":5,"length":3,"replacements":[]}
        ]}"#;
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let ms = v["matches"].as_array().unwrap();
        assert_eq!(ms.len(), 3);
        // el filtro real (lt_check) descartaría length 0 y sin reemplazos;
        // aquí se verifica el mapeo chars->box con ñ multibyte
        let ws = vec![
            Word { text: "El".into(), x0: 0.0, y0: 0.0, x1: 5.0, y1: 5.0 },
            Word { text: "niño".into(), x0: 6.0, y0: 0.0, x1: 15.0, y1: 5.0 },
            Word { text: "juega".into(), x0: 16.0, y0: 0.0, x1: 25.0, y1: 5.0 },
        ];
        let (t, r) = join_words(&ws);
        assert_eq!(t, "El niño juega");
        assert_eq!(r[1], 3..7); // chars, no bytes (niño = 4 chars)
        let b = span_to_box(&ws, &r, 3, 7).unwrap();
        assert_eq!(b.4, "niño");
    }
    #[test]
    fn underline_paints_red() {
        let mut img = image::RgbaImage::from_pixel(10, 10, image::Rgba([255, 255, 255, 255]));
        underline(&mut img, 1.0, 1.0, 5.0, (211, 47, 47));
        let p = img.get_pixel(2, 2);
        assert!(p[0] > 200 && p[1] < 150 && p[2] < 150);
        assert_eq!(img.get_pixel(0, 0), &image::Rgba([255, 255, 255, 255]));
        let mut img = image::RgbaImage::from_pixel(10, 10, image::Rgba([255, 255, 255, 255]));
        underline(&mut img, 1.0, 1.0, 5.0, under_color("Sintaxis"));
        let p = img.get_pixel(2, 2);
        assert!(p[2] > p[0]); // morado, no rojo
        assert_eq!(under_color("Ortografía"), (211, 47, 47));
    }
    /// Estado mínimo para tests (sin UI). Ojo: usar Pdfium cuelga si se
    /// instancea dos veces (un solo test debe tocarlo).
    fn state_for(path: PathBuf, zoom: f32) -> State {
        let (tx, _rx) = channel::<SpellMsg>();
        State {
            pdfium: load_pdfium(),
            doc: None, doc_from: None, doc_stamp: None,
            path: Some(path), working: None, password: None, dlg_pending: None,
            settings: Settings::default(), err_anno: HashMap::new(), free_rects: None,
            page: 0, total: 3,
            words: vec![], page_w: 0.0, page_h: 0.0, img_w: 0, img_h: 0,
            all_errs: vec![
                Misspelling { word: "herror".into(), sug: vec!["error".into()], page: 0, x0: 72.0, y0: 700.0, x1: 130.0, y1: 712.0, dismissed: false, kind: "Ortografía".into(), sev: "intermedia".into() },
                Misspelling { word: "segundo".into(), sug: vec!["otro".into()], page: 1, x0: 72.0, y0: 600.0, x1: 130.0, y1: 612.0, dismissed: false, kind: "Sintaxis".into(), sev: "mínima".into() },
            ],
            doc_notes: vec![],
            scan_id: 0, scan_queue: vec![], scan_pos: 0, popup_until: None, zoom, free_pt: None, tx, last_lang: 0, flash: None, view: vec![], view_group: vec![], expanded: HashSet::new(), lt_ready: true, lt_dead: false, pending: 0, jobs_total: 0, sec_bytes: 0, boot_start: std::time::Instant::now(),
        syn_received: false, lt_child: None,
        }
    }

    /// Un solo test toca Pdfium (instanciarlo dos veces se cuelga): aquí van
    /// encadenados crear, comprobar el parcheo y borrar.
    #[test]
    fn anotaciones_en_copia_de_trabajo() {
        let src = fixture_pdf();
        let mut st = state_for(src.clone(), 1.0);
        // la copia de trabajo se crea y es privada
        let out = create_working_copy(&mut st).unwrap();
        assert_eq!(out.file_name().unwrap(), "doc3_anotado.pdf");
        assert!(out.metadata().unwrap().len() > 500);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "la copia de trabajo debe ser solo del usuario");
        }
        // se anota sobre el documento abierto y se guarda al momento
        st.working = Some(out.clone());
        open_doc_file(&mut st, &out).unwrap();
        let added = annotate_open(&mut st, &AnnotateReq::Highlight(vec![0], "nota redonda".into())).unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].kind, AnnoKind::Highlight);
        // la nota se lee del fichero guardado (roundtrip)
        let notes = read_notes(st.pdfium, &out);
        assert!(notes.iter().any(|n| n.text.contains("nota redonda") && n.page == 0));
        // el resaltado lleva /QuadPoints (si no, no se ve en ningún lector)
        {
            let doc = lopdf::Document::load(&out).unwrap();
            let pages = doc.get_pages();
            let page = doc.get_object(*pages.get(&1u32).unwrap()).unwrap();
            let anns = match lget(&page, b"Annots") {
                Some(lopdf::Object::Array(v)) => v,
                _ => panic!("sin anotaciones"),
            };
            let mut found = false;
            for a in anns {
                let obj = match &a {
                    lopdf::Object::Reference(id) => doc.get_object(*id).unwrap().clone(),
                    other => other.clone(),
                };
                if lget(&obj, b"Subtype") == Some(lopdf::Object::Name(b"Highlight".to_vec())) {
                    assert!(lget(&obj, b"QuadPoints").is_some(), "falta /QuadPoints");
                    assert!(lget(&obj, b"CA").is_some(), "falta /CA (transparencia)");
                    // ORDEN DEL SPEC (UL, UR, LL, LR): con el orden inverso pdfium
                    // pinta 27 px (un punto) en vez de la palabra entera
                    let qp = match lget(&obj, b"QuadPoints") {
                        Some(lopdf::Object::Array(v)) => {
                            v.iter().filter_map(|o| o.as_float().ok()).collect::<Vec<f32>>()
                        }
                        _ => panic!("/QuadPoints no es un array"),
                    };
                    assert_eq!(qp.len(), 8, "se esperaban 8 coordenadas");
                    let rect = match lget(&obj, b"Rect") {
                        Some(lopdf::Object::Array(v)) => {
                            v.iter().filter_map(|o| o.as_float().ok()).collect::<Vec<f32>>()
                        }
                        _ => panic!("/Rect no es un array"),
                    };
                    let (x0, y0, x1, y1) = (rect[0], rect[1], rect[2], rect[3]);
                    let near = |a: f32, b: f32| (a - b).abs() < 0.01;
                    assert!(near(qp[0], x0) && near(qp[1], y1), "UL incorrecto: {qp:?}");
                    assert!(near(qp[2], x1) && near(qp[3], y1), "UR incorrecto: {qp:?}");
                    assert!(near(qp[4], x0) && near(qp[5], y0), "LL incorrecto: {qp:?}");
                    assert!(near(qp[6], x1) && near(qp[7], y0), "LR incorrecto: {qp:?}");
                    found = true;
                }
            }
            assert!(found, "no hay resaltados en el PDF");
        }
        // y se puede borrar (desaparece del PDF)
        st.doc_notes.extend(added);
        delete_anno(&mut st, 0).unwrap();
        assert!(st.doc_notes.is_empty());
        {
            let doc = lopdf::Document::load(&out).unwrap();
            let pages = doc.get_pages();
            let page = doc.get_object(*pages.get(&1u32).unwrap()).unwrap();
            let n = match lget(&page, b"Annots") {
                Some(lopdf::Object::Array(v)) => v.len(),
                _ => 0,
            };
            assert_eq!(n, 0, "quedan anotaciones en el PDF");
        }
        // Al borrar uno, los OTROS deben seguir con sus /QuadPoints (si no,
        // se ve el comentario pero el resaltado desaparece).
        let b = annotate_open(&mut st, &AnnotateReq::Highlight(vec![1], "otro".into())).unwrap();
        st.doc_notes.extend(b);
        assert_eq!(count_highlights_with_quads(&out), 1);
        delete_anno(&mut st, 0).unwrap();
        assert_eq!(count_highlights_with_quads(&out), 0, "no debe quedar ningún resaltado");
        let _ = std::fs::remove_file(&out);
    }

    /// Nº de resaltados del fichero que tienen /QuadPoints (o sea, que se ven).
    fn count_highlights_with_quads(path: &std::path::Path) -> usize {
        let doc = match lopdf::Document::load(path) {
            Ok(d) => d,
            Err(_) => return 0,
        };
        let mut n = 0;
        for page_id in doc.get_pages().values() {
            let page = match doc.get_object(*page_id) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(lopdf::Object::Array(anns)) = lget(&page, b"Annots") {
                for a in anns {
                    let obj = match &a {
                        lopdf::Object::Reference(id) => match doc.get_object(*id) {
                            Ok(o) => o.clone(),
                            Err(_) => continue,
                        },
                        other => other.clone(),
                    };
                    if lget(&obj, b"Subtype") == Some(lopdf::Object::Name(b"Highlight".to_vec()))
                        && lget(&obj, b"QuadPoints").is_some()
                    {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    #[test]
    fn el_prestamo_se_suelta_antes_de_usar_el_resultado() {
        // el helper existe para esto: si el RefMut sobrevive al match, el
        // RefCell peta ("RefCell already borrowed") y la app se cae
        let c = Rc::new(RefCell::new(vec![1, 2, 3]));
        let res = with_state(&c, |v: &mut Vec<i32>| v.len());
        match res {
            n => {
                c.borrow_mut().push(n as i32); // otro préstamo: no debe petar
                assert_eq!(c.borrow().len(), 4);
            }
        }
    }

    #[test]
    fn el_rectangulo_de_la_pantalla_pasa_a_puntos_pdf() {
        // pág 612x792 pt, imagen 1224x1584 px (escala 2)
        let (x0, y0, x1, y1) = px_rect_to_pdf(100.0, 200.0, 300.0, 400.0, 612.0, 792.0, 1224.0, 1584.0);
        assert!((x0 - 50.0).abs() < 0.01 && (x1 - 150.0).abs() < 0.01);
        // la y se invierte: arriba de la imagen = abajo del PDF
        assert!((y0 - 592.0).abs() < 0.01, "y0={y0}");
        assert!((y1 - 692.0).abs() < 0.01, "y1={y1}");
        // arrastrar al revés da el mismo rectángulo
        let (a, b, c, d) = px_rect_to_pdf(300.0, 400.0, 100.0, 200.0, 612.0, 792.0, 1224.0, 1584.0);
        assert!((a - x0).abs() < 0.01 && (b - y0).abs() < 0.01);
        assert!((c - x1).abs() < 0.01 && (d - y1).abs() < 0.01);
    }

    /// Las palabras que toca el rectángulo se resaltan (como al seleccionar
    /// texto en un PDF); las de fuera, no.
    #[test]
    fn el_resaltado_por_palabras_solo_coge_las_que_toca() {
        let w = vec![
            Word { text: "uno".into(), x0: 10.0, y0: 100.0, x1: 40.0, y1: 110.0 },
            Word { text: "dos".into(), x0: 50.0, y0: 100.0, x1: 80.0, y1: 110.0 },
            Word { text: "tres".into(), x0: 90.0, y0: 100.0, x1: 120.0, y1: 110.0 },
            Word { text: "abajo".into(), x0: 10.0, y0: 50.0, x1: 40.0, y1: 60.0 },
        ];
        // un rectángulo que pilla "uno" y "dos"
        let hits = words_in_rect(&w, [5.0, 95.0, 85.0, 115.0]);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0][0], 10.0);
        assert_eq!(hits[1][2], 80.0);
        // uno que solo pilla "dos"
        assert_eq!(words_in_rect(&w, [45.0, 95.0, 85.0, 115.0]).len(), 1);
        // uno que no pilla nada
        assert!(words_in_rect(&w, [200.0, 200.0, 300.0, 300.0]).is_empty());
    }

    /// El alto del texto de la licencia se estima para que el Flickable
    /// pueda desplazar (si no, el texto se sale de la ventana).
    #[test]
    fn el_alto_de_la_licencia_crece_con_el_texto() {
        let corto = license_height("una linea\n");
        let largo = license_text(&["GPL3-LexPDF.txt"]);
        assert!(license_height(&largo) > corto * 10, "el texto largo debe más alto");
        assert!(license_height(&largo) < 60000);
    }

    /// El texto seleccionado sale en orden de lectura (línea por línea).
    #[test]
    fn el_texto_seleccionado_sale_en_orden() {
        let w = vec![
            Word { text: "uno".into(), x0: 10.0, y0: 100.0, x1: 40.0, y1: 110.0 },
            Word { text: "dos".into(), x0: 50.0, y0: 100.0, x1: 80.0, y1: 110.0 },
            Word { text: "tres".into(), x0: 10.0, y0: 50.0, x1: 40.0, y1: 60.0 },
            Word { text: "fuera".into(), x0: 300.0, y0: 50.0, x1: 340.0, y1: 60.0 },
        ];
        // la línea de arriba va primero, y dentro de la línea de izquierda a derecha
        assert_eq!(text_in_rect(&w, [5.0, 40.0, 90.0, 115.0]), "uno dos tres");
        assert_eq!(text_in_rect(&w, [200.0, 200.0, 300.0, 300.0]), "");
    }

    /// Los ajustes se leen del JSON (con el campo nuevo incluido).
    #[test]
    fn los_ajustes_se_leen_del_json() {
        let s: Settings =
            serde_json::from_str(r#"{"hl_color":1,"copy_mode":2,"motor":false,"free_sel":1,"free_text":1}"#).unwrap();
        assert_eq!(s.hl_color, 1);
        assert_eq!(s.copy_mode, 2);
        assert!(!s.motor);
        assert_eq!(s.free_sel, 1);
        assert_eq!(s.free_text, 1);
        // un fichero de ajustes viejo (sin el campo nuevo) se sigue leyendo
        let viejo: Settings =
            serde_json::from_str(r#"{"hl_color":0,"copy_mode":0,"motor":true,"free_sel":0}"#).unwrap();
        assert_eq!(viejo.free_text, Settings::default().free_text);
        // por defecto: selección de texto (no por recuadro)
        assert_eq!(Settings::default().free_text, 1);
    }

    /// El texto de cada licencia debe ser el de verdad (y no el de otra).
    #[test]
    fn los_textos_de_licencia_son_los_correctos() {
        // cada texto real de cada repo (nada de genéricos compartidos)
        let entries = license_entries();
        let first = license_text(entries[0].files);     // LexPDF: GPL-3
        assert!(first.contains("GNU GENERAL PUBLIC LICENSE"), "no es la GPL:\n{}", &first[..first.len().min(120)]);
        let slint = license_text(entries[1].files);     // Slint: GPL-3
        assert!(slint.contains("GNU GENERAL PUBLIC LICENSE"), "no es la GPL de Slint");
        let pdfium = license_text(&["BSD3-PDFium.txt"]);
        assert!(pdfium.contains("PDFium Authors"), "no es la BSD de PDFium");
        let pdfr_mit = license_text(&["MIT-pdfium-render.txt"]);
        assert!(pdfr_mit.contains("MIT License") && pdfr_mit.contains("Alastair Carey"), "no es la MIT de pdfium-render");
        let lt = license_text(&["LGPL21-LT.txt"]);
        assert!(lt.contains("GNU LESSER GENERAL PUBLIC LICENSE"), "no es la LGPL");
        let mit = license_text(&["MIT-lopdf.txt"]);
        assert!(mit.contains("MIT License") && mit.contains("Junfeng Liu"), "no es la MIT de lopdf");
        // y ninguna lista puede traer "no se encontró"
        for e in &entries {
            let t = license_text(e.files);
            assert!(!t.contains("no se encontró") && !t.contains("no se pudo leer"), "{}: {}", e.name, &t[..t.len().min(80)]);
        }
    }

    /// Un resaltado por texto sale continuo: una banda por línea, sin huecos
    /// entre palabras.
    #[test]
    fn el_resaltado_por_texto_sale_continuo_por_linea() {
        let w = vec![
            Word { text: "uno".into(), x0: 10.0, y0: 100.0, x1: 40.0, y1: 110.0 },
            Word { text: "dos".into(), x0: 45.0, y0: 100.0, x1: 70.0, y1: 110.0 },
            Word { text: "tres".into(), x0: 10.0, y0: 80.0, x1: 40.0, y1: 90.0 },
            Word { text: "lejos".into(), x0: 200.0, y0: 100.0, x1: 240.0, y1: 110.0 },
        ];
        let lineas = words_in_lines(&w, [5.0, 75.0, 100.0, 115.0]);
        assert_eq!(lineas.len(), 2, "una banda por línea");
        // la primera línea une las dos palabras (sin hueco)
        assert_eq!(lineas[0][0], 10.0);
        assert_eq!(lineas[0][2], 70.0, "debe abarcar hasta la última palabra");
        // la segunda línea va aparte
        assert!(lineas.iter().any(|l| l[3] <= 91.0));
    }

    #[test]
    fn working_path_never_doubles_the_suffix() {
        assert_eq!(working_path(std::path::Path::new("/tmp/doc.pdf")), PathBuf::from("/tmp/doc_anotado.pdf"));
        // ya es copia de trabajo: se usa tal cual (nada de _anotado_anotado)
        assert_eq!(working_path(std::path::Path::new("/tmp/doc_anotado.pdf")), PathBuf::from("/tmp/doc_anotado.pdf"));
        assert_eq!(working_path(std::path::Path::new("/tmp/doc_anotado_anotado.pdf")), PathBuf::from("/tmp/doc_anotado_anotado.pdf"));
    }

    /// El resaltado debe quedar en el PDF con /QuadPoints (si no, no se ve en
    /// ningún lector) y con el texto del documento intacto.

    /// Borrar una anotación la quita del PDF (y de la lista en memoria).

    #[test]
    fn every_listed_license_file_exists() {
        // si falta un .txt ya no se ve "no se encontró X" dentro de la app
        for e in license_entries() {
            for f in e.files {
                assert!(asset_file(f).is_some(), "falta assets/licenses/{f} (entró {})", e.name);
            }
        }
    }

    #[test]
    fn working_copy_is_private() {        // nombre propio: los tests corren en paralelo y comparten /tmp
        let src = std::env::temp_dir().join("doc3_priv.pdf");
        std::fs::copy(fixture_pdf(), &src).unwrap();
        let out = copy_to_working(&src).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "la copia de trabajo debe ser solo del usuario");
        }
        let _ = std::fs::remove_file(&out);
        let _ = std::fs::remove_file(&src);
    }
}
