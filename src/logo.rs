//! Das ZEBack-Logo, rein rechnerisch gezeichnet: eine Person ("Profil") in einem
//! Kreispfeil ("Sicherung") auf orangem Quadrat.
//!
//! Diese Datei wird auch von `build.rs` eingebunden (für das EXE-Icon) und darf deshalb
//! keine anderen Module oder Crates benutzen.

/// Zeichnet das Logo als RGBA-Bild (nicht vormultipliziert), `size` × `size` Pixel.
pub fn render_rgba(size: u32) -> Vec<u8> {
    // Kleine Größen brauchen mehr Abtastpunkte pro Pixel, damit die Kanten sauber bleiben.
    let samples: u32 = if size <= 48 { 8 } else { 4 };
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut r, mut g, mut b, mut covered) = (0.0f32, 0.0f32, 0.0f32, 0u32);
            for sy in 0..samples {
                for sx in 0..samples {
                    // Koordinaten im Bereich -1 … 1, y nach unten.
                    let x = ((px as f32 + (sx as f32 + 0.5) / samples as f32) / size as f32) * 2.0 - 1.0;
                    let y = ((py as f32 + (sy as f32 + 0.5) / samples as f32) / size as f32) * 2.0 - 1.0;
                    if let Some(color) = sample(x, y) {
                        r += color[0];
                        g += color[1];
                        b += color[2];
                        covered += 1;
                    }
                }
            }
            if covered == 0 {
                out.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let n = covered as f32;
                let alpha = n / (samples * samples) as f32;
                out.extend_from_slice(&[
                    (r / n).round() as u8,
                    (g / n).round() as u8,
                    (b / n).round() as u8,
                    (alpha * 255.0).round() as u8,
                ]);
            }
        }
    }
    out
}

/// Farbe an der Stelle (x, y) oder `None` außerhalb des Logos.
fn sample(x: f32, y: f32) -> Option<[f32; 3]> {
    // Hintergrund: Quadrat mit leicht abgerundeten Ecken, einfarbig orange.
    const CORNER: f32 = 0.30;
    let (qx, qy) = (x.abs() - 1.0 + CORNER, y.abs() - 1.0 + CORNER);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - CORNER;
    if outside > 0.0 {
        return None;
    }
    if glyph(x, y) {
        // Zeichen in fast Schwarz
        return Some([0x12 as f32, 0x0B as f32, 0x08 as f32]);
    }
    Some([0xF2 as f32, 0x6A as f32, 0x45 as f32])
}

/// Das dunkle Zeichen: Kreispfeil mit Person in der Mitte.
fn glyph(x: f32, y: f32) -> bool {
    const RADIUS: f32 = 0.62;
    const HALF_WIDTH: f32 = 0.075;
    let dist = (x * x + y * y).sqrt();

    // Ring mit Lücke oben rechts; der Bogen läuft im Uhrzeigersinn von -30° bis 260°.
    let angle = y.atan2(x).to_degrees();
    let along = (angle + 30.0).rem_euclid(360.0);
    if (dist - RADIUS).abs() <= HALF_WIDTH && along <= 290.0 {
        return true;
    }
    // Abgerundeter Anfang des Bogens.
    let start = (-30.0f32).to_radians();
    if hypot(x - RADIUS * start.cos(), y - RADIUS * start.sin()) <= HALF_WIDTH {
        return true;
    }
    // Pfeilspitze am Ende des Bogens (bei 260°), sie zeigt in die Lücke.
    let end = 260.0f32.to_radians();
    let (ex, ey) = (RADIUS * end.cos(), RADIUS * end.sin());
    let (tx, ty) = (-end.sin(), end.cos());
    let (rx, ry) = (end.cos(), end.sin());
    let tip = (ex + tx * 0.34, ey + ty * 0.34);
    let outer = (ex + rx * 0.23, ey + ry * 0.23);
    let inner = (ex - rx * 0.23, ey - ry * 0.23);
    if in_triangle((x, y), tip, outer, inner) {
        return true;
    }

    // Person: Kopf …
    if hypot(x, y + 0.15) <= 0.17 {
        return true;
    }
    // … und Schultern (obere Hälfte einer Ellipse).
    let (bx, by) = (x / 0.31, (y - 0.38) / 0.27);
    y <= 0.38 && bx * bx + by * by <= 1.0
}

fn hypot(x: f32, y: f32) -> f32 {
    (x * x + y * y).sqrt()
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let cross = |u: (f32, f32), v: (f32, f32), w: (f32, f32)| (v.0 - u.0) * (w.1 - u.1) - (v.1 - u.1) * (w.0 - u.0);
    let (d1, d2, d3) = (cross(a, b, p), cross(b, c, p), cross(c, a, p));
    let negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(negative && positive)
}
