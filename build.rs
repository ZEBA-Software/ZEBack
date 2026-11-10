//! Erzeugt aus dem Logo (src/logo.rs) die Icon-Datei `assets/Packup.ico`
//! und bettet sie zusammen mit den Versionsangaben in die EXE ein.

use std::fs;
use std::path::Path;

#[path = "src/logo.rs"]
mod logo;

const ICON_PATH: &str = "assets/Packup.ico";
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");

    let ico = build_ico();
    let path = Path::new(ICON_PATH);
    // Nur schreiben, wenn sich etwas geändert hat (sonst würde jedes Mal neu gelinkt).
    if fs::read(path).ok().as_deref() != Some(ico.as_slice()) {
        fs::create_dir_all("assets").expect("Ordner assets anlegen");
        fs::write(path, &ico).expect("Icon-Datei schreiben");
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(ICON_PATH);
        resource.set("ProductName", "Packup");
        resource.set("FileDescription", "Packup");
        resource.set("InternalName", "Packup");
        resource.set("OriginalFilename", "Packup.exe");
        resource.set("CompanyName", "ZEBA Software UG");
        resource.set("LegalCopyright", "Copyright (c) 2026 ZEBA Software UG - zeba.xyz");
        if let Err(e) = resource.compile() {
            // Ohne Ressourcen-Compiler funktioniert das Programm trotzdem, nur ohne EXE-Icon.
            println!("cargo:warning=EXE-Icon konnte nicht eingebettet werden: {e}");
        }
    }
}

/// ICO-Datei mit mehreren Größen (32-Bit-Bitmaps mit Alphakanal).
fn build_ico() -> Vec<u8> {
    let images: Vec<(u32, Vec<u8>)> = SIZES.iter().map(|&s| (s, dib(s))).collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserviert
    out.extend_from_slice(&1u16.to_le_bytes()); // Typ: Icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes()); // Farbebenen
        out.extend_from_slice(&32u16.to_le_bytes()); // Bit pro Pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// Ein Bild im Icon: BITMAPINFOHEADER, Pixel (BGRA, von unten nach oben) und leere Maske.
fn dib(size: u32) -> Vec<u8> {
    let rgba = logo::render_rgba(size);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::with_capacity((40 + size * size * 4 + mask_row * size) as usize);

    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // Bild + Maske
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // unkomprimiert
    out.extend_from_slice(&(size * size * 4 + mask_row * size).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]);

    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            out.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    out.resize(out.len() + (mask_row * size) as usize, 0);
    out
}
