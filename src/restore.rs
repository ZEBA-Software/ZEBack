//! Wiederherstellung: ZIP-Sicherung prüfen und an den ursprünglichen oder einen anderen Ort entpacken.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{Local, NaiveDateTime, TimeZone};
use zip::result::ZipError;
use zip::{ZipArchive, ZipReadOptions};

use crate::backup::{MANIFEST_NAME, Manifest, ManifestRoot};
use crate::job::{Cancelled, Counter, Reporter};
use crate::profiles::Profile;

const COPY_BUFFER: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct ArchiveInfo {
    pub path: PathBuf,
    pub size: u64,
    pub entries: usize,
    /// Entpackte Gesamtgröße laut Archiv.
    pub total_bytes: u64,
    /// Fehlt bei ZIP-Dateien, die nicht von diesem Programm stammen.
    pub manifest: Option<Manifest>,
    /// Die Dateiinhalte sind verschlüsselt; zum Wiederherstellen wird das Passwort gebraucht.
    pub encrypted: bool,
}

impl ArchiveInfo {
    /// Sicherung eines ganzen Laufwerks (statt einzelner Profilordner).
    pub fn is_drive_backup(&self) -> bool {
        self.manifest.as_ref().is_some_and(|m| m.roots.iter().any(|r| r.kind.as_deref() == Some("drive")))
    }

    /// Lässt sich die Sicherung in die Ordner eines Benutzerprofils zurückspielen?
    /// Laufwerkssicherungen und fremde ZIP-Dateien gehen nur in einen frei gewählten Ordner.
    pub fn fits_profile(&self) -> bool {
        self.manifest.is_some() && !self.is_drive_backup()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// Vorhandene Dateien bleiben unverändert.
    Skip,
    Overwrite,
}

#[derive(Clone, Debug)]
pub enum Mapping {
    /// Ausgewählte Ordner der Sicherung (Name im Archiv) → Zielordner.
    Roots(HashMap<String, PathBuf>),
    /// Fremdes ZIP ohne Inhaltsbeschreibung: alles in diesen Ordner entpacken.
    Everything(PathBuf),
}

pub struct RestoreJob {
    pub archive: PathBuf,
    pub mapping: Mapping,
    pub conflict: Conflict,
    /// Erwartete Datenmenge für die Fortschrittsanzeige.
    pub total_bytes: u64,
    /// Passwort einer verschlüsselten Sicherung.
    pub password: Option<String>,
    /// Nur diese Dateien und Ordner wiederherstellen (Pfade im Archiv mit `/`); `None` = alles.
    pub only: Option<HashSet<String>>,
}

/// Eine Datei der Sicherung, wie sie im Archiv liegt.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Pfad im Archiv mit `/` als Trenner, z. B. `Dokumente/Briefe/brief.txt`
    pub path: String,
    pub size: u64,
}

/// Ein Ordner oder eine Datei in der Baumansicht der Sicherung.
#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub name: String,
    pub path: String,
    pub dir: bool,
    /// Bei Ordnern: Summe aller Dateien darin
    pub size: u64,
    pub files: u64,
}

/// Der Inhalt der Sicherung als Baum, zum Blättern und Auswählen einzelner Dateien.
#[derive(Default, Debug)]
pub struct Tree {
    children: HashMap<String, Vec<Node>>,
    stats: HashMap<String, (u64, u64)>,
}

impl Tree {
    pub fn build(entries: &[Entry]) -> Self {
        let mut stats: HashMap<String, (u64, u64)> = HashMap::new();
        let mut kids: HashMap<String, HashMap<String, bool>> = HashMap::new();
        for entry in entries {
            let parts: Vec<&str> = entry.path.split('/').filter(|p| !p.is_empty()).collect();
            let mut parent = String::new();
            for (depth, part) in parts.iter().enumerate() {
                let path = if parent.is_empty() { part.to_string() } else { format!("{parent}/{part}") };
                kids.entry(parent).or_default().insert(part.to_string(), depth + 1 < parts.len());
                let stat = stats.entry(path.clone()).or_default();
                stat.0 += 1;
                stat.1 += entry.size;
                parent = path;
            }
        }
        let children = kids
            .into_iter()
            .map(|(dir, names)| {
                let mut nodes: Vec<Node> = names
                    .into_iter()
                    .map(|(name, is_dir)| {
                        let path = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
                        let (files, size) = stats.get(&path).copied().unwrap_or_default();
                        Node { name, path, dir: is_dir, size, files }
                    })
                    .collect();
                // Ordner zuerst, dann nach Namen
                nodes.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
                (dir, nodes)
            })
            .collect();
        Self { children, stats }
    }

    /// Inhalt eines Ordners (`""` = oberste Ebene).
    pub fn children(&self, dir: &str) -> &[Node] {
        self.children.get(dir).map(Vec::as_slice).unwrap_or_default()
    }

    /// Anzahl der Dateien und Datenmenge einer Auswahl.
    pub fn total(&self, selection: &HashSet<String>) -> (u64, u64) {
        selection.iter().filter_map(|path| self.stats.get(path)).fold((0, 0), |sum, s| (sum.0 + s.0, sum.1 + s.1))
    }
}

/// Liegt der Pfad selbst oder einer seiner übergeordneten Ordner in der Auswahl?
pub fn is_selected(selection: &HashSet<String>, path: &str) -> bool {
    selection.contains(path) || path.match_indices('/').any(|(end, _)| selection.contains(&path[..end]))
}

/// Liest die Liste aller Dateien der Sicherung (ohne die Inhaltsbeschreibung). Dateinamen sind
/// auch bei verschlüsselten Sicherungen lesbar, ein Passwort ist dafür nicht nötig.
pub fn list_entries(path: &Path) -> Result<Vec<Entry>> {
    let file = File::open(path)
        .with_context(|| tr!("Datei kann nicht geöffnet werden: {}", "Cannot open file: {}", path.display()))?;
    let archive = ZipArchive::new(BufReader::new(file))
        .with_context(|| tr!("{} ist keine gültige ZIP-Datei", "{} is not a valid ZIP file", path.display()))?;
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let Ok(entry) = archive.by_index_data(index) else { continue };
        if !entry.is_file() {
            continue;
        }
        let Some(rel) = entry.enclosed_name() else { continue };
        let path = archive_path(&rel);
        if path != MANIFEST_NAME {
            entries.push(Entry { path, size: entry.size() });
        }
    }
    Ok(entries)
}

/// Pfad im Archiv als Text mit `/` als Trenner.
fn archive_path(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[derive(Debug)]
pub struct RestoreSummary {
    pub restored: u64,
    pub bytes: u64,
    pub skipped_existing: u64,
    pub failed: u64,
    pub duration: Duration,
}

/// Öffnet die ZIP-Datei und liest die Inhaltsbeschreibung.
pub fn inspect(path: &Path) -> Result<ArchiveInfo> {
    let file = File::open(path)
        .with_context(|| tr!("Datei kann nicht geöffnet werden: {}", "Cannot open file: {}", path.display()))?;
    let size = file.metadata()?.len();
    let mut archive = ZipArchive::new(BufReader::new(file))
        .with_context(|| tr!("{} ist keine gültige ZIP-Datei", "{} is not a valid ZIP file", path.display()))?;
    let manifest = match archive.by_name(MANIFEST_NAME) {
        Ok(mut entry) => {
            let mut text = String::new();
            entry.read_to_string(&mut text).ok();
            serde_json::from_str::<Manifest>(&text).ok()
        }
        Err(_) => None,
    };
    let total_bytes = archive.decompressed_size().map(|s| s.min(u64::MAX as u128) as u64).unwrap_or(0);
    // Bei fremden ZIP-Dateien an den ersten Einträgen erkennen, ob verschlüsselt wurde.
    let encrypted = manifest.as_ref().is_some_and(|m| m.encrypted)
        || (0..archive.len().min(200)).any(|i| archive.by_index_data(i).is_ok_and(|e| e.encrypted()));
    Ok(ArchiveInfo { path: path.to_path_buf(), size, entries: archive.len(), total_bytes, manifest, encrypted })
}

/// Prüft das Passwort einer verschlüsselten Sicherung, indem eine kleine Datei vollständig
/// entschlüsselt wird. `Ok(false)` = falsches Passwort.
pub fn check_password(path: &Path, password: &str) -> Result<bool> {
    const MAX_PROBE: u64 = 16 * 1024 * 1024;
    let file = File::open(path)
        .with_context(|| tr!("Datei kann nicht geöffnet werden: {}", "Cannot open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(BufReader::new(file))
        .with_context(|| tr!("{} ist keine gültige ZIP-Datei", "{} is not a valid ZIP file", path.display()))?;
    // Die kleinste verschlüsselte Datei unter den ersten Einträgen ist am schnellsten geprüft.
    let probe = (0..archive.len().min(5000))
        .filter_map(|i| {
            let entry = archive.by_index_data(i).ok()?;
            (entry.encrypted() && entry.is_file()).then(|| (entry.size(), i))
        })
        .min();
    let Some((size, index)) = probe else { return Ok(true) };

    let options = ZipReadOptions::new().password(Some(password.as_bytes()));
    let mut entry = match archive.by_index_with_options(index, options) {
        Ok(entry) => entry,
        Err(ZipError::InvalidPassword) => return Ok(false),
        Err(e) => return Err(anyhow!(e).context(t!("Die Sicherung ist nicht lesbar", "The backup cannot be read"))),
    };
    // Erst am Dateiende steht fest, ob Passwort und Inhalt wirklich zusammenpassen.
    if size <= MAX_PROBE && io::copy(&mut entry, &mut io::sink()).is_err() {
        return Ok(false);
    }
    Ok(true)
}

/// Zielordner bei der Wiederherstellung in ein Benutzerprofil: Standardordner landen in den
/// Ordnern des gewählten Profils (auch auf einem neuen PC oder mit anderem Benutzernamen),
/// eigene Ordner aus dem alten Profil an derselben Stelle im gewählten Profil,
/// alles andere am alten Pfad.
pub fn target_for(manifest: &Manifest, root: &ManifestRoot, profile: &Profile) -> PathBuf {
    if let Some(path) = root.kind.as_deref().and_then(|kind| profile.target_for(kind)) {
        return path;
    }
    let original = PathBuf::from(&root.original_path);
    if !manifest.profile_dir.is_empty()
        && let Ok(rest) = original.strip_prefix(&manifest.profile_dir)
    {
        return profile.dir.join(rest);
    }
    original
}

fn to_system_time(time: zip::DateTime) -> Option<SystemTime> {
    let naive = NaiveDateTime::try_from(time).ok()?;
    Local.from_local_datetime(&naive).earliest().map(SystemTime::from)
}

pub fn run(job: &RestoreJob, rep: &Reporter) -> Result<RestoreSummary> {
    let started = Instant::now();
    let file = File::open(&job.archive)
        .with_context(|| tr!("Datei kann nicht geöffnet werden: {}", "Cannot open file: {}", job.archive.display()))?;
    let mut archive = ZipArchive::new(BufReader::with_capacity(COPY_BUFFER, file))
        .with_context(|| tr!("{} ist keine gültige ZIP-Datei", "{} is not a valid ZIP file", job.archive.display()))?;

    let mut summary = RestoreSummary { restored: 0, bytes: 0, skipped_existing: 0, failed: 0, duration: Duration::ZERO };
    let mut counter = Counter::new(rep, job.total_bytes);
    let mut buf = vec![0u8; COPY_BUFFER];
    rep.status(t!("Dateien werden wiederhergestellt …", "Restoring files …"));

    for index in 0..archive.len() {
        rep.check()?;
        let options = ZipReadOptions::new().password(job.password.as_deref().map(str::as_bytes));
        let mut entry = match archive.by_index_with_options(index, options) {
            Ok(e) => e,
            // Ohne passendes Passwort ist nichts wiederherstellbar: sofort abbrechen.
            Err(ZipError::InvalidPassword) => bail!(t!("Das Passwort ist falsch", "The password is wrong")),
            Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) => {
                bail!(t!(
                    "Die Sicherung ist verschlüsselt – bitte das Passwort angeben",
                    "The backup is encrypted – please enter the password"
                ))
            }
            Err(e) => {
                rep.warn(tr!("Eintrag {index} ist nicht lesbar ({e})", "Entry {index} cannot be read ({e})"));
                summary.failed += 1;
                continue;
            }
        };
        // `enclosed_name` verhindert, dass ein manipuliertes Archiv außerhalb des Ziels schreibt.
        let Some(rel) = entry.enclosed_name() else {
            rep.warn(tr!(
                "Unsicherer Pfad im Archiv übersprungen: {}",
                "Skipped unsafe path in the archive: {}",
                entry.name().unwrap_or_default()
            ));
            summary.failed += 1;
            continue;
        };
        // Bei einer Auswahl einzelner Dateien alles andere auslassen.
        if let Some(only) = &job.only
            && !is_selected(only, &archive_path(&rel))
        {
            continue;
        }
        let target = match &job.mapping {
            Mapping::Everything(dest) => dest.join(&rel),
            Mapping::Roots(roots) => {
                let mut components = rel.components();
                let Some(Component::Normal(first)) = components.next() else { continue };
                // Nicht ausgewählte Ordner und die Inhaltsbeschreibung auslassen.
                let Some(dest) = roots.get(first.to_string_lossy().as_ref()) else { continue };
                dest.join(components.as_path())
            }
        };

        if entry.is_dir() {
            if let Err(e) = fs::create_dir_all(&target) {
                rep.warn(tr!(
                    "Ordner kann nicht angelegt werden: {} ({e})",
                    "Cannot create folder: {} ({e})",
                    target.display()
                ));
                summary.failed += 1;
            }
            continue;
        }
        if !entry.is_file() {
            continue;
        }

        let size = entry.size();
        if job.conflict == Conflict::Skip && target.exists() {
            summary.skipped_existing += 1;
            counter.add(size);
            continue;
        }

        let before = counter.done();
        let modified = entry.last_modified().and_then(to_system_time);
        match extract_file(&mut entry, &target, modified, rep, &mut counter, &mut buf) {
            Ok(written) => {
                summary.restored += 1;
                summary.bytes += written;
            }
            Err(e) if e.is::<Cancelled>() => return Err(e),
            Err(e) => {
                rep.warn(tr!("Nicht wiederhergestellt: {} ({e:#})", "Not restored: {} ({e:#})", target.display()));
                summary.failed += 1;
                counter.set(before + size);
            }
        }
    }

    counter.flush();
    summary.duration = started.elapsed();
    Ok(summary)
}

/// Zu einer Teilsicherung die ganze Kette: die Vollsicherung, auf der sie aufbaut, alle
/// Teilsicherungen dazwischen und sie selbst – älteste zuerst. Gesucht wird im Ordner der
/// geöffneten Datei. `None`, wenn die Datei keine Teilsicherung ist oder die Vollsicherung fehlt.
pub fn chain_for(info: &ArchiveInfo) -> Option<Vec<PathBuf>> {
    let manifest = info.manifest.as_ref().filter(|m| m.incremental)?;
    let base = manifest.base.as_deref()?;
    let dir = info.path.parent()?;
    if !dir.join(base).is_file() {
        return None;
    }
    // Vollsicherungen heißen `<Präfix>JJJJ-MM-TT_HHMMSS.zip`.
    let prefix = base.get(..base.len().checked_sub("0000-00-00_000000.zip".len())?)?;
    let (base_stamp, false) = crate::backup::archive_stamp(base, prefix)? else { return None };
    let own_name = info.path.file_name()?.to_string_lossy().into_owned();
    let (own_stamp, true) = crate::backup::archive_stamp(&own_name, prefix)? else { return None };

    let mut partials: Vec<String> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| {
            matches!(crate::backup::archive_stamp(name, prefix), Some((stamp, true)) if stamp > base_stamp && stamp <= own_stamp)
        })
        .collect();
    partials.sort();
    Some(std::iter::once(base.to_string()).chain(partials).map(|name| dir.join(name)).collect())
}

/// Stellt eine Kette aus Vollsicherung und Teilsicherungen in einem Zug wieder her. Von jeder
/// Datei gilt der Stand aus der neuesten Sicherung, die sie enthält. `template` liefert
/// Zuordnung, Umgang mit vorhandenen Dateien, Passwort und (optional) eine Einzelauswahl.
pub fn run_chain(archives: &[PathBuf], template: &RestoreJob, rep: &Reporter) -> Result<RestoreSummary> {
    let started = Instant::now();
    // Von neu nach alt: Was eine neuere Sicherung schon liefert, lassen die älteren aus.
    let mut seen: HashSet<String> = HashSet::new();
    let mut plan: Vec<(&PathBuf, HashSet<String>, u64)> = Vec::new();
    for archive in archives.iter().rev() {
        rep.check()?;
        rep.status(tr!(
            "Inhalt wird gelesen: {} …",
            "Reading contents: {} …",
            archive.file_name().unwrap_or_default().to_string_lossy()
        ));
        let mut only = HashSet::new();
        let mut bytes = 0;
        for entry in list_entries(archive)? {
            let wanted = template.only.as_ref().is_none_or(|picked| is_selected(picked, &entry.path));
            if wanted && seen.insert(entry.path.clone()) {
                bytes += entry.size;
                only.insert(entry.path);
            }
        }
        plan.push((archive, only, bytes));
    }

    let mut total = RestoreSummary { restored: 0, bytes: 0, skipped_existing: 0, failed: 0, duration: Duration::ZERO };
    for (step, (archive, only, bytes)) in plan.into_iter().rev().enumerate() {
        if only.is_empty() {
            continue;
        }
        rep.log(tr!("Sicherung {} von {}: {}", "Backup {} of {}: {}", step + 1, archives.len(), archive.display()));
        let job = RestoreJob {
            archive: archive.clone(),
            mapping: template.mapping.clone(),
            conflict: template.conflict,
            total_bytes: bytes,
            password: template.password.clone(),
            only: Some(only),
        };
        let part = run(&job, rep)?;
        total.restored += part.restored;
        total.bytes += part.bytes;
        total.skipped_existing += part.skipped_existing;
        total.failed += part.failed;
    }
    total.duration = started.elapsed();
    Ok(total)
}

/// Schreibt zuerst in eine Nachbardatei und ersetzt das Ziel erst, wenn alles vollständig ist.
/// So bleibt eine vorhandene Datei bei Abbruch oder Fehler unversehrt.
fn extract_file(
    entry: &mut impl Read,
    target: &Path,
    modified: Option<SystemTime>,
    rep: &Reporter,
    counter: &mut Counter,
    buf: &mut [u8],
) -> Result<u64> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp_name: OsString = target.as_os_str().to_owned();
    temp_name.push(".pbtmp");
    let mut temp = crate::util::TempFile::new(temp_name);

    let file = File::create(temp.path())?;
    let mut out = BufWriter::with_capacity(COPY_BUFFER, file);
    let mut written = 0u64;
    loop {
        rep.check()?;
        let n = match entry.read(buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow!(e).context(t!("Daten im Archiv beschädigt", "Data in the archive is damaged"))),
        };
        out.write_all(&buf[..n])?;
        written += n as u64;
        counter.add(n as u64);
    }
    let file = out.into_inner().map_err(|e| anyhow!("{e}"))?;
    if let Some(time) = modified {
        let _ = file.set_modified(time);
    }
    drop(file);

    // Schreibgeschützte Zieldatei ersetzbar machen.
    if let Ok(meta) = fs::metadata(target) {
        let mut permissions = meta.permissions();
        if permissions.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = fs::set_permissions(target, permissions);
        }
    }
    fs::rename(temp.path(), target)?;
    temp.keep();
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{self, BackupJob, Destination, Source};
    use crate::config::Compression;
    use crate::job::Msg;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("packup-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    fn make_backup(base: &Path, compression: Compression) -> (PathBuf, PathBuf) {
        make_backup_with(base, compression, None)
    }

    fn make_backup_with(base: &Path, compression: Compression, password: Option<&str>) -> (PathBuf, PathBuf) {
        let docs = base.join("quelle").join("Dokumente");
        let photos = base.join("quelle").join("Fotos Übersee");
        write(&docs.join("brief.txt"), "Sehr geehrte Damen und Herren, äöü ß".repeat(500).as_bytes());
        write(&docs.join("Unterordner").join("tief").join("tabelle.csv"), b"a;b;c\n1;2;3\n");
        write(&docs.join("leer.txt"), b"");
        fs::create_dir_all(docs.join("Leerer Ordner")).unwrap();
        let binary: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        write(&photos.join("bild.jpg"), &binary);

        let target = base.join("ziel");
        let job = BackupJob {
            profile_name: "anna".into(),
            profile_dir: base.join("quelle"),
            sources: vec![
                Source::plain(docs, None),
                Source::plain(photos, None),
                Source::plain(base.join("gibt-es-nicht"), None),
            ],
            destinations: vec![Destination::folder(target.clone())],
                        compression,
            skip_online_only: true,
            verify: true,
            password: password.map(str::to_owned),
            incremental: None,
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = backup::run(&job, &rep).unwrap();
        assert_eq!(summary.files, 4);
        assert_eq!(summary.skipped_errors, 0);
        let zip_path = summary.local_path.unwrap();
        assert!(zip_path.exists());
        assert_eq!(summary.zip_size, fs::metadata(&zip_path).unwrap().len());
        // Keine halbfertige .part-Datei zurücklassen.
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
        (zip_path, base.join("quelle"))
    }

    fn assert_same_tree(a: &Path, b: &Path) {
        let list = |root: &Path| {
            let mut items: Vec<(PathBuf, Option<Vec<u8>>)> = walkdir::WalkDir::new(root)
                .min_depth(1)
                .into_iter()
                .map(|e| e.unwrap())
                .map(|e| {
                    let rel = e.path().strip_prefix(root).unwrap().to_path_buf();
                    let data = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
                    (rel, data)
                })
                .collect();
            items.sort();
            items
        };
        assert_eq!(list(a), list(b));
    }

    #[test]
    fn backup_and_restore_roundtrip() {
        for (name, compression) in [("rt-normal", Compression::Normal), ("rt-none", Compression::None)] {
            let base = temp_dir(name);
            let (zip_path, source) = make_backup(&base, compression);

            let info = inspect(&zip_path).unwrap();
            let manifest = info.manifest.clone().expect("Inhaltsbeschreibung fehlt");
            let names: Vec<&str> = manifest.roots.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["Dokumente", "Fotos Übersee"]);
            assert_eq!(manifest.roots[0].files, 3);
            assert_eq!(manifest.roots[1].bytes, 300_000);

            let out = base.join("wiederhergestellt");
            let mapping = manifest.roots.iter().map(|r| (r.name.clone(), out.join(&r.name))).collect();
            let job = RestoreJob {
                archive: zip_path,
                mapping: Mapping::Roots(mapping),
                conflict: Conflict::Skip,
                total_bytes: info.total_bytes,
                password: None,
            only: None,
            };
            let (rep, rx, _cancel) = Reporter::new(|| {});
            let summary = run(&job, &rep).unwrap();
            assert_eq!((summary.restored, summary.failed, summary.skipped_existing), (4, 0, 0));
            assert!(!rx.try_iter().any(|m| matches!(m, Msg::Warn(_))));
            assert_same_tree(&source, &out);
            assert!(!out.join(MANIFEST_NAME).exists());

            // Änderungszeit bleibt erhalten (ZIP speichert auf 2 Sekunden genau).
            let original = fs::metadata(source.join("Dokumente").join("brief.txt")).unwrap().modified().unwrap();
            let restored = fs::metadata(out.join("Dokumente").join("brief.txt")).unwrap().modified().unwrap();
            let diff = original.duration_since(restored).or_else(|_| restored.duration_since(original)).unwrap();
            assert!(diff <= Duration::from_secs(2), "{diff:?}");
            let _ = fs::remove_dir_all(base);
        }
    }

    #[test]
    fn partial_backups_form_a_chain_that_restores_the_newest_state() {
        use crate::backup::{BackupKind, Incremental};
        let base = temp_dir("teil");
        let docs = base.join("quelle").join("Dokumente");
        write(&docs.join("a.txt"), b"eins");
        write(&docs.join("b.txt"), b"zwei");
        write(&docs.join("tief").join("c.txt"), b"drei");
        let target = base.join("ziel");
        let job = BackupJob {
            profile_name: "anna".into(),
            profile_dir: base.join("quelle"),
            sources: vec![Source::plain(docs.clone(), None)],
            destinations: vec![Destination::folder(target.clone())],
            compression: Compression::Fast,
            skip_online_only: true,
            verify: true,
            password: None,
            incremental: Some(Incremental { state_file: base.join("stand.json"), full_every: 3 }),
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        // Zeitstempel im Dateinamen sind sekundengenau: zwischen den Läufen kurz warten.
        let pause = || std::thread::sleep(Duration::from_millis(1100));

        // 1. Lauf: Vollsicherung. Gleich danach hat sich nichts geändert.
        let full = backup::run(&job, &rep).unwrap();
        assert_eq!((full.kind, full.files), (BackupKind::Full, 3));
        let full_path = full.local_path.unwrap();
        let unchanged = backup::run(&job, &rep).unwrap();
        assert_eq!((unchanged.kind, unchanged.files, unchanged.saved.len()), (BackupKind::Unchanged, 0, 0));
        assert_eq!(fs::read_dir(&target).unwrap().count(), 1);

        // 2. Lauf: eine Datei geändert, eine neu – nur diese beiden landen in der Teilsicherung.
        pause();
        write(&docs.join("a.txt"), b"eins, aber neuer");
        write(&docs.join("neu.txt"), b"vier");
        let first = backup::run(&job, &rep).unwrap();
        assert_eq!((first.kind, first.files), (BackupKind::Partial, 2));
        let first_path = first.local_path.unwrap();
        assert!(first_path.to_string_lossy().ends_with("_teil.zip"));
        let manifest = inspect(&first_path).unwrap().manifest.unwrap();
        assert!(manifest.incremental);
        assert_eq!(manifest.base.as_deref(), full_path.file_name().and_then(|n| n.to_str()));

        // 3. Lauf: noch eine Änderung.
        pause();
        write(&docs.join("b.txt"), b"zwei, aber neuer");
        let second = backup::run(&job, &rep).unwrap();
        assert_eq!((second.kind, second.files), (BackupKind::Partial, 1));
        let second_path = second.local_path.unwrap();

        // Die Kette zur letzten Teilsicherung: Vollsicherung und beide Teilsicherungen.
        let info = inspect(&second_path).unwrap();
        let chain = chain_for(&info).expect("Kette vollständig");
        assert_eq!(chain, [full_path.clone(), first_path.clone(), second_path.clone()]);
        assert!(chain_for(&inspect(&full_path).unwrap()).is_none());

        // Zusammen wiederhergestellt gilt von jeder Datei der neueste Stand.
        let out = base.join("zurueck");
        let template = RestoreJob {
            archive: second_path.clone(),
            mapping: Mapping::Roots(HashMap::from([("Dokumente".to_string(), out.clone())])),
            conflict: Conflict::Skip,
            total_bytes: 0,
            password: None,
            only: None,
        };
        let summary = run_chain(&chain, &template, &rep).unwrap();
        assert_eq!((summary.restored, summary.failed, summary.skipped_existing), (4, 0, 0));
        assert_same_tree(&docs, &out);

        // 4. Lauf: Jede dritte Sicherung ist wieder vollständig.
        pause();
        let again = backup::run(&job, &rep).unwrap();
        assert_eq!((again.kind, again.files), (BackupKind::Full, 4));

        // Aufräumen mit "1 behalten": die alte Vollsicherung geht samt ihren Teilsicherungen.
        assert_eq!(crate::jobs::prune(&job, 1, &rep), 3);
        let left: Vec<PathBuf> = fs::read_dir(&target).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(left, [again.local_path.unwrap()]);
        // Ohne die Vollsicherung daneben gibt es keine Kette mehr.
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn single_files_and_subfolders_can_be_picked() {
        let base = temp_dir("pick");
        let (zip_path, _source) = make_backup(&base, Compression::Fast);
        let info = inspect(&zip_path).unwrap();
        let manifest = info.manifest.clone().unwrap();

        // Die Baumansicht kennt Ordner und Dateien mit ihren Summen, aber nicht die Inhaltsbeschreibung.
        let entries = list_entries(&zip_path).unwrap();
        assert_eq!(entries.len(), 4);
        let tree = Tree::build(&entries);
        let top: Vec<(&str, bool, u64)> = tree.children("").iter().map(|n| (n.name.as_str(), n.dir, n.files)).collect();
        assert_eq!(top, [("Dokumente", true, 3), ("Fotos Übersee", true, 1)]);
        let docs: Vec<(&str, bool)> = tree.children("Dokumente").iter().map(|n| (n.name.as_str(), n.dir)).collect();
        assert_eq!(docs, [("Unterordner", true), ("brief.txt", false), ("leer.txt", false)]);

        // Auswahl: ein Unterordner und eine einzelne Datei aus einem anderen Ordner.
        let only: HashSet<String> =
            ["Dokumente/Unterordner".to_string(), "Fotos Übersee/bild.jpg".to_string()].into_iter().collect();
        assert_eq!(tree.total(&only), (2, 300_012));
        assert!(is_selected(&only, "Dokumente/Unterordner/tief/tabelle.csv"));
        assert!(!is_selected(&only, "Dokumente/Unterordner-alt/x.txt") && !is_selected(&only, "Dokumente/brief.txt"));

        let out = base.join("auswahl");
        let mapping = manifest.roots.iter().map(|r| (r.name.clone(), out.join(&r.name))).collect();
        let job = RestoreJob {
            archive: zip_path,
            mapping: Mapping::Roots(mapping),
            conflict: Conflict::Skip,
            total_bytes: 300_012,
            password: None,
            only: Some(only),
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = run(&job, &rep).unwrap();
        assert_eq!((summary.restored, summary.failed), (2, 0));
        assert!(out.join("Dokumente").join("Unterordner").join("tief").join("tabelle.csv").is_file());
        assert!(out.join("Fotos Übersee").join("bild.jpg").is_file());
        assert!(!out.join("Dokumente").join("brief.txt").exists());
        assert!(!out.join("Dokumente").join("Leerer Ordner").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn restore_respects_selection_and_conflict_mode() {
        let base = temp_dir("conflict");
        let (zip_path, _source) = make_backup(&base, Compression::Fast);
        let info = inspect(&zip_path).unwrap();
        let out = base.join("teil");
        let existing = out.join("brief.txt");
        write(&existing, b"lokale Version");

        // Nur "Dokumente" auswählen; vorhandene Datei bleibt beim Überspringen erhalten.
        let mapping = Mapping::Roots(HashMap::from([("Dokumente".to_string(), out.clone())]));
        let mut job = RestoreJob {
            archive: zip_path,
            mapping,
            conflict: Conflict::Skip,
            total_bytes: info.total_bytes,
                password: None,
            only: None,
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = run(&job, &rep).unwrap();
        assert_eq!((summary.restored, summary.skipped_existing), (2, 1));
        assert_eq!(fs::read(&existing).unwrap(), b"lokale Version");
        assert!(!out.join("bild.jpg").exists());
        assert!(out.join("Leerer Ordner").is_dir());

        // Überschreiben ersetzt auch schreibgeschützte Dateien.
        let mut permissions = fs::metadata(&existing).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&existing, permissions).unwrap();
        job.conflict = Conflict::Overwrite;
        let summary = run(&job, &rep).unwrap();
        assert_eq!((summary.restored, summary.failed), (3, 0));
        assert!(fs::read(&existing).unwrap().starts_with("Sehr geehrte".as_bytes()));
        assert!(!out.join("brief.txt.pbtmp").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn cancelled_backup_leaves_no_file_behind() {
        let base = temp_dir("cancel-backup");
        let source = base.join("quelle");
        write(&source.join("a.txt"), b"abc");
        let target = base.join("ziel");
        let job = BackupJob {
            profile_name: "anna".into(),
            profile_dir: base.clone(),
            sources: vec![Source::plain(source, None)],
            destinations: vec![Destination::folder(target.clone())],
            compression: Compression::Normal,
            skip_online_only: true,
            verify: true,
            password: None,
            incremental: None,
        };
        let (rep, _rx, cancel) = Reporter::new(|| {});
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = backup::run(&job, &rep).err().unwrap();
        assert!(err.is::<Cancelled>());
        // Abgebrochen wird schon vor dem Anlegen des Zielordners; liegt er doch da, ist er leer.
        assert!(fs::read_dir(&target).map(|entries| entries.count() == 0).unwrap_or(true));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn targets_follow_the_chosen_profile() {
        let base = temp_dir("targets");
        let (zip_path, source) = make_backup(&base, Compression::None);
        let manifest = inspect(&zip_path).unwrap().manifest.unwrap();
        assert_eq!(manifest.user, "anna");
        assert!(zip_path.file_name().unwrap().to_string_lossy().contains("_anna_"));

        let other = Profile {
            name: "bernd".into(),
            dir: PathBuf::from("C:\\Users\\bernd"),
            current: false,
            accessible: true,
            folders: Vec::new(),
            optional: Vec::new(),
        };
        // Eigener Ordner aus dem alten Profil → gleiche Stelle im gewählten Profil.
        assert_eq!(target_for(&manifest, &manifest.roots[0], &other), PathBuf::from("C:\\Users\\bernd\\Dokumente"));
        // Standardordner → Standardordner des gewählten Profils.
        let mut pictures = manifest.roots[1].clone();
        pictures.kind = Some("pictures".into());
        assert_eq!(target_for(&manifest, &pictures, &other), PathBuf::from("C:\\Users\\bernd\\Pictures"));
        // Ordner außerhalb des alten Profils bleibt am alten Pfad.
        let mut outside = manifest.roots[0].clone();
        outside.original_path = "D:\\Projekte".into();
        assert_eq!(target_for(&manifest, &outside, &other), PathBuf::from("D:\\Projekte"));
        let _ = (source, fs::remove_dir_all(base));
    }

    #[test]
    fn verification_detects_damage() {
        use crate::verify::verify_archive;
        let base = temp_dir("verify");
        // Unkomprimiert sichern, damit sich der Dateiinhalt in der ZIP-Datei wiederfinden lässt.
        let (zip_path, _source) = make_backup(&base, Compression::None);
        let (rep, _rx, _cancel) = Reporter::new(|| {});

        // Unbeschädigt: alle vier Dateien werden gelesen, die Zahlen passen.
        let manifest = inspect(&zip_path).unwrap().manifest.unwrap();
        let expected: (u64, u64) =
            (manifest.roots.iter().map(|r| r.files).sum(), manifest.roots.iter().map(|r| r.bytes).sum());
        let report = verify_archive(&zip_path, Some(expected), None, &rep).unwrap();
        assert_eq!((report.files, report.bytes), (4, 320_012));
        // Falsche Erwartung = unvollständige Sicherung.
        let err = verify_archive(&zip_path, Some((5, 0)), None, &rep).err().unwrap();
        assert!(err.to_string().contains("unvollständig"), "{err}");

        // Ein Byte mitten im Brief verändern: die Prüfsumme passt nicht mehr.
        let mut data = fs::read(&zip_path).unwrap();
        let needle = "Sehr geehrte Damen".as_bytes();
        let position = data.windows(needle.len()).position(|w| w == needle).expect("Inhalt nicht gefunden");
        data[position + 5000] ^= 0xFF;
        fs::write(&zip_path, &data).unwrap();
        let err = verify_archive(&zip_path, None, None, &rep).err().unwrap();
        assert!(err.to_string().contains("brief.txt") && err.to_string().contains("beschädigt"), "{err}");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn encrypted_backup_needs_the_password() {
        use crate::verify::verify_archive;
        const PASSWORD: &str = "Geheim-2026!";
        let base = temp_dir("encrypted");
        // Unkomprimiert, damit Klartext in der ZIP-Datei auffallen würde.
        let (zip_path, source) = make_backup_with(&base, Compression::None, Some(PASSWORD));
        let (rep, _rx, _cancel) = Reporter::new(|| {});

        // Der Inhalt steht nicht im Klartext in der Datei.
        let raw = fs::read(&zip_path).unwrap();
        let needle = "Sehr geehrte Damen".as_bytes();
        assert!(!raw.windows(needle.len()).any(|w| w == needle), "Klartext in verschlüsselter Sicherung");

        // Die Inhaltsbeschreibung bleibt lesbar und meldet die Verschlüsselung.
        let info = inspect(&zip_path).unwrap();
        assert!(info.encrypted);
        let manifest = info.manifest.clone().unwrap();
        assert!(manifest.encrypted);

        assert!(check_password(&zip_path, PASSWORD).unwrap());
        assert!(!check_password(&zip_path, "falsch").unwrap());

        // Prüfen: mit Passwort in Ordnung, ohne oder mit falschem Passwort ein klarer Fehler.
        assert_eq!(verify_archive(&zip_path, None, Some(PASSWORD), &rep).unwrap().files, 4);
        let err = verify_archive(&zip_path, None, Some("falsch"), &rep).err().unwrap();
        assert!(err.to_string().contains("Passwort"), "{err}");
        let err = verify_archive(&zip_path, None, None, &rep).err().unwrap();
        assert!(err.to_string().contains("verschlüsselt"), "{err}");

        // Wiederherstellen: ohne Passwort bricht es ab, mit Passwort stimmt alles überein.
        let out = base.join("wiederhergestellt");
        let mapping: HashMap<String, PathBuf> =
            manifest.roots.iter().map(|r| (r.name.clone(), out.join(&r.name))).collect();
        let mut job = RestoreJob {
            archive: zip_path,
            mapping: Mapping::Roots(mapping),
            conflict: Conflict::Skip,
            total_bytes: info.total_bytes,
                password: None,
            only: None,
        };
        let err = run(&job, &rep).err().unwrap();
        assert!(err.to_string().contains("verschlüsselt"), "{err}");
        job.password = Some("falsch".into());
        assert!(run(&job, &rep).err().unwrap().to_string().contains("Passwort"));
        job.password = Some(PASSWORD.into());
        let summary = run(&job, &rep).unwrap();
        assert_eq!((summary.restored, summary.failed), (4, 0));
        assert_same_tree(&source, &out);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn named_source_skips_excluded_folders() {
        let base = temp_dir("exclude");
        let data = base.join("profil").join("AppData").join("Local").join("Google").join("Chrome").join("User Data");
        write(&data.join("Default").join("Bookmarks"), b"{}");
        write(&data.join("Default").join("Cache").join("data_0"), &[7u8; 5000]);
        write(&data.join("Default").join("Code Cache").join("js").join("x"), b"zwischenspeicher");
        write(&data.join("Local State"), b"{}");

        let job = BackupJob {
            profile_name: "anna".into(),
            profile_dir: base.join("profil"),
            sources: vec![Source {
                path: data,
                kind: Some("chrome".into()),
                name: Some("Chrome".into()),
                exclude: vec!["cache".into(), "code cache".into()],
                exclude_paths: Vec::new(),
            }],
            destinations: vec![Destination::folder(base.join("ziel"))],
                        compression: Compression::Fast,
            skip_online_only: true,
            verify: true,
            password: None,
            incremental: None,
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = backup::run(&job, &rep).unwrap();
        assert_eq!(summary.files, 2);

        let info = inspect(&summary.local_path.unwrap()).unwrap();
        let manifest = info.manifest.unwrap();
        assert_eq!(manifest.roots[0].name, "Chrome");

        // Zurück in ein anderes Profil: gleicher Platz unter AppData.
        let other = Profile {
            name: "bernd".into(),
            dir: base.join("neu"),
            current: false,
            accessible: true,
            folders: Vec::new(),
            optional: Vec::new(),
        };
        let target = target_for(&manifest, &manifest.roots[0], &other);
        assert_eq!(target, base.join("neu").join("AppData").join("Local").join("Google").join("Chrome").join("User Data"));

        let job = RestoreJob {
            archive: info.path,
            mapping: Mapping::Roots(HashMap::from([("Chrome".to_string(), target.clone())])),
            conflict: Conflict::Skip,
            total_bytes: info.total_bytes,
                password: None,
            only: None,
        };
        assert_eq!(run(&job, &rep).unwrap().restored, 2);
        assert!(target.join("Default").join("Bookmarks").is_file());
        assert!(!target.join("Default").join("Cache").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn backup_is_written_to_every_folder_target() {
        let base = temp_dir("targets-many");
        let source = base.join("quelle");
        write(&source.join("a.txt"), "Inhalt".repeat(2000).as_bytes());
        let (first, second, third) = (base.join("usb"), base.join("nas"), base.join("zweitplatte"));
        let job = BackupJob {
            profile_name: "anna".into(),
            profile_dir: base.clone(),
            sources: vec![Source::plain(source, None)],
            destinations: vec![
                Destination::Folder { name: "USB-Stick".into(), path: first.clone(), login: None },
                Destination::Folder { name: "NAS".into(), path: second.clone(), login: None },
                Destination::Folder { name: "Zweitplatte".into(), path: third.clone(), login: None },
            ],
            compression: Compression::Normal,
            skip_online_only: true,
            verify: true,
            password: None,
            incremental: None,
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = backup::run(&job, &rep).unwrap();

        // Die ZIP-Datei liegt in allen drei Ordnern, bitgleich und ohne .part-Reste.
        let zip = summary.local_path.clone().unwrap();
        assert!(zip.starts_with(&first));
        let name = zip.file_name().unwrap();
        let original = fs::read(&zip).unwrap();
        for dir in [&first, &second, &third] {
            assert_eq!(fs::read_dir(dir).unwrap().count(), 1, "{}", dir.display());
            assert_eq!(fs::read(dir.join(name)).unwrap(), original);
        }
        assert_eq!(summary.saved.len(), 3);
        assert!(summary.saved[1].starts_with("NAS: "), "{:?}", summary.saved);

        // Ohne Ziel gibt es eine klare Absage.
        let empty = BackupJob { destinations: Vec::new(), ..job };
        assert!(backup::run(&empty, &rep).err().unwrap().to_string().contains("kein Ziel"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn drive_backup_leaves_out_system_folders() {
        // Nachgebautes Laufwerk mit Windows, Programmen, Auslagerungsdatei und Benutzerdaten.
        let base = temp_dir("drive");
        let root = base.join("laufwerk");
        write(&root.join("Windows").join("System32").join("kernel32.dll"), &[1u8; 4000]);
        write(&root.join("Program Files").join("App").join("app.exe"), &[2u8; 3000]);
        write(&root.join("pagefile.sys"), &[0u8; 9000]);
        write(&root.join("$Recycle.Bin").join("alt.txt"), b"weg");
        write(&root.join("Users").join("anna").join("AppData").join("Local").join("Temp").join("tmp.bin"), &[3u8; 500]);
        write(&root.join("Users").join("anna").join("Documents").join("brief.txt"), b"wichtig");
        write(&root.join("Daten").join("tabelle.csv"), b"1;2;3");
        let target = root.join("Sicherungen");
        write(&target.join("alte-sicherung.zip"), &[9u8; 7000]);

        let excludes_for = |data_only: bool| {
            let mut paths = backup::drive_excludes(&root, data_only);
            paths.push(target.clone()); // der Zielordner sichert sich nicht selbst
            paths
        };
        let run_backup = |data_only: bool| {
            let job = BackupJob {
                profile_name: "Laufwerk-X".into(),
                profile_dir: PathBuf::new(),
                sources: vec![Source {
                    path: root.clone(),
                    kind: Some("drive".into()),
                    name: Some("Laufwerk X".into()),
                    exclude: Vec::new(),
                    exclude_paths: excludes_for(data_only),
                }],
                destinations: vec![Destination::folder(target.clone())],
                                compression: Compression::Fast,
                skip_online_only: true,
                verify: true,
                password: None,
            incremental: None,
            };
            let (rep, _rx, _cancel) = Reporter::new(|| {});
            backup::run(&job, &rep).unwrap()
        };

        // Nur Daten: Brief und Tabelle, sonst nichts.
        let summary = run_backup(true);
        assert_eq!((summary.files, summary.bytes), (2, 12));
        let info = inspect(&summary.local_path.clone().unwrap()).unwrap();
        assert!(info.is_drive_backup() && !info.fits_profile());
        assert_eq!(info.manifest.unwrap().roots[0].name, "Laufwerk X");
        fs::remove_file(summary.local_path.unwrap()).unwrap();

        // Alles: zusätzlich Windows und Programme, aber weiterhin ohne Auslagerungsdatei,
        // Papierkorb, Temp-Ordner und den Zielordner.
        let summary = run_backup(false);
        assert_eq!((summary.files, summary.bytes), (4, 12 + 4000 + 3000));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn foreign_zip_is_extracted_as_is() {
        use zip::write::SimpleFileOptions;
        let base = temp_dir("foreign");
        let zip_path = base.join("fremd.zip");
        let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        zip.start_file("ordner/datei.txt", SimpleFileOptions::default()).unwrap();
        zip.write_all(b"hallo").unwrap();
        zip.finish().unwrap();

        let info = inspect(&zip_path).unwrap();
        assert!(info.manifest.is_none());
        let out = base.join("aus");
        let job = RestoreJob {
            archive: zip_path,
            mapping: Mapping::Everything(out.clone()),
            conflict: Conflict::Overwrite,
            total_bytes: info.total_bytes,
                password: None,
            only: None,
        };
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        assert_eq!(run(&job, &rep).unwrap().restored, 1);
        assert_eq!(fs::read(out.join("ordner").join("datei.txt")).unwrap(), b"hallo");
        let _ = fs::remove_dir_all(base);
    }
}
