//! Freier Speicherplatz und Dateisystem eines Ziellaufwerks.
//! Die Abfrage läuft im Hintergrund, weil sie bei Netzlaufwerken dauern kann.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskInfo {
    /// Für den Benutzer verfügbarer Platz in Bytes
    pub free: u64,
    pub total: u64,
    /// z. B. "NTFS", "exFAT", "FAT32"; leer, wenn nicht ermittelbar
    pub file_system: String,
}

impl DiskInfo {
    /// FAT32 kann keine Dateien über 4 GB speichern.
    pub fn is_fat32(&self) -> bool {
        self.file_system.eq_ignore_ascii_case("FAT32")
    }
}

/// Größte Datei, die auf FAT32 passt.
pub const FAT32_MAX_FILE: u64 = 0xFFFF_FFFF;

#[cfg(windows)]
mod ffi {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetDiskFreeSpaceExW(
            directory: *const u16,
            free_to_caller: *mut u64,
            total: *mut u64,
            total_free: *mut u64,
        ) -> i32;
        pub fn GetVolumePathNameW(file_name: *const u16, volume_path: *mut u16, length: u32) -> i32;
        pub fn GetLogicalDrives() -> u32;
        pub fn GetDriveTypeW(root: *const u16) -> u32;
        pub fn GetVolumeInformationW(
            root: *const u16,
            volume_name: *mut u16,
            volume_name_length: u32,
            serial: *mut u32,
            max_component_length: *mut u32,
            flags: *mut u32,
            file_system_name: *mut u16,
            file_system_name_length: u32,
        ) -> i32;
    }
}

#[cfg(windows)]
fn from_wide(buffer: &[u16]) -> String {
    let length = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..length])
}

/// Bezeichnung und Dateisystem eines Volumes (Pfad der Volume-Wurzel, nullterminiert).
#[cfg(windows)]
fn volume_names(root: &[u16]) -> Option<(String, String)> {
    use std::ptr::null_mut;
    let mut label = [0u16; 262];
    let mut file_system = [0u16; 34];
    // SAFETY: `root` ist nullterminiert; beide Puffer sind mit ihrer tatsächlichen Länge angegeben.
    let ok = unsafe {
        ffi::GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            null_mut(),
            null_mut(),
            null_mut(),
            file_system.as_mut_ptr(),
            file_system.len() as u32,
        ) != 0
    };
    ok.then(|| (from_wide(&label), from_wide(&file_system)))
}

/// Fragt Windows nach Platz und Dateisystem. Blockiert, bis die Antwort da ist.
#[cfg(windows)]
pub fn info(path: &Path) -> Option<DiskInfo> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `wide` ist nullterminiert; die Zeiger zeigen auf gültige lokale Variablen.
    if unsafe { ffi::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, null_mut()) } == 0 {
        return None;
    }

    let mut volume = [0u16; 262];
    // SAFETY: Der Puffer ist mit seiner tatsächlichen Länge angegeben und wird nullterminiert gefüllt.
    let found = unsafe { ffi::GetVolumePathNameW(wide.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) != 0 };
    let file_system = found.then(|| volume_names(&volume)).flatten().map(|(_, fs)| fs).unwrap_or_default();
    Some(DiskInfo { free, total, file_system })
}

/// Ein Laufwerk dieses PCs (nur fest eingebaute und Wechseldatenträger).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drive {
    /// Wurzel mit Laufwerksbuchstabe, z. B. `C:\`
    pub root: String,
    pub label: String,
    pub file_system: String,
    /// USB-Stick oder anderer Wechseldatenträger
    pub removable: bool,
    /// Laufwerk, auf dem Windows liegt
    pub system: bool,
    pub total: u64,
    pub free: u64,
}

impl Drive {
    /// Laufwerksbuchstabe mit Doppelpunkt, z. B. `C:`
    pub fn letter(&self) -> &str {
        self.root.trim_end_matches('\\')
    }

    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Anzeigename wie im Explorer, z. B. `Daten (D:)`
    pub fn title(&self) -> String {
        let label = if !self.label.is_empty() {
            self.label.as_str()
        } else if self.removable {
            t!("Wechseldatenträger", "Removable disk")
        } else {
            t!("Lokaler Datenträger", "Local disk")
        };
        format!("{label} ({})", self.letter())
    }
}

/// Listet die Laufwerke auf. Netzlaufwerke und CD-Laufwerke bleiben außen vor
/// (ihre Abfrage kann lange blockieren).
#[cfg(windows)]
pub fn list_drives() -> Vec<Drive> {
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    // SAFETY: Aufruf ohne Parameter.
    let mask = unsafe { ffi::GetLogicalDrives() };
    (0..26u8)
        .filter(|bit| mask & (1 << bit) != 0)
        .filter_map(|bit| {
            let root = format!("{}:\\", (b'A' + bit) as char);
            let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `wide` ist nullterminiert.
            let kind = unsafe { ffi::GetDriveTypeW(wide.as_ptr()) };
            if kind != DRIVE_REMOVABLE && kind != DRIVE_FIXED {
                return None;
            }
            // Leere Kartenleser liefern keine Angaben und werden ausgelassen.
            let disk = info(Path::new(&root))?;
            let (label, file_system) = volume_names(&wide).unwrap_or_default();
            let system = root.trim_end_matches('\\').eq_ignore_ascii_case(&system_drive);
            Some(Drive {
                root,
                label,
                file_system,
                removable: kind == DRIVE_REMOVABLE,
                system,
                total: disk.total,
                free: disk.free,
            })
        })
        .collect()
}

#[cfg(not(windows))]
pub fn list_drives() -> Vec<Drive> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn info(_path: &Path) -> Option<DiskInfo> {
    None
}

/// Ergebnis einer Abfrage über [`DiskProbe`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    /// Antwort steht noch aus
    Pending,
    /// Ordner nicht erreichbar
    Unavailable,
    Ready(DiskInfo),
}

/// Fragt den Speicherplatz im Hintergrund ab und merkt sich die letzte Antwort.
pub struct DiskProbe {
    tx: mpsc::Sender<(PathBuf, Option<DiskInfo>)>,
    rx: mpsc::Receiver<(PathBuf, Option<DiskInfo>)>,
    /// Zuletzt beantwortete Abfrage mit Zeitpunkt
    answer: Option<(PathBuf, Option<DiskInfo>, Instant)>,
    /// Pfad, der gerade angefragt wird
    asking: Option<PathBuf>,
    /// Zuletzt gesehener Pfad und seit wann er unverändert ist (beim Tippen nicht jede Zwischenstufe abfragen)
    seen: Option<(PathBuf, Instant)>,
    wake: std::sync::Arc<dyn Fn() + Send + Sync>,
}

impl DiskProbe {
    const SETTLE: Duration = Duration::from_millis(450);
    const MAX_AGE: Duration = Duration::from_secs(5);

    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, answer: None, asking: None, seen: None, wake: std::sync::Arc::new(wake) }
    }

    /// Liefert den Stand für `path`; stößt bei Bedarf eine neue Abfrage an.
    pub fn lookup(&mut self, path: &Path) -> Probe {
        for (answered, info) in self.rx.try_iter() {
            if self.asking.as_deref() == Some(answered.as_path()) {
                self.asking = None;
            }
            self.answer = Some((answered, info, Instant::now()));
        }

        let settled = match &self.seen {
            Some((seen, since)) if seen == path => since.elapsed() >= Self::SETTLE,
            _ => {
                self.seen = Some((path.to_path_buf(), Instant::now()));
                false
            }
        };
        let cached = self.answer.as_ref().filter(|(answered, _, _)| answered == path);
        let fresh = cached.is_some_and(|(_, _, at)| at.elapsed() < Self::MAX_AGE);
        let result = match cached {
            Some((_, Some(info), _)) => Probe::Ready(info.clone()),
            Some((_, None, _)) => Probe::Unavailable,
            None => Probe::Pending,
        };

        if settled && !fresh && self.asking.is_none() {
            self.asking = Some(path.to_path_buf());
            let (tx, wake, path) = (self.tx.clone(), self.wake.clone(), path.to_path_buf());
            std::thread::spawn(move || {
                let info = info(&path);
                let _ = tx.send((path, info));
                wake();
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn reads_free_space_and_file_system() {
        let disk = info(&std::env::temp_dir()).expect("Temp-Ordner sollte abfragbar sein");
        assert!(disk.total > 0 && disk.free <= disk.total);
        assert!(!disk.file_system.is_empty());
        assert!(info(Path::new("Q:\\gibt\\es\\nicht")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn lists_the_system_drive() {
        let drives = list_drives();
        let system: Vec<&Drive> = drives.iter().filter(|d| d.system).collect();
        assert_eq!(system.len(), 1, "{drives:?}");
        let drive = system[0];
        assert!(drive.total > 0 && drive.used() > 0 && !drive.file_system.is_empty());
        assert_eq!(drive.letter().len(), 2);
        assert!(drive.title().ends_with(&format!("({})", drive.letter())));
    }

    #[test]
    fn probe_answers_after_the_path_settles() {
        let mut probe = DiskProbe::new(|| {});
        let path = std::env::temp_dir();
        assert_eq!(probe.lookup(&path), Probe::Pending);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut result = Probe::Pending;
        while result == Probe::Pending && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            result = probe.lookup(&path);
        }
        if cfg!(windows) {
            assert!(matches!(result, Probe::Ready(_)), "{result:?}");
        }
    }
}
