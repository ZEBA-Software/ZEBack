//! Ordnergrößen im Hintergrund berechnen, damit die Oberfläche flüssig bleibt.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use crate::backup::{self, Source};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeInfo {
    pub files: u64,
    pub bytes: u64,
    /// Davon in Dateien, die nur online vorliegen (OneDrive)
    pub online_bytes: u64,
}

impl SizeInfo {
    /// Datenmenge, die tatsächlich gesichert würde.
    pub fn bytes_to_back_up(&self, skip_online: bool) -> u64 {
        if skip_online { self.bytes - self.online_bytes } else { self.bytes }
    }
}

/// Ordner plus Kennung seiner Pfad-Ausschlüsse: derselbe Ordner kann mit unterschiedlichen
/// Ausschlüssen gemessen werden (z. B. ein Laufwerk mit und ohne Windows-Ordner).
type Key = (PathBuf, u64);

fn key(path: &Path, exclude_paths: &[PathBuf]) -> Key {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    exclude_paths.hash(&mut hasher);
    (path.to_path_buf(), if exclude_paths.is_empty() { 0 } else { hasher.finish() })
}

struct Request {
    key: Key,
    exclude: Vec<String>,
    exclude_paths: Vec<PathBuf>,
    generation: u64,
}

/// Misst Ordner nacheinander in einem eigenen Thread und merkt sich die Ergebnisse.
pub struct SizeScanner {
    /// `None` = wird noch berechnet
    known: HashMap<Key, Option<SizeInfo>>,
    tx: mpsc::Sender<Request>,
    rx: mpsc::Receiver<(Key, SizeInfo)>,
    /// Wird erhöht, um laufende und wartende Messungen abzubrechen.
    generation: Arc<AtomicU64>,
}

impl SizeScanner {
    /// `wake` wird aufgerufen, sobald ein Ergebnis vorliegt.
    pub fn new(wake: impl Fn() + Send + 'static) -> Self {
        let (tx, requests) = mpsc::channel::<Request>();
        let (results, rx) = mpsc::channel();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        std::thread::spawn(move || {
            // Endet von selbst, wenn die Oberfläche (der Sender) nicht mehr existiert.
            for request in requests {
                if let Some(info) = measure(&request, &current) {
                    if results.send((request.key, info)).is_err() {
                        return;
                    }
                    wake();
                }
            }
        });
        Self { known: HashMap::new(), tx, rx, generation }
    }

    /// Fordert die Größe an, falls sie noch nicht bekannt oder in Arbeit ist.
    pub fn request(&mut self, path: &Path, exclude: &[&str]) {
        self.request_with(path, exclude, &[]);
    }

    /// Wie [`Self::request`], zusätzlich mit ausgeschlossenen Pfaden.
    pub fn request_with(&mut self, path: &Path, exclude: &[&str], exclude_paths: &[PathBuf]) {
        let key = key(path, exclude_paths);
        if self.known.contains_key(&key) {
            return;
        }
        self.known.insert(key.clone(), None);
        let _ = self.tx.send(Request {
            key,
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            exclude_paths: exclude_paths.to_vec(),
            generation: self.generation.load(Ordering::Relaxed),
        });
    }

    /// Bricht alle noch nicht fertigen Messungen ab (z. B. nach einem Profilwechsel).
    pub fn cancel_pending(&mut self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
        self.known.retain(|_, info| info.is_some());
    }

    /// Fertige Ergebnisse übernehmen; einmal pro Bild aufrufen.
    pub fn poll(&mut self) {
        for (key, info) in self.rx.try_iter() {
            self.known.insert(key, Some(info));
        }
    }

    /// `None` = nie angefordert, `Some(None)` = wird berechnet, `Some(Some(_))` = fertig.
    pub fn get(&self, path: &Path) -> Option<Option<SizeInfo>> {
        self.get_with(path, &[])
    }

    /// Wie [`Self::get`] für eine Messung mit ausgeschlossenen Pfaden.
    pub fn get_with(&self, path: &Path, exclude_paths: &[PathBuf]) -> Option<Option<SizeInfo>> {
        self.known.get(&key(path, exclude_paths)).copied()
    }
}

fn measure(request: &Request, generation: &AtomicU64) -> Option<SizeInfo> {
    let source = Source {
        path: request.key.0.clone(),
        kind: None,
        name: None,
        exclude: request.exclude.clone(),
        exclude_paths: request.exclude_paths.clone(),
    };
    let mut info = SizeInfo::default();
    for entry in source.walk() {
        if generation.load(Ordering::Relaxed) != request.generation {
            return None;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        info.files += 1;
        info.bytes += meta.len();
        if backup::is_online_only(&meta) {
            info.online_bytes += meta.len();
        }
    }
    Some(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, Instant};

    #[test]
    fn measures_folder_and_respects_excludes() {
        let base = std::env::temp_dir().join(format!("packup-test-sizes-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("Cache")).unwrap();
        fs::create_dir_all(base.join("Unter")).unwrap();
        fs::write(base.join("a.txt"), vec![1u8; 1000]).unwrap();
        fs::write(base.join("Unter").join("b.txt"), vec![1u8; 2500]).unwrap();
        fs::write(base.join("Cache").join("gross.bin"), vec![1u8; 90_000]).unwrap();

        let mut scanner = SizeScanner::new(|| {});
        assert_eq!(scanner.get(&base), None);
        scanner.request(&base, &["cache"]);
        assert_eq!(scanner.get(&base), Some(None));

        let deadline = Instant::now() + Duration::from_secs(10);
        while scanner.get(&base) == Some(None) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            scanner.poll();
        }
        let info = scanner.get(&base).flatten().expect("Größe wurde nicht berechnet");
        assert_eq!((info.files, info.bytes), (2, 3500));
        assert_eq!(info.bytes_to_back_up(true), 3500);
        let _ = fs::remove_dir_all(base);
    }
}
