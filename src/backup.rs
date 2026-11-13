//! Backup: Ordner in ein ZIP-Archiv packen und optional zu Nextcloud hochladen.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Local;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::config::Compression;
use crate::job::{Counter, Reporter};
use crate::remote::{Remote, RemoteSettings};
use crate::util::{TempFile, fmt_bytes};

/// Name der Inhaltsbeschreibung im Archiv. Sie wird für die Wiederherstellung gebraucht.
pub const MANIFEST_NAME: &str = "backup-manifest.json";

/// Ab dieser Dateigröße wird ein ZIP64-Eintrag geschrieben (Grenze des klassischen ZIP: 4 GiB).
const LARGE_FILE_THRESHOLD: u64 = 0xF000_0000;
const COPY_BUFFER: usize = 1 << 20;

/// Dateitypen, die bereits komprimiert sind – erneutes Komprimieren kostet nur Zeit.
const ALREADY_COMPRESSED: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "heic", "avif", "mp3", "m4a", "aac", "ogg", "flac", "wma",
    "mp4", "m4v", "mkv", "mov", "avi", "wmv", "webm", "zip", "7z", "rar", "gz", "bz2", "xz", "zst",
    "cab", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", "jar", "apk",
];

/// Art (`Source::kind`) einer einzelnen Datei, die für sich gesichert wird. Im Archiv liegt sie
/// auf der obersten Ebene, ohne Ordner darum.
pub const KIND_FILE: &str = "file";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Source {
    /// Ein Ordner – oder eine einzelne Datei (`kind` ist dann [`KIND_FILE`]).
    pub path: PathBuf,
    /// Schlüssel des Standardordners ("desktop", "documents", …), "drive", "file" oder `None`
    /// für eigene Ordner.
    pub kind: Option<String>,
    /// Name des Ordners im Archiv; `None` = Name des Quellordners.
    pub name: Option<String>,
    /// Unterordner (Namen in Kleinbuchstaben), die ausgelassen werden, z. B. Zwischenspeicher.
    pub exclude: Vec<String>,
    /// Vollständige Pfade (Ordner oder Dateien), die ausgelassen werden, z. B. `C:\Windows`.
    pub exclude_paths: Vec<PathBuf>,
}

impl Source {
    /// Quelle ohne Besonderheiten.
    pub fn plain(path: PathBuf, kind: Option<String>) -> Self {
        Self { path, kind, name: None, exclude: Vec::new(), exclude_paths: Vec::new() }
    }

    /// Ein Ordner oder eine einzelne Datei, vom Benutzer frei ausgewählt.
    pub fn picked(path: PathBuf) -> Self {
        let kind = path.is_file().then(|| KIND_FILE.to_string());
        Self::plain(path, kind)
    }

    /// Wird eine einzelne Datei gesichert (kein Ordner)?
    pub fn is_file(&self) -> bool {
        self.kind.as_deref() == Some(KIND_FILE)
    }

    /// Durchläuft den Ordner, ohne Verknüpfungen zu folgen und ohne ausgeschlossene Einträge.
    pub(crate) fn walk(&self) -> impl Iterator<Item = walkdir::Result<walkdir::DirEntry>> + '_ {
        WalkDir::new(&self.path).follow_links(false).into_iter().filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let by_name = entry.file_type().is_dir()
                && self.exclude.iter().any(|name| entry.file_name().to_string_lossy().eq_ignore_ascii_case(name));
            let by_path = self
                .exclude_paths
                .iter()
                .any(|path| entry.path().as_os_str().eq_ignore_ascii_case(path.as_os_str()));
            !(by_name || by_path)
        })
    }
}

/// Dateien und Ordner eines Laufwerks, die in einer Dateisicherung nichts verloren haben:
/// Auslagerungsdateien, Papierkorb, Zwischenspeicher. Mit `data_only` zusätzlich Windows und
/// die installierten Programme (sie lassen sich aus einer ZIP-Datei nicht startfähig zurückspielen).
pub fn drive_excludes(root: &Path, data_only: bool) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = [
        "pagefile.sys",
        "hiberfil.sys",
        "swapfile.sys",
        "DumpStack.log.tmp",
        "$Recycle.Bin",
        "System Volume Information",
        "Recovery",
        "$WinREAgent",
        "$SysReset",
        "Config.Msi",
        "Windows\\Temp",
        "Windows\\SoftwareDistribution\\Download",
        "Windows\\CSC",
        "WindowsImageBackup",
    ]
    .iter()
    .map(|p| root.join(p))
    .collect();
    if data_only {
        for dir in ["Windows", "Windows.old", "Program Files", "Program Files (x86)"] {
            paths.push(root.join(dir));
        }
    }
    // Temp-Ordner und Browser-Zwischenspeicher aller Benutzer
    if let Ok(users) = fs::read_dir(root.join("Users")) {
        for user in users.flatten() {
            let local = user.path().join("AppData").join("Local");
            paths.push(local.join("Temp"));
            paths.push(local.join("Microsoft").join("Windows").join("INetCache"));
        }
    }
    paths
}

/// Anmeldung an einer Netzwerkfreigabe.
#[derive(Clone, Debug)]
pub struct SmbLogin {
    pub user: String,
    pub password: String,
}

/// Ein Sicherungsziel, wie die Sicherung es braucht.
#[derive(Clone, Debug)]
pub enum Destination {
    /// Ordner auf einem Datenträger oder einer Netzwerkfreigabe
    Folder { name: String, path: PathBuf, login: Option<SmbLogin> },
    /// Ziel im Netz: Nextcloud, WebDAV oder S3-Speicher
    Cloud { name: String, settings: RemoteSettings },
}

#[cfg(test)]
impl Destination {
    /// Einfacher Ordner ohne Anmeldung.
    pub fn folder(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Destination::Folder { name: path.display().to_string(), path, login: None }
    }
}

pub struct BackupJob {
    /// Name und Ordner des Benutzerprofils, das gesichert wird.
    pub profile_name: String,
    pub profile_dir: PathBuf,
    pub sources: Vec<Source>,
    /// Wohin die ZIP-Datei kommt. Der erste Ordner in der Liste erhält sie direkt, alle
    /// weiteren Ziele bekommen danach eine Kopie.
    pub destinations: Vec<Destination>,
    pub compression: Compression,
    /// OneDrive-Dateien überspringen, die nur online vorliegen (kein automatischer Download).
    pub skip_online_only: bool,
    /// Die fertige ZIP-Datei vollständig lesen und gegen die Prüfsummen vergleichen.
    pub verify: bool,
    /// Passwort für die AES-256-Verschlüsselung der Dateiinhalte; `None` = unverschlüsselt.
    pub password: Option<String>,
    /// Nur Geändertes sichern (für Aufträge); `None` = immer vollständig.
    pub incremental: Option<Incremental>,
}

#[derive(Debug)]
pub struct BackupSummary {
    pub files: u64,
    pub bytes: u64,
    pub zip_size: u64,
    pub skipped_errors: u64,
    pub skipped_online: u64,
    /// Die ZIP-Datei wurde nach dem Schreiben vollständig geprüft.
    pub verified: bool,
    pub encrypted: bool,
    /// Die ZIP-Datei im ersten Ordner-Ziel (fehlt, wenn nur zu Nextcloud gesichert wurde)
    #[cfg_attr(not(test), allow(dead_code))]
    pub local_path: Option<PathBuf>,
    /// Alle Orte, an denen die Sicherung jetzt liegt, als "Zielname: Ort"
    pub saved: Vec<String>,
    pub kind: BackupKind,
    pub duration: Duration,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ManifestRoot {
    /// Name des Ordners im Archiv (oberste Ebene).
    pub name: String,
    pub original_path: String,
    #[serde(default)]
    pub kind: Option<String>,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub format: u32,
    pub created: String,
    pub computer: String,
    pub user: String,
    /// Profilordner zum Zeitpunkt des Backups (z. B. C:\Users\anna).
    #[serde(default)]
    pub profile_dir: String,
    /// Die Dateiinhalte sind mit einem Passwort verschlüsselt (AES-256).
    #[serde(default)]
    pub encrypted: bool,
    pub tool_version: String,
    pub roots: Vec<ManifestRoot>,
    /// Teilsicherung: enthält nur, was sich seit der vorigen Sicherung geändert hat.
    #[serde(default)]
    pub incremental: bool,
    /// Bei Teilsicherungen: Dateiname der Vollsicherung, auf der sie aufbaut.
    #[serde(default)]
    pub base: Option<String>,
}

/// "Nur Geändertes sichern": Zwischen zwei Vollsicherungen entstehen Teilsicherungen, die nur
/// neue und geänderte Dateien enthalten.
#[derive(Clone, Debug)]
pub struct Incremental {
    /// Hier merkt sich der Auftrag Größe und Änderungszeit aller gesicherten Dateien.
    pub state_file: PathBuf,
    /// Jede wievielte Sicherung wieder vollständig ist (7 = eine Voll-, dann sechs Teilsicherungen).
    pub full_every: u32,
}

/// Was eine Sicherung geworden ist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupKind {
    Full,
    /// Teilsicherung mit den Änderungen seit der vorigen Sicherung
    Partial,
    /// Seit der vorigen Sicherung hat sich nichts geändert: es wurde keine Datei angelegt.
    Unchanged,
}

/// Größe und Änderungszeit (Sekunden seit 1970) einer Datei.
type FileStamp = (u64, i64);

/// Stand nach der letzten Sicherung eines Auftrags.
#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Dateiname der letzten Vollsicherung
    base: String,
    /// Teilsicherungen seit der letzten Vollsicherung
    since_full: u32,
    files: HashMap<String, FileStamp>,
}

fn file_stamp(meta: &fs::Metadata) -> FileStamp {
    let modified = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs() as i64);
    (meta.len(), modified)
}

#[derive(Default)]
struct Stats {
    roots: Vec<ManifestRoot>,
    files: u64,
    bytes: u64,
    skipped_errors: u64,
    skipped_online: u64,
}

pub fn run(job: &BackupJob, rep: &Reporter) -> Result<BackupSummary> {
    let started = Instant::now();

    let mut sources = Vec::new();
    for src in &job.sources {
        // Eine einzelne Datei muss eine Datei sein, alles andere ein Ordner.
        if src.is_file() && src.path.is_file() || !src.is_file() && src.path.is_dir() {
            sources.push(src.clone());
        } else if src.is_file() {
            rep.warn(tr!(
                "Datei nicht gefunden, wird ausgelassen: {}",
                "File not found, skipping it: {}",
                src.path.display()
            ));
        } else {
            rep.warn(tr!(
                "Ordner nicht gefunden, wird ausgelassen: {}",
                "Folder not found, skipping it: {}",
                src.path.display()
            ));
        }
    }
    if sources.is_empty() {
        bail!(t!(
            "Nichts von der Auswahl ist vorhanden.",
            "Nothing of what you selected exists."
        ));
    }
    if job.destinations.is_empty() {
        bail!(t!("Es ist kein Ziel ausgewählt.", "No target is selected."));
    }

    // Alle Ziele zuerst prüfen – sonst fällt ein falsches Passwort oder ein fehlender
    // Datenträger erst nach dem Packen auf.
    let mut folders: Vec<(&str, &Path)> = Vec::new();
    let mut clouds: Vec<(&str, Box<dyn Remote>)> = Vec::new();
    for destination in &job.destinations {
        rep.check()?;
        match destination {
            Destination::Folder { name, path, login } => {
                rep.status(tr!("Ziel „{name}“ wird geprüft …", "Checking target “{name}” …"));
                if let Some(login) = login {
                    crate::smb::connect(&path.to_string_lossy(), &login.user, &login.password)
                        .with_context(|| tr!("Ziel „{name}“", "Target “{name}”"))?;
                }
                fs::create_dir_all(path).with_context(|| {
                    tr!("Ziel „{name}“ ist nicht erreichbar: {}", "Target “{name}” cannot be reached: {}", path.display())
                })?;
                folders.push((name.as_str(), path.as_path()));
            }
            Destination::Cloud { name, settings } => {
                rep.status(tr!("Anmeldung an „{name}“ wird geprüft …", "Checking sign-in to “{name}” …"));
                let client = settings.connect().with_context(|| tr!("Ziel „{name}“", "Target “{name}”"))?;
                clouds.push((name.as_str(), client));
            }
        }
    }

    // Nur Geändertes sichern: Gibt es einen Stand der letzten Sicherung und ist noch keine
    // Vollsicherung fällig, entsteht eine Teilsicherung.
    let previous: Option<State> = job
        .incremental
        .as_ref()
        .and_then(|inc| fs::read_to_string(&inc.state_file).ok())
        .and_then(|text| serde_json::from_str(&text).ok());
    let previous = match (&job.incremental, previous) {
        (Some(inc), Some(state)) if !state.base.is_empty() && state.since_full + 1 < inc.full_every.max(1) => Some(state),
        _ => None,
    };
    let partial = previous.is_some();
    let known = previous.as_ref().map(|state| &state.files);

    let file_name = archive_file_name(&job.profile_name, partial);
    let (work_path, final_local) = match folders.first() {
        Some((_, dir)) => (dir.join(format!("{file_name}.part")), Some(dir.join(&file_name))),
        None => (std::env::temp_dir().join(format!("{file_name}.part")), None),
    };
    let mut work_file = TempFile::new(&work_path);

    rep.status(t!("Dateien werden gezählt …", "Counting files …"));
    let (total_files, total_bytes) = scan(&sources, job.skip_online_only, &work_path, known, rep)?;
    if partial {
        rep.log(tr!(
            "Teilsicherung: {total_files} neue oder geänderte Dateien ({}).",
            "Partial backup: {total_files} new or changed files ({}).",
            fmt_bytes(total_bytes)
        ));
    } else {
        rep.log(tr!(
            "{total_files} Dateien ({}) werden gesichert.",
            "Backing up {total_files} files ({}).",
            fmt_bytes(total_bytes)
        ));
    }

    let base = previous.as_ref().map(|state| state.base.clone());
    let mut current: HashMap<String, FileStamp> = HashMap::new();
    let stats = write_archive(&sources, job, &work_path, total_bytes, known, base.clone(), &mut current, rep)?;
    if partial && stats.files == 0 {
        // Nichts geändert: keine leere ZIP-Datei anlegen (die Arbeitsdatei räumt sich selbst weg).
        rep.log(t!(
            "Seit der letzten Sicherung hat sich nichts geändert.",
            "Nothing has changed since the last backup."
        ));
        return Ok(BackupSummary {
            files: 0,
            bytes: 0,
            zip_size: 0,
            skipped_errors: stats.skipped_errors,
            skipped_online: stats.skipped_online,
            verified: false,
            encrypted: job.password.is_some(),
            local_path: None,
            saved: Vec::new(),
            kind: BackupKind::Unchanged,
            duration: started.elapsed(),
        });
    }
    let zip_size = fs::metadata(&work_path)?.len();

    // Erst prüfen, dann den endgültigen Namen vergeben: eine beschädigte Datei bleibt nicht liegen.
    if job.verify {
        rep.status(t!("Sicherung wird geprüft …", "Verifying backup …"));
        let report = crate::verify::verify_archive(
            &work_path,
            Some((stats.files, stats.bytes)),
            job.password.as_deref(),
            rep,
        )
            .context(t!(
                "Die Prüfung der ZIP-Datei ist fehlgeschlagen – möglicherweise ist der Zieldatenträger defekt",
                "Verifying the ZIP file failed – the target drive may be faulty"
            ))?;
        rep.log(tr!(
            "Geprüft: {} Dateien gelesen, alle Prüfsummen stimmen.",
            "Verified: {} files read, all checksums match.",
            report.files
        ));
    }

    // Erstes Ordner-Ziel: die fertige Datei bekommt erst jetzt ihren endgültigen Namen.
    let mut saved: Vec<String> = Vec::new();
    let source_file = match (&final_local, folders.first()) {
        (Some(final_path), Some((name, _))) => {
            fs::rename(&work_path, final_path)
                .with_context(|| tr!("Umbenennen nach {} fehlgeschlagen", "Renaming to {} failed", final_path.display()))?;
            work_file.keep();
            rep.log(tr!("ZIP-Datei gespeichert: {}", "ZIP file saved: {}", final_path.display()));
            saved.push(format!("{name}: {}", final_path.display()));
            final_path.clone()
        }
        _ => work_path.clone(),
    };
    // Schlägt ein weiteres Ziel fehl, soll die Meldung sagen, wo die Sicherung schon liegt.
    let explain = |error: anyhow::Error, name: &str, saved: &[String]| -> anyhow::Error {
        if error.is::<crate::job::Cancelled>() {
            return error;
        }
        let error = error.context(tr!("Ziel „{name}“ ist fehlgeschlagen", "Target “{name}” failed"));
        if saved.is_empty() {
            error
        } else {
            error.context(tr!(
                "Die Sicherung liegt bereits hier: {}",
                "The backup is already stored here: {}",
                saved.join("; ")
            ))
        }
    };

    // Weitere Ordner-Ziele bekommen eine Kopie.
    for (name, dir) in folders.iter().skip(1) {
        rep.status(tr!("Kopie nach „{name}“ ({}) …", "Copying to “{name}” ({}) …", fmt_bytes(zip_size)));
        let copy = dir.join(&file_name);
        copy_file(&source_file, &copy, zip_size, rep).map_err(|e| explain(e, name, &saved))?;
        rep.log(tr!("Kopie gespeichert: {}", "Copy saved: {}", copy.display()));
        saved.push(format!("{name}: {}", copy.display()));
    }

    for (name, client) in &clouds {
        rep.status(tr!("Upload nach „{name}“ ({}) …", "Uploading to “{name}” ({}) …", fmt_bytes(zip_size)));
        let uploaded = client.upload(&source_file, &file_name, rep).map_err(|e| explain(e, name, &saved))?;
        rep.log(tr!("Hochgeladen nach „{name}“: {uploaded}", "Uploaded to “{name}”: {uploaded}"));
        if job.verify {
            // Der Server meldet die Größe der angekommenen Datei; sie muss zur lokalen passen.
            match client.list() {
                Ok(list) => match list.iter().find(|f| f.name == file_name) {
                    Some(remote) if remote.size == zip_size => {
                        rep.log(tr!(
                            "„{name}“: Die Größe der hochgeladenen Datei stimmt.",
                            "“{name}”: the size of the uploaded file matches."
                        ));
                    }
                    Some(remote) => bail!(tr!(
                        "Die Datei auf „{name}“ ist unvollständig ({} statt {}). Bitte die Sicherung wiederholen.",
                        "The file on “{name}” is incomplete ({} instead of {}). Please run the backup again.",
                        fmt_bytes(remote.size),
                        fmt_bytes(zip_size)
                    )),
                    None => rep.warn(tr!(
                        "„{name}“: Die hochgeladene Datei wurde in der Liste nicht gefunden.",
                        "“{name}”: the uploaded file was not found in the file list."
                    )),
                },
                Err(e) => rep.warn(tr!(
                    "„{name}“: Die hochgeladene Datei konnte nicht geprüft werden ({e:#})",
                    "“{name}”: the uploaded file could not be verified ({e:#})"
                )),
            }
        }
        saved.push(format!("{name}: {uploaded}"));
    }

    // Erst wenn die Sicherung an allen Zielen liegt, gilt der neue Stand: Scheitert ein Lauf,
    // nimmt der nächste dieselben Änderungen noch einmal mit.
    if let Some(inc) = &job.incremental {
        let state = State {
            base: base.unwrap_or_else(|| file_name.clone()),
            since_full: previous.as_ref().map_or(0, |state| state.since_full + 1),
            files: current,
        };
        let written = inc
            .state_file
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|_| fs::write(&inc.state_file, serde_json::to_string(&state).unwrap_or_default()));
        if let Err(e) = written {
            rep.warn(tr!(
                "Der Stand für die nächste Teilsicherung wurde nicht gespeichert ({e}); die nächste Sicherung wird wieder vollständig.",
                "The state for the next partial backup was not saved ({e}); the next backup will be a full backup again."
            ));
            let _ = fs::remove_file(&inc.state_file);
        }
    }

    Ok(BackupSummary {
        files: stats.files,
        bytes: stats.bytes,
        zip_size,
        skipped_errors: stats.skipped_errors,
        skipped_online: stats.skipped_online,
        verified: job.verify,
        encrypted: job.password.is_some(),
        local_path: final_local,
        saved,
        kind: if partial { BackupKind::Partial } else { BackupKind::Full },
        duration: started.elapsed(),
    })
}

/// Kopiert die fertige ZIP-Datei in ein weiteres Ziel: erst als `.part`, dann umbenennen.
/// Die Größe der Kopie wird mit dem Original verglichen.
fn copy_file(source: &Path, target: &Path, size: u64, rep: &Reporter) -> Result<()> {
    let mut part_name = target.as_os_str().to_owned();
    part_name.push(".part");
    let mut part = TempFile::new(part_name);
    let mut input =
        File::open(source).with_context(|| tr!("{} ist nicht lesbar", "{} cannot be read", source.display()))?;
    let mut output = File::create(part.path())
        .with_context(|| tr!("Datei kann nicht angelegt werden: {}", "Cannot create file: {}", target.display()))?;
    let mut counter = Counter::new(rep, size);
    let mut buf = vec![0u8; COPY_BUFFER];
    loop {
        rep.check()?;
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow!(e).context(tr!("Lesefehler in {}", "Read error in {}", source.display()))),
        };
        output.write_all(&buf[..n]).map_err(|e| write_error(e, target))?;
        counter.add(n as u64);
    }
    output.sync_all().map_err(|e| write_error(e, target))?;
    let written = output.metadata()?.len();
    drop(output);
    if written != size {
        bail!(tr!(
            "Die Kopie ist unvollständig ({} statt {})",
            "The copy is incomplete ({} instead of {})",
            fmt_bytes(written),
            fmt_bytes(size)
        ));
    }
    fs::rename(part.path(), target)
        .with_context(|| tr!("Umbenennen nach {} fehlgeschlagen", "Renaming to {} failed", target.display()))?;
    part.keep();
    counter.flush();
    Ok(())
}

/// Dateiname der Sicherung, z. B. `Backup_PC01_anna_2026-11-10_143005.zip`.
pub fn archive_file_name(profile_name: &str, partial: bool) -> String {
    let stamp = Local::now().format("%Y-%m-%d_%H%M%S");
    format!("{}{stamp}{}.zip", archive_prefix(profile_name), if partial { PARTIAL_SUFFIX } else { "" })
}

/// Namenszusatz von Teilsicherungen, z. B. `Backup_PC01_anna_2026-11-10_143005_teil.zip`.
const PARTIAL_SUFFIX: &str = "_teil";

/// Der Teil des Dateinamens vor dem Zeitstempel, z. B. `Backup_PC01_anna_`.
pub fn archive_prefix(profile_name: &str) -> String {
    let computer = sanitize(&std::env::var("COMPUTERNAME").unwrap_or_default());
    let user = sanitize(profile_name);
    let mut name = String::from("Backup");
    for part in [computer, user] {
        if !part.is_empty() {
            name.push('_');
            name.push_str(&part);
        }
    }
    name.push('_');
    name
}

/// Gehört die Datei zu den Sicherungen mit diesem Präfix? Verlangt wird genau
/// `<Präfix>JJJJ-MM-TT_HHMMSS.zip` – nichts anderes wird beim Aufräumen je angefasst.
#[cfg(test)]
pub fn is_archive_of(file_name: &str, prefix: &str) -> bool {
    archive_stamp(file_name, prefix).is_some()
}

/// Zeitstempel einer Sicherung mit diesem Präfix und ob sie eine Teilsicherung ist.
/// `None`, wenn der Name nicht genau dem Muster entspricht.
pub fn archive_stamp<'a>(file_name: &'a str, prefix: &str) -> Option<(&'a str, bool)> {
    let rest = file_name.strip_prefix(prefix)?.strip_suffix(".zip")?;
    let (stamp, partial) = match rest.strip_suffix(PARTIAL_SUFFIX) {
        Some(stamp) => (stamp, true),
        None => (rest, false),
    };
    let pattern = b"dddd-dd-dd_dddddd";
    let matches = stamp.len() == pattern.len()
        && stamp.bytes().zip(pattern).all(|(c, p)| if *p == b'd' { c.is_ascii_digit() } else { c == *p });
    matches.then_some((stamp, partial))
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

#[cfg(windows)]
pub(crate) fn is_online_only(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const OFFLINE: u32 = 0x0000_1000;
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    meta.file_attributes() & (OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS) != 0
}

#[cfg(not(windows))]
pub(crate) fn is_online_only(_meta: &fs::Metadata) -> bool {
    false
}

/// Erster Durchlauf: Anzahl und Gesamtgröße für die Fortschrittsanzeige ermitteln.
fn scan(
    sources: &[Source],
    skip_online: bool,
    exclude: &Path,
    known: Option<&HashMap<String, FileStamp>>,
    rep: &Reporter,
) -> Result<(u64, u64)> {
    let (mut files, mut bytes) = (0u64, 0u64);
    for src in sources {
        for entry in src.walk() {
            rep.check()?;
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_file() || entry.path() == exclude {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if skip_online && is_online_only(&meta) {
                continue;
            }
            // Teilsicherung: unveränderte Dateien zählen nicht mit.
            if known.is_some_and(|known| known.get(entry.path().to_string_lossy().as_ref()) == Some(&file_stamp(&meta))) {
                continue;
            }
            files += 1;
            bytes += meta.len();
            if files % 2000 == 0 {
                rep.status(tr!("Dateien werden gezählt … {files}", "Counting files … {files}"));
            }
        }
    }
    Ok((files, bytes))
}

/// Eindeutiger Ordnername auf oberster Ebene des Archivs.
fn root_name(source: &Source, used: &mut HashSet<String>) -> String {
    let path = &source.path;
    let base = source
        .name
        .clone()
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| {
            // Laufwerkswurzel wie "D:\"
            let s: String = path.to_string_lossy().chars().filter(|c| c.is_alphanumeric()).collect();
            if s.is_empty() { "Ordner".into() } else { s }
        });
    // Bei einzelnen Dateien kommt die Nummer vor die Endung: "Brief (2).docx".
    let (stem, extension) = match base.rfind('.') {
        Some(dot) if source.is_file() && dot > 0 => base.split_at(dot),
        _ => (base.as_str(), ""),
    };
    let mut name = base.clone();
    let mut n = 2;
    while !used.insert(name.to_lowercase()) {
        name = format!("{stem} ({n}){extension}");
        n += 1;
    }
    name
}

fn entry_name(root: &str, rel: &Path) -> String {
    let mut name = String::from(root);
    for comp in rel.components() {
        name.push('/');
        name.push_str(&comp.as_os_str().to_string_lossy());
    }
    name
}

fn zip_time(meta: &fs::Metadata) -> zip::DateTime {
    let modified: chrono::DateTime<Local> = meta.modified().map(Into::into).unwrap_or_else(|_| Local::now());
    zip::DateTime::try_from(modified.naive_local())
        .or_else(|_| zip::DateTime::try_from(Local::now().naive_local()))
        .unwrap_or_default()
}

fn file_options<'a>(
    compression: Compression,
    path: &Path,
    meta: &fs::Metadata,
    password: Option<&'a str>,
) -> zip::write::FileOptions<'a, 'static, ()> {
    let precompressed = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ALREADY_COMPRESSED.contains(&e.to_ascii_lowercase().as_str()));
    let options = SimpleFileOptions::default()
        .last_modified_time(zip_time(meta))
        .large_file(meta.len() >= LARGE_FILE_THRESHOLD);
    let options = match compression.deflate_level() {
        Some(level) if !precompressed && meta.len() > 0 => options
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(level)),
        _ => options.compression_method(CompressionMethod::Stored),
    };
    match password {
        Some(password) => options.with_aes_encryption(zip::AesMode::Aes256, password),
        None => options,
    }
}

/// Fehler beim Schreiben des Archivs verständlich machen (z. B. FAT32-Grenze auf USB-Sticks).
fn write_error(err: impl Into<anyhow::Error>, zip_path: &Path) -> anyhow::Error {
    let size = fs::metadata(zip_path).map(|m| m.len()).unwrap_or(0);
    let mut msg =
        tr!("Fehler beim Schreiben der ZIP-Datei {}", "Error writing the ZIP file {}", zip_path.display());
    if size >= 0xFFF0_0000 && size <= 0x1_0010_0000 {
        msg.push_str(t!(
            " – das Ziellaufwerk ist vermutlich mit FAT32 formatiert (höchstens 4 GB pro Datei). \
             Abhilfe: Laufwerk mit exFAT oder NTFS formatieren",
            " – the target drive is probably formatted with FAT32 (at most 4 GB per file). \
             Fix: format the drive with exFAT or NTFS"
        ));
    }
    err.into().context(msg)
}

/// Packt die Dateien. `known` ist der Stand der vorigen Sicherung (dann entsteht eine
/// Teilsicherung auf Grundlage von `base`), `current` nimmt den neuen Stand aller Dateien auf.
#[allow(clippy::too_many_arguments)]
fn write_archive(
    sources: &[Source],
    job: &BackupJob,
    zip_path: &Path,
    total_bytes: u64,
    known: Option<&HashMap<String, FileStamp>>,
    base: Option<String>,
    current: &mut HashMap<String, FileStamp>,
    rep: &Reporter,
) -> Result<Stats> {
    let file = File::create(zip_path)
        .with_context(|| tr!("ZIP-Datei kann nicht angelegt werden: {}", "Cannot create ZIP file: {}", zip_path.display()))?;
    let mut zip = ZipWriter::new(BufWriter::with_capacity(COPY_BUFFER, file));
    let dir_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let mut stats = Stats::default();
    let mut counter = Counter::new(rep, total_bytes);
    let mut used_names = HashSet::new();
    let mut buf = vec![0u8; COPY_BUFFER];

    for src in sources {
        let root = root_name(src, &mut used_names);
        rep.status(tr!("Sichere {} …", "Backing up {} …", src.path.display()));
        let mut root_stats = ManifestRoot {
            name: root.clone(),
            original_path: src.path.to_string_lossy().into_owned(),
            kind: src.kind.clone(),
            files: 0,
            bytes: 0,
        };

        for entry in src.walk() {
            rep.check()?;
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    rep.warn(tr!("Übersprungen (kein Zugriff): {e}", "Skipped (no access): {e}"));
                    stats.skipped_errors += 1;
                    continue;
                }
            };
            let file_type = entry.file_type();
            // Verknüpfungen/Junctions (z. B. "Eigene Bilder") nicht verfolgen.
            if entry.depth() > 0 && file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let rel = path.strip_prefix(&src.path).unwrap_or(Path::new(""));
            let name = entry_name(&root, rel);

            if file_type.is_dir() {
                // Eine Teilsicherung enthält nur die Ordner, in denen sich etwas geändert hat
                // (sie entstehen beim Entpacken von selbst).
                if known.is_none() {
                    zip.add_directory(name, dir_options).map_err(|e| write_error(e, zip_path))?;
                }
                continue;
            }
            if !file_type.is_file() || path == zip_path {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    rep.warn(tr!("Übersprungen: {} ({e})", "Skipped: {} ({e})", path.display()));
                    stats.skipped_errors += 1;
                    continue;
                }
            };
            if job.skip_online_only && is_online_only(&meta) {
                stats.skipped_online += 1;
                continue;
            }
            let size = meta.len();
            let key = path.to_string_lossy().into_owned();
            let stamp = file_stamp(&meta);
            if known.is_some_and(|known| known.get(&key) == Some(&stamp)) {
                // Unverändert seit der vorigen Sicherung: nicht noch einmal packen.
                current.insert(key, stamp);
                continue;
            }
            let mut input = match File::open(path) {
                Ok(f) => f,
                Err(e) => {
                    rep.warn(tr!("Übersprungen: {} ({e})", "Skipped: {} ({e})", path.display()));
                    stats.skipped_errors += 1;
                    counter.add(size);
                    continue;
                }
            };

            zip.start_file(name, file_options(job.compression, path, &meta, job.password.as_deref()))
                .map_err(|e| write_error(e, zip_path))?;
            let mut written = 0u64;
            let mut read_error = None;
            loop {
                rep.check()?;
                let n = match input.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        read_error = Some(e);
                        break;
                    }
                };
                zip.write_all(&buf[..n]).map_err(|e| write_error(e, zip_path))?;
                written += n as u64;
                counter.add(n as u64);
            }

            if let Some(e) = read_error {
                // Halb gelesene Datei wieder aus dem Archiv entfernen.
                zip.abort_file().map_err(|e| write_error(e, zip_path))?;
                rep.warn(tr!("Übersprungen (Lesefehler): {} ({e})", "Skipped (read error): {} ({e})", path.display()));
                stats.skipped_errors += 1;
                counter.add(size.saturating_sub(written));
                continue;
            }
            root_stats.files += 1;
            root_stats.bytes += written;
            // Erst jetzt gilt die Datei als gesichert; nach einem Lesefehler kommt sie nächstes Mal wieder dran.
            current.insert(key, stamp);
        }

        stats.files += root_stats.files;
        stats.bytes += root_stats.bytes;
        stats.roots.push(root_stats);
    }

    let manifest = Manifest {
        format: 1,
        created: Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
        computer: std::env::var("COMPUTERNAME").unwrap_or_default(),
        user: job.profile_name.clone(),
        profile_dir: job.profile_dir.to_string_lossy().into_owned(),
        encrypted: job.password.is_some(),
        tool_version: env!("CARGO_PKG_VERSION").into(),
        roots: stats.roots.clone(),
        incremental: known.is_some(),
        base,
    };
    let manifest_options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .last_modified_time(
            zip::DateTime::try_from(Local::now().naive_local()).unwrap_or_default(),
        );
    zip.start_file(MANIFEST_NAME, manifest_options).map_err(|e| write_error(e, zip_path))?;
    zip.write_all(serde_json::to_string_pretty(&manifest)?.as_bytes())
        .map_err(|e| write_error(e, zip_path))?;

    rep.status(t!("ZIP-Datei wird abgeschlossen …", "Finalizing ZIP file …"));
    let mut writer = zip.finish().map_err(|e| write_error(e, zip_path))?;
    writer.flush().map_err(|e| write_error(e, zip_path))?;
    let file = writer.into_inner().map_err(|e| write_error(anyhow!("{e}"), zip_path))?;
    // Wichtig bei USB-Sticks: Daten wirklich auf den Datenträger schreiben lassen.
    file.sync_all().map_err(|e| write_error(e, zip_path))?;
    counter.flush();
    Ok(stats)
}
