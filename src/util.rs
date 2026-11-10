use std::path::{Path, PathBuf};
use std::time::Duration;

/// Größe gut lesbar, z. B. "1,4 GB".
pub fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["Bytes", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", t!(UNITS[0], "bytes"))
    } else if crate::i18n::english() {
        // Englisch: Dezimalpunkt statt Komma.
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit]).replace('.', ",")
    }
}

/// Dauer als "m:ss min" bzw. "h:mm:ss h".
pub fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02} h")
    } else if m > 0 {
        format!("{m}:{s:02} min")
    } else {
        format!("{s} s")
    }
}

/// Löscht die Datei beim Verlassen des Gültigkeitsbereichs, solange `keep()` nicht aufgerufen wurde.
/// Damit bleiben nach Fehlern oder Abbruch keine halben Dateien liegen.
pub struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), armed: true }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn keep(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(fmt_bytes(512), "512 Bytes");
        assert_eq!(fmt_bytes(1536), "1,5 KB");
        assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3,0 GB");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42 s");
        assert_eq!(fmt_duration(Duration::from_secs(125)), "2:05 min");
        assert_eq!(fmt_duration(Duration::from_secs(3725)), "1:02:05 h");
    }
}
