// SPDX-License-Identifier: GPL-3.0-only
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
use slint::{Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, StyledText, Timer, TimerMode, VecModel};

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
    // ponytail: recargamos el doc por página para evitar líos de lifetimes Pdfium/PdfDocument; cachear si va lento
    pdfium: Pdfium,
    path: Option<PathBuf>,
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

#[derive(Clone, Debug)]
struct DocNote {
    page: u32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    text: String,
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
        ("Sintaxis", false) => "Posible error de sintaxis (LanguageTool)",
        ("Sintaxis", true) => "Possible grammar issue (LanguageTool)",
        (_, false) => "Posible falta de ortografía (LanguageTool)",
        (_, true) => "Possible spelling mistake (LanguageTool)",
    }
}

enum SpellEvent {
    LtReady(Option<std::process::Child>), // hijo java propio
    LtFail(String), // java ausente o LT no arranca: sin revisión hasta que haya servidor
    Syn(u64, Vec<Misspelling>), // (gen, hallazgos LT de una página: ortografía + sintaxis)
}
type SpellMsg = SpellEvent;

const LT_PORT: u16 = 8081;

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
    format!("http://localhost:{LT_PORT}{path}")
}

/// Tripwire: el texto del PDF solo puede viajar a loopback. Puro y testeable.
/// Ojo con `localhost.evil.com`: se compara el host exacto, no prefijo.
fn is_loopback_url(url: &str) -> bool {
    let after = url.strip_prefix("http://").unwrap_or("");
    let host = after.split(['/', ':']).next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

/// ¿Hay un servidor LT respondiendo? (sirve para reutilizar uno ya abierto).
fn lt_alive() -> bool {
    ureq::get(&lt_url("/v2/languages"))
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
    let (all, expanded) = {
        let s = st.borrow();
        (s.all_errs.clone(), s.expanded.clone())
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
            rows.push(ErrRow {
                word: format!("{} (×{n})", e.word).into(),
                sug: if sug.is_empty() {
                    (if en { "No suggestions" } else { "No hay sugerencias" }).into()
                } else {
                    format!("→ {} | {}", sug.join(", "), if en { "tap for pages" } else { "toca para ver páginas" }).into()
                },
                page: e.page as i32,
                dimmed,
                kind: e.kind.clone().into(),
                sev: gsev.into(),
                bar: kind_bar(gsev),
                desc: (if en { format!("Repeated {n} times — comment/reject applies to all") } else { format!("Se repite {n} veces — comentar/rechazar aplica a todas") }).into(),
                count: n as i32,
                is_group: true,
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
        let Some(path) = s.path.clone() else { return false };
        while s.scan_pos < s.scan_queue.len() && t.elapsed() < std::time::Duration::from_millis(80) {
            let i = s.scan_queue[s.scan_pos];
            s.scan_pos += 1;
            let words = {
                let Ok(doc) = s.pdfium.load_pdf_from_file(&path, None) else { continue };
                let Ok(pg) = doc.pages().get(i as u16) else { continue };
                extract_words(&pg)
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
    format!("Motor: listo (local :8081 · {kb} KB revisados sin salir del PC).")
}

/// Destino de anotaciones + base a abrir: si ya existe `{nombre}_anotado.pdf`
/// se anota ENCIMA (acumula), si no se parte del original. Nunca se toca el original.
/// ponytail: si el _anotado es de otra sesión con otro estado, igual vale (mismo layout)
fn annotate_base(st: &State) -> anyhow::Result<(PdfDocument, PathBuf)> {
    let src = st.path.clone().context("sin documento")?;
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("doc");
    let out = src.with_file_name(format!("{stem}_anotado.pdf"));
    let base = if out.is_file() { &out } else { &src };
    let doc = st.pdfium.load_pdf_from_file(base, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    Ok((doc, out))
}

/// Lee las sticky-notes (Text) ya existentes en el documento.
/// Puro dato (sin lifetimes Pdfium): tolera anotaciones sin texto o sin box.
fn read_notes(pdfium: &Pdfium, path: &PathBuf) -> Vec<DocNote> {
    let mut out = vec![];
    let Ok(doc) = pdfium.load_pdf_from_file(path, None) else { return out };
    let pages = doc.pages();
    for i in 0..pages.len() {
        let Ok(pg) = pages.get(i) else { continue };
        for ann in pg.annotations().iter() {
            if ann.annotation_type() != PdfPageAnnotationType::Text {
                continue;
            }
            let Some(text) = ann.contents() else { continue };
            if text.trim().is_empty() {
                continue;
            }
            let (x0, y0, x1, y1) = match ann.bounds() {
                Ok(b) => (b.left().value, b.bottom().value, b.right().value, b.top().value),
                Err(_) => continue,
            };
            out.push(DocNote { page: i as u32, x0, y0, x1, y1, text });
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
    }
    let _ = std::fs::write(&f, serde_json::to_string(&v).unwrap_or_default());
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
fn open_doc(ui: &AppWindow, st: &Rc<RefCell<State>>, path: PathBuf) {
    if !is_pdf(&path) {
        ui.set_popup_text("Solo se admiten documentos PDF.".into());
        st.borrow_mut().popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        return;
    }
    let notes = read_notes(&st.borrow().pdfium, &path);
    st.borrow_mut().path = Some(path);
    st.borrow_mut().doc_notes = notes;
    show(ui, &mut st.borrow_mut(), 0);
    start_scan(ui, &mut st.borrow_mut());
    touch_history(&st.borrow());
    push_history(ui);
}

/// Relee las notas del PDF tras anotar (acumulan) y repinta los marcadores + historial.
fn refresh_notes(ui: &AppWindow, st: &Rc<RefCell<State>>) {
    let path = st.borrow().path.clone();
    if let Some(p) = path {
        let notes = read_notes(&st.borrow().pdfium, &p);
        st.borrow_mut().doc_notes = notes;
    }
    touch_history(&st.borrow());
    push_history(ui);
    let pg = st.borrow().page;
    show(ui, &mut st.borrow_mut(), pg); // repinta marcadores
}
fn marker(img: &mut image::RgbaImage, x: f32, y: f32) {
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    for dx in 0..8i64 {
        for dy in 0..8i64 {
            let (px, py) = (x.round() as i64 + dx, y.round() as i64 + dy);
            if px < 0 || py < 0 || px >= iw || py >= ih {
                continue;
            }
            let p = img.get_pixel_mut(px as u32, py as u32);
            p[0] = 255;
            p[1] = 179;
            p[2] = 0;
        }
    }
}

/// Inyecta una nota nativa (sticky-note) en el box del error y guarda
/// en `{nombre}_anotado.pdf`. Nunca sobreescribe el original.
fn annotate(st: &State, idx: usize, text: &str) -> anyhow::Result<PathBuf> {
    let e = st.all_errs.get(idx).context("hallazgo no válido")?;
    st.path.clone().context("sin documento")?;
    let text = if text.trim().is_empty() {
        format!("Revisar: {} (sugerencias: {})", e.word, e.sug.join(", "))
    } else {
        text.to_string()
    };
    let (doc, out) = annotate_base(st)?;
    let mut pg = doc.pages().get(e.page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut ann = pg.annotations_mut().create_text_annotation(&text).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ann.set_bounds(PdfRect::new_from_values(e.y0, e.x0, e.y1, e.x1))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    save_doc(&doc, &out)?;
    Ok(out)
}

/// Anota N hallazgos de una vez (grupo): abre el doc una sola vez.
/// ponytail: evita N open/save que se pisaban; techo = todo el grupo en memoria, bien para <1000
fn annotate_many(st: &State, idxs: &[usize], text: &str) -> anyhow::Result<PathBuf> {
    st.path.clone().context("sin documento")?;
    anyhow::ensure!(!idxs.is_empty(), "grupo vacío");
    let (doc, out) = annotate_base(st)?;
    for idx in idxs {
        let e = st.all_errs.get(*idx).context("hallazgo no válido")?;
        let t = if text.trim().is_empty() {
            format!("Revisar: {} (sugerencias: {})", e.word, e.sug.join(", "))
        } else {
            text.to_string()
        };
        let mut pg = doc.pages().get(e.page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let mut ann = pg.annotations_mut().create_text_annotation(&t).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        ann.set_bounds(PdfRect::new_from_values(e.y0, e.x0, e.y1, e.x1))
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    }
    save_doc(&doc, &out)?;
    Ok(out)
}

/// Nota libre en un punto cualquiera de la página (coords PDF, origen abajo-izq).
fn annotate_at(st: &State, page: u32, x: f32, y: f32, text: &str) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(!text.trim().is_empty(), "escribe la nota primero");
    let (doc, out) = annotate_base(st)?;
    let mut pg = doc.pages().get(page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let mut ann = pg.annotations_mut().create_text_annotation(text).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    ann.set_bounds(PdfRect::new_from_values((y - 6.0).max(0.0), (x - 6.0).max(0.0), y + 6.0, x + 6.0))
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    save_doc(&doc, &out)?;
    Ok(out)
}

fn show(ui: &AppWindow, st: &mut State, page: u32) {
    let Some(path) = st.path.clone() else { return };
    let res: anyhow::Result<(Image, u32, Vec<Word>, f32, f32, u32, u32)> = (|| {
        let doc = st.pdfium.load_pdf_from_file(&path, None).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let pages = doc.pages();
        let total = pages.len() as u32;
        let pg = pages.get(page as u16).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let (pw, ph) = (pg.width().value, pg.height().value);
        let config = PdfRenderConfig::new().set_target_width((1400.0 * st.zoom) as i32);
        let mut rgba = pg.render_with_config(&config).map_err(|e| anyhow::anyhow!("{e:?}"))?
            .as_image().as_rgba8().context("pdfium no devolvió RGBA")?.clone();
        let (iw, ih) = (rgba.width(), rgba.height());
        for h in st.all_errs.iter().filter(|e| e.page == page && !e.dismissed) {
            let (x, y, w, hh) = pdf_to_image(h.x0, h.y0, h.x1, h.y1, pw, ph, iw, ih);
            underline(&mut rgba, x, y + hh - 3.0, w, under_color(&h.kind));
        }
        for n in st.doc_notes.iter().filter(|n| n.page == page) {
            let (x, y, _, _) = pdf_to_image(n.x0, n.y0, n.x1, n.y1, pw, ph, iw, ih);
            marker(&mut rgba, x, y);
        }
        // flash amarillo 5s del error clicado
        if let Some((fi, t)) = &st.flash {
            if t.elapsed() < std::time::Duration::from_secs(5) {
                if let Some(h) = st.all_errs.get(*fi) {
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
            // notas nativas de esta página: el Slint pone el tooltip al pasar el ratón
            let pn: Vec<NoteRow> = st
                .doc_notes
                .iter()
                .filter(|n| n.page == page)
                .map(|n| {
                    let (x, y, _, _) = pdf_to_image(n.x0, n.y0, n.x1, n.y1, pw, ph, iw, ih);
                    let mut t: String = n.text.chars().take(200).collect();
                    if n.text.chars().count() > 200 {
                        t.push('…');
                    }
                    NoteRow { text: StyledText::from_plain_text(&t), page: n.page as i32, x, y }
                })
                .collect();
            ui.set_page_notes(ModelRc::new(VecModel::from_slice(&pn)));
            ui.set_note_total(st.doc_notes.len() as i32);
        }
        Err(e) => {
            ui.set_popup_text(format!("Error: {e:#}").into());
            st.popup_until = Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
        }
    }
}

fn load_pdfium() -> Pdfium {
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
    if std::env::var("PDF_DARK").is_ok() {
        ui.set_is_dark(true);
    }
    let (tx, rx) = channel::<SpellMsg>();
    let st = Rc::new(RefCell::new(State {
        pdfium: load_pdfium(),
        path: None,
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
                        ui.set_lt_status("Motor: listo (local :8081).".into());
                        notify(&ui, &st, "Revisión lista (LanguageTool local).".into());
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
            st.borrow_mut().free_pt = None; // armado: el toque en el doc fija el punto
            ui.set_free_note_text("".into());
            ui.set_free_note_on(true);
            notify(&ui, &st, "Toca el punto del documento para la nota".into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_page_clicked(move |mx, my| {
            let Some(ui) = ui_weak.upgrade() else { return };
            if !ui.get_free_note_on() {
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
            notify(&ui, &st, "Punto marcado: escribe y pulsa Guardar".into());
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_free_note_save(move || {
            let Some(ui) = ui_weak.upgrade() else { return };
            let text = ui.get_free_note_text().to_string();
            let pt = st.borrow().free_pt;
            let res = match pt {
                Some((pg, x, y)) => annotate_at(&st.borrow(), pg, x, y, &text),
                None => Err(anyhow::anyhow!("toca primero el punto del PDF")),
            };
            match res {
                Ok(out) => {
                    notify(&ui, &st, format!("Nota guardada en {} (reabre ese archivo para verla)", out.display()));
                    refresh_notes(&ui, &st);
                }
                Err(e) => notify(&ui, &st, format!("Error al anotar: {e:#}")),
            }
            ui.set_free_note_on(false);
        });
    }
    {
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_free_note_cancel(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_free_note_on(false);
            }
            st.borrow_mut().free_pt = None;
        });
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
        let ui_weak = ui.as_weak();
        let st = st.clone();
        ui.on_save_note(move |i| {
            let Some(ui) = ui_weak.upgrade() else { return };
            let text = ui.get_note_text().to_string();
            let group = st.borrow().view_group.get(i as usize).cloned().flatten();
            let res = if let Some(w) = group {
                let idxs: Vec<usize> = st.borrow().all_errs.iter().enumerate()
                    .filter(|(_, e)| e.word == w).map(|(gi, _)| gi).collect();
                annotate_many(&st.borrow(), &idxs, &text)
            } else {
                let gi = match st.borrow().view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                annotate(&st.borrow(), gi, &text)
            };
            match res {
                Ok(out) => {
                    notify(&ui, &st, format!("Nota guardada en {} (reabre ese archivo para verla)", out.display()));
                    refresh_notes(&ui, &st);
                }
                Err(e) => notify(&ui, &st, format!("Error al anotar: {e:#}")),
            }
            ui.set_selected_note(-1);
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
                annotate_many(&st.borrow(), &idxs, &text)
            } else {
                let gi = match st.borrow().view.get(i as usize) {
                    Some(g) => *g,
                    None => return,
                };
                annotate(&st.borrow(), gi, &text)
            };
            match res {
                Ok(out) => {
                    notify(&ui, &st, format!("Observación guardada en {} (reabre ese archivo para verla)", out.display()));
                    refresh_notes(&ui, &st);
                }
                Err(e) => notify(&ui, &st, format!("Error al anotar: {e:#}")),
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

    // sidecar LanguageTool: SOLO el hijo propio. Un :8081 ajeno recibiría el
    // texto íntegro del PDF, así que ocupado = sin revisión (fail-closed).
    // ponytail: el Child viaja por el canal al hilo UI; los hilos de página solo hacen HTTP
    {
        let tx = st.borrow().tx.clone();
        std::thread::spawn(move || {
            // sonda de propiedad: si no podemos atar el puerto, alguien lo ocupa
            if std::net::TcpListener::bind(("127.0.0.1", LT_PORT)).is_err() {
                let _ = tx.send(SpellEvent::LtFail("puerto 8081 ocupado por un proceso ajeno: sin revisión (cierra el otro programa y reabre)".into()));
                return;
            }
            let Some(dir) = lt_dir() else {
                let _ = tx.send(SpellEvent::LtFail("no se encontró assets/lt (¿falló la descarga en build?)".into()));
                return;
            };
            let jar = dir.join("languagetool-server.jar");
            let mut cmd = java_cmd();
            #[cfg(windows)]
            {
                // sin ventana de consola del hijo Java en Windows
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
            }
            let child = cmd
                .args(["-Xms256m", "-Xmx1g", "-Dfile.encoding=UTF-8", "-cp"])
                .arg(&jar)
                .arg("org.languagetool.server.HTTPServer")
                .args(["--port", &LT_PORT.to_string()])
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
            } else {
                let _ = child.kill();
                let _ = tx.send(SpellEvent::LtFail("el servidor LT no respondió a tiempo".into()));
            }
        });
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
    /// Fixture para anotar: /tmp se limpia solo; se restaura desde input/.
    fn fixture_pdf() -> PathBuf {
        let tmp = PathBuf::from("/tmp/doc3.pdf");
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
    #[test]
    fn notes_roundtrip() {
        let src = fixture_pdf();
        let (tx, _rx) = channel::<SpellMsg>();
        let st = State {
            pdfium: load_pdfium(), path: Some(src.clone()), page: 0, total: 3,
            words: vec![], page_w: 0.0, page_h: 0.0, img_w: 0, img_h: 0,
            all_errs: vec![Misspelling { word: "herror".into(), sug: vec!["error".into()], page: 0, x0: 72.0, y0: 700.0, x1: 130.0, y1: 712.0, dismissed: false, kind: "Ortografía".into(), sev: "intermedia".into() }],
            doc_notes: vec![],
            scan_id: 0, scan_queue: vec![], scan_pos: 0, popup_until: None, zoom: 0.5, free_pt: None, tx, last_lang: 0, flash: None, view: vec![], view_group: vec![], expanded: HashSet::new(), lt_ready: true, lt_dead: false, pending: 0, jobs_total: 0, sec_bytes: 0, boot_start: std::time::Instant::now(),
        syn_received: false, lt_child: None,
        };
        annotate(&st, 0, "nota redonda").unwrap();
        let out = src.with_file_name("doc3_anotado.pdf");
        let notes = read_notes(&st.pdfium, &out);
        assert!(notes.iter().any(|n| n.text.contains("nota redonda") && n.page == 0));
    }
    #[test]
    fn annotate_creates_sibling_file() {
        // ponytail: usa /tmp/doc3.pdf generado en dev; si falta, el test falla explícito
        let src = fixture_pdf();
        let (tx, _rx) = channel::<SpellMsg>();
        let st = State {
            pdfium: load_pdfium(), path: Some(src), page: 0, total: 3,
            words: vec![], page_w: 0.0, page_h: 0.0, img_w: 0, img_h: 0,
            all_errs: vec![Misspelling { word: "herror".into(), sug: vec!["error".into()], page: 0, x0: 72.0, y0: 700.0, x1: 130.0, y1: 712.0, dismissed: false, kind: "Ortografía".into(), sev: "intermedio".into() }],
            doc_notes: vec![],
            scan_id: 0, scan_queue: vec![], scan_pos: 0, popup_until: None, zoom: 1.0, free_pt: None, tx, last_lang: 0, flash: None, view: vec![], view_group: vec![], expanded: HashSet::new(), lt_ready: true, lt_dead: false, pending: 0, jobs_total: 0, sec_bytes: 0, boot_start: std::time::Instant::now(),
        syn_received: false, lt_child: None,
        };
        let out = annotate(&st, 0, "nota de prueba").unwrap();
        assert_eq!(out.file_name().unwrap(), "doc3_anotado.pdf");
        assert!(out.metadata().unwrap().len() > 500);
    }
}
