// SPDX-License-Identifier: GPL-3.0-only
//
// LexPDF — compila el UI (Slint) y prepara assets al construir.
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

use std::io::Write;

// icono del exe en Windows: la esquina de la ventana y la barra de tareas
// lo sacan del recurso incrustado (Slint no tiene set_icon en 1.18)
#[cfg(windows)]
fn embed_icon() {
    winresource::WindowsResource::new()
        .set_icon("installer/windows/lexpdf.ico")
        .compile()
        .expect("incrustar lexpdf.ico");
}

#[cfg(not(windows))]
fn embed_icon() {}

fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    embed_icon();

    // ponytail: LT (~240MB zip) se descarga una vez a assets/lt/ (gitignoreado);
    // en runtime va empaquetado junto al exe. Requiere red solo la primera vez.
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lt_jar = manifest.join("assets/lt/languagetool-server.jar");
    // El zip estable cambia de versión; si ya hay jar, no se toca nada.
    if !lt_jar.is_file() {
        let zip = manifest.join("assets/lt.zip");
        if !zip.is_file() {
            eprintln!("lexpdf: descargando LanguageTool (~240MB, solo primera vez)…");
            let mut resp = ureq::get("https://languagetool.org/download/LanguageTool-stable.zip")
                .call()
                .expect("descarga de LanguageTool fallida (¿sin red?)");
            let mut out = std::fs::File::create(&zip).expect("no se pudo crear assets/lt.zip");
            let mut reader = resp.into_body().into_reader();
            std::io::copy(&mut reader, &mut out).expect("escritura de assets/lt.zip");
        }
        eprintln!("lexpdf: descomprimiendo LanguageTool…");
        unzip_lt(&zip, &manifest.join("assets"));
    }

    // ponytail: copia plana junto al exe, sin crate de assets; build de instalador si hace falta
    let lib = if cfg!(target_os = "windows") {
        "pdfium.dll"
    } else {
        "libpdfium.so"
    };
    let src = manifest.join("assets/pdfium").join(lib);
    println!("cargo:rerun-if-changed=assets/pdfium/{lib}");

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    if let Some(profile_dir) = out.ancestors().nth(3) {
        let _ = std::fs::copy(&src, profile_dir.join(lib));
    }
}

/// Descomprime el zip quitando el directorio raíz (LanguageTool-X.Y/ -> assets/lt/).
fn unzip_lt(zip: &std::path::Path, assets: &std::path::Path) {
    let f = std::fs::File::open(zip).expect("assets/lt.zip");
    let mut ar = zip::ZipArchive::new(f).expect("zip inválido");
    let dest = assets.join("lt");
    for i in 0..ar.len() {
        let mut e = ar.by_index(i).expect("entrada zip");
        let rel = e.enclosed_name().expect("ruta zip");
        let mut parts = rel.components();
        parts.next(); // quita LanguageTool-X.Y/
        let out = dest.join(parts.as_path());
        if e.is_dir() {
            let _ = std::fs::create_dir_all(&out);
        } else {
            if let Some(p) = out.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let mut o = std::fs::File::create(&out).expect("extraer LT");
            std::io::copy(&mut e, &mut o).expect("extraer LT");
        }
    }
    let log = dest.join("VERSION.lt");
    let _ = std::fs::File::create(log).and_then(|mut f| f.write_all(b"LanguageTool-stable (ver languagetool-server.jar)"));
}
