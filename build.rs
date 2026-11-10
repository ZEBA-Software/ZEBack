//! Erzeugt aus dem Logo (src/logo.rs) die Icon-Datei `assets/ZEBack.ico`
//! und bettet sie zusammen mit den Versionsangaben in die EXE ein.

use std::fs;
use std::path::Path;

#[path = "src/logo.rs"]
mod logo;

const ICON_PATH: &str = "assets/ZEBack.ico";
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/logo.rs");

    write_if_changed(ICON_PATH, &build_ico());
    // Bilder für den Installer: Kopfzeile und linker Streifen des Assistenten.
    write_if_changed(BANNER_PATH, &installer_banner());
    write_if_changed(DIALOG_PATH, &installer_dialog());

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon(ICON_PATH);
        resource.set("ProductName", "ZEBack");
        resource.set("FileDescription", "ZEBack");
        resource.set("InternalName", "ZEBack");
        resource.set("OriginalFilename", "ZEBack.exe");
        resource.set("CompanyName", "ZEBA Software UG");
        resource.set("LegalCopyright", "Copyright (c) 2026 ZEBA Software UG - zeba-software.de");
        if let Err(e) = resource.compile() {
            // Ohne Ressourcen-Compiler funktioniert das Programm trotzdem, nur ohne EXE-Icon.
            println!("cargo:warning=EXE-Icon konnte nicht eingebettet werden: {e}");
        }
    }
}

/// Schreibt die Datei nur, wenn sich etwas geändert hat (sonst würde jedes Mal neu gelinkt).
fn write_if_changed(path: &str, data: &[u8]) {
    let path = Path::new(path);
    if fs::read(path).ok().as_deref() != Some(data) {
        fs::create_dir_all("assets").expect("Ordner assets anlegen");
        fs::write(path, data).expect("Datei in assets schreiben");
    }
}

const BANNER_PATH: &str = "assets/installer-banner.bmp";
const DIALOG_PATH: &str = "assets/installer-dialog.bmp";
/// Breite der Installer-Bilder, wie Windows Installer sie erwartet.
const INSTALLER_WIDTH: u32 = 493;

const WHITE: [u8; 3] = [0xFF, 0xFF, 0xFF];
const DARK: [u8; 3] = [0x0D, 0x0D, 0x0F];
const GRID: [u8; 3] = [0x1B, 0x1B, 0x1F];
const ACCENT: [u8; 3] = [0xF2, 0x6A, 0x45];

/// Eine Bildfläche aus RGB-Pixeln, Zeilen von oben nach unten.
struct Canvas {
    width: u32,
    height: u32,
    pixels: Vec<[u8; 3]>,
}

impl Canvas {
    fn new(width: u32, height: u32, color: [u8; 3]) -> Self {
        Self { width, height, pixels: vec![color; (width * height) as usize] }
    }

    fn set(&mut self, x: u32, y: u32, color: [u8; 3]) {
        if x < self.width && y < self.height {
            self.pixels[(y * self.width + x) as usize] = color;
        }
    }

    /// Zeichnet das Logo in der Größe `size` mit der linken oberen Ecke bei (`left`, `top`).
    fn logo(&mut self, size: u32, left: u32, top: u32) {
        let rgba = logo::render_rgba(size);
        for y in 0..size {
            for x in 0..size {
                let i = ((y * size + x) * 4) as usize;
                let alpha = u32::from(rgba[i + 3]);
                let (px, py) = (left + x, top + y);
                if px >= self.width || py >= self.height {
                    continue;
                }
                let under = self.pixels[(py * self.width + px) as usize];
                let mut mixed = [0u8; 3];
                for channel in 0..3 {
                    mixed[channel] = ((u32::from(rgba[i + channel]) * alpha + u32::from(under[channel]) * (255 - alpha)) / 255) as u8;
                }
                self.set(px, py, mixed);
            }
        }
    }

    /// Als 24-Bit-BMP (Zeilen von unten nach oben, auf vier Bytes aufgefüllt).
    fn bmp(&self) -> Vec<u8> {
        let row = (self.width * 3).div_ceil(4) * 4;
        let data = row * self.height;
        let mut out = Vec::with_capacity((54 + data) as usize);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + data).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(self.width as i32).to_le_bytes());
        out.extend_from_slice(&(self.height as i32).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // unkomprimiert
        out.extend_from_slice(&data.to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        for y in (0..self.height).rev() {
            let start = out.len();
            for x in 0..self.width {
                let [r, g, b] = self.pixels[(y * self.width + x) as usize];
                out.extend_from_slice(&[b, g, r]);
            }
            out.resize(start + row as usize, 0);
        }
        out
    }
}

/// Kopfzeile der Installer-Seiten: weiß (der Text darauf ist schwarz), rechts das Logo.
fn installer_banner() -> Vec<u8> {
    const HEIGHT: u32 = 58;
    const LOGO: u32 = 40;
    let mut canvas = Canvas::new(INSTALLER_WIDTH, HEIGHT, WHITE);
    canvas.logo(LOGO, INSTALLER_WIDTH - LOGO - 12, (HEIGHT - LOGO) / 2);
    canvas.bmp()
}

/// Erste und letzte Seite des Installers: links ein dunkler Streifen im Aussehen des
/// Programms (Raster, Akzentlinie, Logo), rechts weiß für den Text.
fn installer_dialog() -> Vec<u8> {
    const HEIGHT: u32 = 312;
    const STRIP: u32 = 164;
    const LOGO: u32 = 88;
    let mut canvas = Canvas::new(INSTALLER_WIDTH, HEIGHT, WHITE);
    for y in 0..HEIGHT {
        for x in 0..STRIP {
            let color = if x >= STRIP - 3 {
                ACCENT
            } else if x % 22 == 10 || y % 22 == 10 {
                GRID
            } else {
                DARK
            };
            canvas.set(x, y, color);
        }
    }
    canvas.logo(LOGO, (STRIP - 3 - LOGO) / 2, 64);
    // Eckmarken wie an den Karten im Programm
    for (corner_x, corner_y, dx, dy) in [(14i32, 14i32, 1i32, 1i32), (146, 14, -1, 1), (14, 297, 1, -1), (146, 297, -1, -1)] {
        for step in 0..12 {
            canvas.set((corner_x + dx * step) as u32, corner_y as u32, ACCENT);
            canvas.set(corner_x as u32, (corner_y + dy * step) as u32, ACCENT);
        }
    }
    canvas.bmp()
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
