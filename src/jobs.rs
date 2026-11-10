//! Gespeicherte Aufträge ausführen: Ziele auflösen, Sicherung starten, alte Sicherungen
//! aufräumen und den Verlauf führen – mit Oberfläche oder ohne (`--run-job`).

use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use crate::backup::{self, BackupJob, BackupKind, BackupSummary, Destination, Incremental, SmbLogin};
use crate::config::{self, Config, ReportMode, SavedJob, Target, TargetKind};
use crate::mail::Mail;
use crate::job::{Msg, Reporter};
use crate::nextcloud::{NcSettings, RemoteFile};
use crate::remote::RemoteSettings;
use crate::s3::S3Settings;
use crate::sftp::SftpSettings;
use crate::util::{fmt_bytes, fmt_duration};

/// So viele Einträge behält der Verlauf.
const MAX_HISTORY: usize = 200;

// ---------------------------------------------------------------------------- Ziele

/// Ist das Ziel vollständig eingerichtet? Sonst eine Meldung, was fehlt.
pub fn check_target(target: &Target, password: &str) -> Result<(), String> {
    let missing = |what: &str| {
        Err(tr!(
            "Ziel „{}“ ist unvollständig\n{what} fehlt. Bitte unter „Sicherungsziele“ ergänzen.",
            "Target “{}” is incomplete\n{what} is missing. Please add it under “Backup targets”.",
            target.name
        ))
    };
    match target.kind {
        TargetKind::Local | TargetKind::Smb if target.path.trim().is_empty() => missing(t!("Der Pfad", "The path")),
        TargetKind::Smb if !target.path.trim().starts_with("\\\\") => Err(tr!(
            "Ziel „{}“ ist ungültig\nEine Netzwerkfreigabe beginnt mit \\\\server\\freigabe.",
            "Target “{}” is invalid\nA network share starts with \\\\server\\share.",
            target.name
        )),
        kind if kind.is_cloud() && target.url.trim().is_empty() => {
            missing(t!("Die Server-Adresse", "The server address"))
        }
        TargetKind::S3 if target.path.trim().is_empty() => missing(t!("Der Bucket", "The bucket")),
        TargetKind::S3 if target.user.trim().is_empty() => missing(t!("Der Zugangsschlüssel", "The access key")),
        TargetKind::S3 if password.is_empty() => missing(t!("Der Geheimschlüssel", "The secret key")),
        kind if kind.is_cloud() && target.user.trim().is_empty() => missing(t!("Der Benutzername", "The user name")),
        _ if target.needs_password() && password.is_empty() => missing(t!("Das Passwort", "The password")),
        _ => Ok(()),
    }
}

/// Zugangsdaten eines Cloud-Ziels (Nextcloud, WebDAV, S3); `None` bei Ordnern und Freigaben.
pub fn remote_settings(target: &Target, password: &str) -> Result<Option<RemoteSettings>, String> {
    check_target(target, password)?;
    let dav = || NcSettings {
        url: target.url.trim().to_string(),
        user: target.user.trim().to_string(),
        password: password.to_string(),
        remote_dir: target.remote_dir.trim().to_string(),
    };
    Ok(match target.kind {
        TargetKind::Nextcloud => Some(RemoteSettings::Nextcloud(dav())),
        TargetKind::WebDav => Some(RemoteSettings::WebDav(dav())),
        TargetKind::S3 => Some(RemoteSettings::S3(S3Settings {
            endpoint: target.url.trim().to_string(),
            region: target.region.trim().to_string(),
            bucket: target.path.trim().to_string(),
            prefix: target.remote_dir.trim().to_string(),
            access_key: target.user.trim().to_string(),
            secret_key: password.to_string(),
        })),
        TargetKind::Sftp => Some(RemoteSettings::Sftp(SftpSettings {
            host: target.url.trim().to_string(),
            user: target.user.trim().to_string(),
            password: password.to_string(),
            key_file: target.path.trim().to_string(),
            remote_dir: target.remote_dir.trim().to_string(),
        })),
        TargetKind::Local | TargetKind::Smb => None,
    })
}

/// Macht aus einem eingerichteten Ziel das, was die Sicherung braucht.
pub fn destination(target: &Target, password: &str) -> Result<Destination, String> {
    if let Some(settings) = remote_settings(target, password)? {
        return Ok(Destination::Cloud { name: target.name.clone(), settings });
    }
    Ok(match target.kind {
        TargetKind::Nextcloud | TargetKind::WebDav | TargetKind::S3 | TargetKind::Sftp => {
            unreachable!("Cloud-Ziele sind oben behandelt")
        }
        TargetKind::Local => {
            let path = PathBuf::from(target.path.trim());
            if !path.is_dir() {
                return Err(tr!(
                    "Ziel „{}“ ist nicht erreichbar\n{} – ist der USB-Stick bzw. das Laufwerk angeschlossen?",
                    "Target “{}” cannot be reached\n{} – is the USB stick or drive connected?",
                    target.name,
                    path.display()
                ));
            }
            Destination::Folder { name: target.name.clone(), path, login: None }
        }
        TargetKind::Smb => Destination::Folder {
            name: target.name.clone(),
            path: PathBuf::from(target.path.trim()),
            login: target
                .needs_password()
                .then(|| SmbLogin { user: target.user.trim().to_string(), password: password.to_string() }),
        },
    })
}

/// Alle Ziele in der Reihenfolge, in der gesichert wird: Ordner vor Freigaben vor Nextcloud,
/// damit die ZIP-Datei möglichst direkt auf einem Datenträger entsteht.
pub fn destinations(targets: &[&Target], password_of: &dyn Fn(&Target) -> String) -> Result<Vec<Destination>, String> {
    let mut result = Vec::new();
    for kind in [TargetKind::Local, TargetKind::Smb, TargetKind::Nextcloud, TargetKind::WebDav, TargetKind::S3, TargetKind::Sftp] {
        for target in targets.iter().filter(|t| t.kind == kind) {
            result.push(destination(target, &password_of(target))?);
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------- Sicherungen finden

/// Wo eine gefundene Sicherung liegt.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// Direkt lesbar: Ordner, USB-Stick oder Netzwerkfreigabe
    File(PathBuf),
    /// Auf einer Nextcloud; muss erst heruntergeladen werden
    Cloud(RemoteFile),
}

/// Eine Sicherung auf einem der Sicherungsziele.
#[derive(Clone, Debug, PartialEq)]
pub struct FoundBackup {
    pub target_id: u64,
    pub target_name: String,
    pub name: String,
    pub size: u64,
    pub modified: Option<DateTime<Local>>,
    pub place: Place,
}

/// Sucht auf allen Zielen nach ZIP-Sicherungen, neueste zuerst. Ein Ziel, das gerade nicht
/// erreichbar ist (USB-Stick nicht angesteckt), wird gemeldet und übersprungen.
pub fn find_backups(targets: &[(Target, String)], rep: &Reporter) -> Vec<FoundBackup> {
    let mut found = Vec::new();
    for (target, password) in targets {
        if rep.cancelled() {
            break;
        }
        rep.status(tr!("Suche auf „{}“ …", "Searching “{}” …", target.name));
        let result: Result<Vec<FoundBackup>> = (|| {
            let hit = |name: String, size: u64, modified, place| FoundBackup {
                target_id: target.id,
                target_name: target.name.clone(),
                name,
                size,
                modified,
                place,
            };
            match destination(target, password).map_err(|m| anyhow::anyhow!(m.replace('\n', " – ")))? {
                Destination::Cloud { settings, .. } => {
                    Ok(settings
                        .connect()?
                        .list()?
                        .into_iter()
                        .map(|file| hit(file.name.clone(), file.size, file.modified, Place::Cloud(file)))
                        .collect())
                }
                Destination::Folder { path, login, .. } => {
                    if let Some(login) = &login {
                        crate::smb::connect(&path.to_string_lossy(), &login.user, &login.password)?;
                    }
                    let mut files = Vec::new();
                    for entry in fs::read_dir(&path)?.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        let Ok(meta) = entry.metadata() else { continue };
                        if meta.is_file() && name.to_ascii_lowercase().ends_with(".zip") {
                            let modified = meta.modified().ok().map(DateTime::<Local>::from);
                            files.push(hit(name, meta.len(), modified, Place::File(entry.path())));
                        }
                    }
                    Ok(files)
                }
            }
        })();
        match result {
            Ok(files) => found.extend(files),
            Err(e) => rep.warn(tr!(
                "Ziel „{}“ wurde nicht durchsucht: {e:#}",
                "Target “{}” was not searched: {e:#}",
                target.name
            )),
        }
    }
    found.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
    found
}

// ---------------------------------------------------------------------------- Auftrag ausführen

/// Baut aus einem gespeicherten Auftrag die Sicherung. `password_of` liefert die Passwörter
/// der Ziele, `job_password` das Passwort für die Verschlüsselung.
pub fn build(
    saved: &SavedJob,
    cfg: &Config,
    password_of: &dyn Fn(&Target) -> String,
    job_password: Option<String>,
) -> Result<BackupJob, String> {
    let targets: Vec<&Target> =
        saved.target_ids.iter().filter_map(|id| cfg.targets.iter().find(|t| t.id == *id)).collect();
    if targets.is_empty() || targets.len() != saved.target_ids.len() {
        return Err(tr!(
            "Auftrag „{}“ hat kein gültiges Ziel\nEin Sicherungsziel dieses Auftrags fehlt oder wurde gelöscht. Bitte den Auftrag neu anlegen.",
            "Job “{}” has no valid target\nA backup target of this job is missing or was deleted. Please create the job again.",
            saved.name
        ));
    }
    if saved.sources.is_empty() {
        return Err(tr!("Auftrag „{}“ enthält keine Ordner", "Job “{}” contains no folders", saved.name));
    }
    let mut sources = saved.sources.clone();
    // Liegt ein Zielordner auf dem gesicherten Laufwerk, darf er sich nicht selbst sichern.
    for source in sources.iter_mut().filter(|s| s.kind.as_deref() == Some("drive")) {
        for target in targets.iter().filter(|t| t.kind == TargetKind::Local) {
            let path = PathBuf::from(target.path.trim().trim_end_matches('\\'));
            if !path.as_os_str().is_empty() && !source.exclude_paths.contains(&path) {
                source.exclude_paths.push(path);
            }
        }
    }
    let password = match (saved.encrypt, job_password) {
        (false, _) => None,
        (true, Some(password)) if !password.is_empty() => Some(password),
        (true, _) => {
            return Err(tr!(
                "Passwort für Auftrag „{}“ fehlt\nDas Verschlüsselungs-Passwort ist nicht mehr gespeichert. Bitte den Auftrag neu anlegen.",
                "Password for job “{}” is missing\nThe encryption password is no longer stored. Please create the job again.",
                saved.name
            ));
        }
    };
    Ok(BackupJob {
        profile_name: saved.profile_name.clone(),
        profile_dir: saved.profile_dir.clone(),
        sources,
        destinations: destinations(&targets, password_of)?,
        compression: saved.compression,
        skip_online_only: saved.skip_online_only,
        verify: saved.verify,
        password,
        incremental: match (saved.incremental, state_path(saved.id)) {
            (true, Some(state_file)) => Some(Incremental { state_file, full_every: saved.full_every.max(2) }),
            _ => None,
        },
    })
}

/// Datei, in der sich ein Auftrag den Stand seiner letzten Sicherung merkt (für Teilsicherungen).
pub fn state_path(job_id: u64) -> Option<PathBuf> {
    Some(Config::data_dir()?.join("staende").join(format!("auftrag-{job_id}.json")))
}

/// Der E-Mail-Bericht, falls einer eingerichtet ist: wann er gesendet wird und wie.
pub type MailReport = Option<(ReportMode, Mail)>;

/// Liest den E-Mail-Bericht aus den Einstellungen; das Passwort kommt aus der
/// Windows-Anmeldeinformationsverwaltung.
pub fn mail_report(cfg: &Config) -> MailReport {
    let settings = &cfg.mail;
    if settings.mode == ReportMode::Never || settings.server.trim().is_empty() {
        return None;
    }
    let password = settings.password_key().and_then(|key| config::load_password(&key)).unwrap_or_default();
    Some((settings.mode, mail_from(settings, password)))
}

pub fn mail_from(settings: &config::MailSettings, password: String) -> Mail {
    Mail {
        server: settings.server.trim().to_string(),
        port: settings.port,
        starttls: settings.starttls,
        user: settings.user.trim().to_string(),
        password,
        from: settings.from.trim().to_string(),
        to: settings.to.trim().to_string(),
    }
}

/// Betreff und Text des Berichts über einen Lauf.
pub fn report_text(job_name: &str, ok: bool, text: &str, saved: &[String]) -> (String, String) {
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
    let subject = tr!(
        "Packup: Sicherung „{job_name}“ {} ({computer})",
        "Packup: backup “{job_name}” {} ({computer})",
        if ok { t!("gelungen", "succeeded") } else { t!("FEHLGESCHLAGEN", "FAILED") }
    );
    let mut body = tr!(
        "Auftrag: {job_name}\nComputer: {computer}\nZeit: {}\nErgebnis: {}\n\n{text}\n",
        "Job: {job_name}\nComputer: {computer}\nTime: {}\nResult: {}\n\n{text}\n",
        Local::now().format("%d.%m.%Y %H:%M"),
        if ok { t!("gelungen", "succeeded") } else { t!("fehlgeschlagen", "failed") }
    );
    if !saved.is_empty() {
        body.push_str(t!("\nGespeichert:\n", "\nSaved to:\n"));
        for place in saved {
            body.push_str(&format!("- {place}\n"));
        }
    }
    body.push_str(t!(
        "\nDiese Nachricht hat Packup automatisch gesendet.\n",
        "\nThis message was sent automatically by Packup.\n"
    ));
    (subject, body)
}

/// Führt den Auftrag aus: sichern, danach aufräumen, Ergebnis im Verlauf festhalten und –
/// falls eingerichtet – per E-Mail berichten.
pub fn execute(saved: &SavedJob, job: &BackupJob, mail: &MailReport, rep: &Reporter) -> Result<BackupSummary> {
    let result = backup::run(job, rep);
    if result.is_ok() && saved.keep_last > 0 {
        rep.status(t!("Alte Sicherungen werden aufgeräumt …", "Cleaning up old backups …"));
        let removed = prune(job, saved.keep_last, rep);
        if removed > 0 {
            rep.log(tr!(
                "Aufgeräumt: {removed} alte Sicherung(en) gelöscht, die letzten {} bleiben.",
                "Cleaned up: {removed} old backup(s) deleted, the last {} are kept.",
                saved.keep_last
            ));
        }
    }
    let text = match &result {
        Ok(s) if s.kind == BackupKind::Unchanged => {
            t!("Keine Änderungen seit der letzten Sicherung", "No changes since the last backup").to_string()
        }
        Ok(s) => tr!(
            "{}{} Dateien ({}) als ZIP mit {} in {}",
            "{}{} files ({}) as a {} ZIP in {}",
            if s.kind == BackupKind::Partial { t!("Teilsicherung: ", "Partial backup: ") } else { "" },
            s.files,
            fmt_bytes(s.bytes),
            fmt_bytes(s.zip_size),
            fmt_duration(s.duration)
        ),
        Err(e) => format!("{e:#}"),
    };
    // Ein abgebrochener Lauf ist kein Fehler, über den berichtet werden müsste.
    let cancelled = result.as_ref().is_err_and(|e| e.is::<crate::job::Cancelled>());
    if let Some((mode, mail)) = mail
        && mode.applies(result.is_ok())
        && !cancelled
    {
        rep.status(t!("Bericht wird per E-Mail gesendet …", "Sending report by e-mail …"));
        let places = result.as_ref().map(|s| s.saved.clone()).unwrap_or_default();
        let (subject, body) = report_text(&saved.name, result.is_ok(), &text, &places);
        match crate::mail::send(mail, &subject, &body) {
            Ok(()) => rep.log(tr!("Bericht per E-Mail gesendet an {}.", "Report sent by e-mail to {}.", mail.to)),
            Err(e) => rep.warn(tr!(
                "Der E-Mail-Bericht wurde nicht gesendet: {e:#}",
                "The e-mail report was not sent: {e:#}"
            )),
        }
    }
    record(RunRecord {
        job_id: saved.id,
        job_name: saved.name.clone(),
        finished: Local::now().format("%d.%m.%Y %H:%M").to_string(),
        ok: result.is_ok(),
        text,
    });
    result
}

/// Löscht an jedem Ziel alle Sicherungen dieses Profils bis auf die `keep` neuesten.
/// Angefasst werden nur Dateien, deren Name genau dem Muster der Sicherungen entspricht.
/// Fehler beim Aufräumen werden gemeldet, lassen die Sicherung aber nicht scheitern.
pub fn prune(job: &BackupJob, keep: u32, rep: &Reporter) -> u32 {
    let prefix = backup::archive_prefix(&job.profile_name);
    let keep = keep.max(1) as usize;
    // Gezählt werden Vollsicherungen. Alles, was älter ist als die `keep`-te Vollsicherung,
    // wird gelöscht – auch Teilsicherungen, die ohne ihre Vollsicherung wertlos wären.
    let surplus = |names: Vec<String>| -> Vec<String> {
        let mut found: Vec<(String, bool, String)> = names
            .into_iter()
            .filter_map(|name| {
                let (stamp, partial) = backup::archive_stamp(&name, &prefix)?;
                Some((stamp.to_string(), partial, name))
            })
            .collect();
        // Der Zeitstempel sortiert sich von selbst: absteigend = neueste zuerst.
        found.sort_by(|a, b| b.0.cmp(&a.0));
        let Some(limit) = found.iter().filter(|(_, partial, _)| !partial).nth(keep - 1).map(|(stamp, _, _)| stamp.clone())
        else {
            return Vec::new();
        };
        found.into_iter().filter(|(stamp, _, _)| *stamp < limit).map(|(_, _, name)| name).collect()
    };
    let mut removed = 0;
    for destination in &job.destinations {
        match destination {
            Destination::Folder { name, path, .. } => {
                let names = match fs::read_dir(path) {
                    Ok(entries) => entries
                        .flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect(),
                    Err(e) => {
                        rep.warn(tr!("Aufräumen auf „{name}“ nicht möglich: {e}", "Cannot clean up on “{name}”: {e}"));
                        continue;
                    }
                };
                for old in surplus(names) {
                    match fs::remove_file(path.join(&old)) {
                        Ok(()) => {
                            rep.log(tr!("Alte Sicherung gelöscht („{name}“): {old}", "Old backup deleted (“{name}”): {old}"));
                            removed += 1;
                        }
                        Err(e) => rep.warn(tr!(
                            "Alte Sicherung {old} auf „{name}“ nicht gelöscht: {e}",
                            "Old backup {old} on “{name}” was not deleted: {e}"
                        )),
                    }
                }
            }
            Destination::Cloud { name, settings } => {
                let listed = settings.connect().and_then(|client| Ok((client.list()?, client)));
                let (files, client) = match listed {
                    Ok(listed) => listed,
                    Err(e) => {
                        rep.warn(tr!("Aufräumen auf „{name}“ nicht möglich: {e:#}", "Cannot clean up on “{name}”: {e:#}"));
                        continue;
                    }
                };
                for old in surplus(files.into_iter().map(|f| f.name).collect()) {
                    match client.delete(&old) {
                        Ok(()) => {
                            rep.log(tr!("Alte Sicherung gelöscht („{name}“): {old}", "Old backup deleted (“{name}”): {old}"));
                            removed += 1;
                        }
                        Err(e) => rep.warn(tr!(
                            "Alte Sicherung {old} auf „{name}“ nicht gelöscht: {e:#}",
                            "Old backup {old} on “{name}” was not deleted: {e:#}"
                        )),
                    }
                }
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------- Verlauf

/// Ergebnis eines Auftragslaufs.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RunRecord {
    pub job_id: u64,
    pub job_name: String,
    /// Zeitpunkt als Text, z. B. `10.11.2026 14:30`
    pub finished: String,
    pub ok: bool,
    pub text: String,
}

fn history_path() -> Option<PathBuf> {
    Some(Config::data_dir()?.join("verlauf.json"))
}

/// Protokoll des letzten Laufs eines Auftrags.
pub fn log_path(job_id: u64) -> Option<PathBuf> {
    Some(Config::data_dir()?.join("protokolle").join(format!("auftrag-{job_id}.log")))
}

/// Bisherige Läufe, ältester zuerst.
pub fn load_history() -> Vec<RunRecord> {
    history_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn record(entry: RunRecord) {
    // Testläufe in Entwicklungs-Builds sollen den echten Verlauf nicht verändern.
    #[cfg(debug_assertions)]
    if std::env::var_os("PACKUP_NO_SAVE").is_some() {
        return;
    }
    if cfg!(test) {
        return;
    }
    let Some(path) = history_path() else { return };
    let mut history = load_history();
    history.push(entry);
    let start = history.len().saturating_sub(MAX_HISTORY);
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string_pretty(&history[start..]) {
        let _ = fs::write(path, text);
    }
}

// ---------------------------------------------------------------------------- Lauf ohne Oberfläche

/// Führt den Auftrag mit dieser Kennung ohne Fenster aus (für den Zeitplan und für Skripte).
/// Rückgabe ist der Beendigungscode des Programms: 0 = gelungen.
pub fn run_headless(job_id: u64) -> i32 {
    let cfg = Config::load();
    let log_file = log_path(job_id);
    if let Some(dir) = log_file.as_ref().and_then(|p| p.parent()) {
        let _ = fs::create_dir_all(dir);
    }
    let mut log = log_file.as_ref().and_then(|path| fs::File::create(path).ok());
    let mut write = move |line: &str| {
        if let Some(file) = log.as_mut() {
            let _ = writeln!(file, "{} {line}", Local::now().format("%d.%m.%Y %H:%M:%S"));
        }
    };

    let Some(saved) = cfg.jobs.iter().find(|j| j.id == job_id).cloned() else {
        write(&tr!("Auftrag {job_id} gibt es nicht.", "Job {job_id} does not exist."));
        if cfg.notify.applies(false) {
            notify_failure(
                &tr!("Auftrag {job_id}", "Job {job_id}"),
                t!(
                    "Diesen Auftrag gibt es nicht mehr. Bitte den Zeitplan in Packup neu speichern.",
                    "This job no longer exists. Please save the schedule in Packup again."
                ),
            );
        }
        return 2;
    };
    write(&tr!(
        "Auftrag „{}“ startet (Packup {}).",
        "Job “{}” is starting (Packup {}).",
        saved.name,
        env!("CARGO_PKG_VERSION")
    ));

    // Passwörter kommen aus der Windows-Anmeldeinformationsverwaltung.
    let password_of = |target: &Target| target.password_key().and_then(|key| config::load_password(&key)).unwrap_or_default();
    let job_password = saved.encrypt.then(|| config::load_password(&saved.password_key())).flatten();
    let job = match build(&saved, &cfg, &password_of, job_password) {
        Ok(job) => job,
        Err(message) => {
            write(&message.replace('\n', " – "));
            record(RunRecord {
                job_id,
                job_name: saved.name.clone(),
                finished: Local::now().format("%d.%m.%Y %H:%M").to_string(),
                ok: false,
                text: message.replace('\n', " – "),
            });
            // Auch ein Auftrag, der gar nicht erst starten konnte, wird gemeldet.
            if let Some((mode, mail)) = mail_report(&cfg)
                && mode.applies(false)
            {
                let (subject, body) = report_text(&saved.name, false, &message, &[]);
                if let Err(e) = crate::mail::send(&mail, &subject, &body) {
                    write(&tr!(
                        "WARNUNG: Der E-Mail-Bericht wurde nicht gesendet: {e:#}",
                        "WARNING: The e-mail report was not sent: {e:#}"
                    ));
                }
            }
            if cfg.notify.applies(false) {
                notify_failure(&saved.name, &message);
            }
            return 1;
        }
    };

    let _awake = crate::sysimage::KeepAwake::new();
    let (rep, rx, _cancel) = Reporter::new(|| {});
    // Meldungen der Sicherung landen im Protokoll des Auftrags.
    let logger = std::thread::spawn(move || {
        for msg in rx {
            match msg {
                Msg::Log(text) => write(&text),
                Msg::Warn(text) => write(&tr!("WARNUNG: {text}", "WARNING: {text}")),
                Msg::Status(_) | Msg::Progress { .. } => {}
            }
        }
        write
    });
    let result = execute(&saved, &job, &mail_report(&cfg), &rep);
    drop(rep);
    let mut write = logger.join().expect("Protokoll-Thread");
    match result {
        Ok(summary) => {
            write(&tr!("Fertig: {} Dateien, {}.", "Done: {} files, {}.", summary.files, summary.saved.join("; ")));
            if cfg.notify.applies(true) {
                let text = tr!(
                    "{} Dateien ({}) gesichert.",
                    "{} files ({}) backed up.",
                    summary.files,
                    fmt_bytes(summary.bytes)
                );
                crate::notify::show_blocking(
                    &tr!("Sicherung „{}“ gelungen", "Backup “{}” succeeded", saved.name),
                    &text,
                    false,
                );
            }
            0
        }
        Err(e) => {
            write(&tr!("FEHLER: {e:#}", "ERROR: {e:#}"));
            if cfg.notify.applies(false) {
                notify_failure(&saved.name, &format!("{e:#}"));
            }
            1
        }
    }
}

/// Meldet einen gescheiterten Lauf mit einem Hinweisfenster – sonst bliebe er unbemerkt.
fn notify_failure(job_name: &str, message: &str) {
    #[cfg(all(windows, not(test)))]
    {
        #[link(name = "user32")]
        unsafe extern "system" {
            fn MessageBoxW(window: isize, text: *const u16, caption: *const u16, kind: u32) -> i32;
        }
        const MB_ICONERROR: u32 = 0x10;
        const MB_SETFOREGROUND: u32 = 0x0001_0000;
        const MB_TOPMOST: u32 = 0x0004_0000;
        let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
        let text = wide(&tr!(
            "Die Sicherung „{job_name}“ ist fehlgeschlagen.\n\n{message}\n\nEinzelheiten stehen in Packup unter „Aufträge“.",
            "The backup “{job_name}” failed.\n\n{message}\n\nYou can find the details in Packup under “Jobs”."
        ));
        let caption = wide("Packup");
        // SAFETY: Beide Zeichenketten sind nullterminiert und leben bis zum Ende des Aufrufs.
        unsafe { MessageBoxW(0, text.as_ptr(), caption.as_ptr(), MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST) };
    }
    #[cfg(not(all(windows, not(test))))]
    let _ = (job_name, message);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::Source;
    use crate::config::Compression;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("packup-test-jobs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn folder_target(id: u64, path: &std::path::Path) -> Target {
        Target { id, name: format!("Ziel {id}"), kind: TargetKind::Local, path: path.display().to_string(), ..Target::default() }
    }

    #[test]
    fn saved_job_runs_and_keeps_only_the_newest_backups() {
        let base = temp_dir("run");
        let source = base.join("quelle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("brief.txt"), "Hallo").unwrap();
        let (first, second) = (base.join("ziel-a"), base.join("ziel-b"));
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();

        // Im ersten Ziel liegen schon drei ältere Sicherungen dieses Profils und Fremdes.
        let prefix = backup::archive_prefix("anna");
        let old: Vec<String> = ["2020-01-01_120000", "2020-01-02_120000", "2020-01-03_120000"]
            .iter()
            .map(|stamp| format!("{prefix}{stamp}.zip"))
            .collect();
        for name in &old {
            fs::write(first.join(name), "alt").unwrap();
        }
        let foreign = [format!("{prefix}Urlaub.zip"), "Backup_PC_berta_2020-01-01_120000.zip".to_string(), "notizen.txt".to_string()];
        for name in &foreign {
            fs::write(first.join(name), "fremd").unwrap();
        }

        let cfg = Config { targets: vec![folder_target(1, &first), folder_target(2, &second)], ..Config::default() };
        let saved = SavedJob {
            id: 7,
            name: "Täglich".into(),
            profile_name: "anna".into(),
            profile_dir: base.clone(),
            sources: vec![Source::plain(source, None)],
            target_ids: vec![1, 2],
            compression: Compression::Fast,
            keep_last: 2,
            ..SavedJob::default()
        };
        let job = build(&saved, &cfg, &|_| String::new(), None).unwrap();
        assert_eq!(job.destinations.len(), 2);
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let summary = execute(&saved, &job, &None, &rep).unwrap();
        assert_eq!((summary.files, summary.saved.len()), (1, 2));

        // Geblieben sind die neue und die jüngste alte Sicherung – und alles Fremde.
        let mut left: Vec<String> =
            fs::read_dir(&first).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        let new_name = summary.local_path.unwrap().file_name().unwrap().to_string_lossy().into_owned();
        let mut expected = vec![old[2].clone(), new_name, foreign[0].clone(), foreign[1].clone(), foreign[2].clone()];
        expected.sort();
        assert_eq!(left, expected);
        assert_eq!(fs::read_dir(&second).unwrap().count(), 1);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn job_without_its_target_or_password_is_refused() {
        let base = temp_dir("refuse");
        let cfg = Config { targets: vec![folder_target(1, &base)], ..Config::default() };
        let mut saved = SavedJob {
            name: "Test".into(),
            sources: vec![Source::plain(base.clone(), None)],
            target_ids: vec![1, 9],
            ..SavedJob::default()
        };
        assert!(build(&saved, &cfg, &|_| String::new(), None).err().unwrap().contains("kein gültiges Ziel"));
        saved.target_ids = vec![1];
        assert!(build(&saved, &cfg, &|_| String::new(), None).is_ok());
        saved.encrypt = true;
        assert!(build(&saved, &cfg, &|_| String::new(), None).err().unwrap().contains("Passwort"));
        assert_eq!(build(&saved, &cfg, &|_| String::new(), Some("geheim123".into())).unwrap().password.as_deref(), Some("geheim123"));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn finds_backups_on_folder_targets_and_skips_unreachable_ones() {
        let base = temp_dir("find");
        fs::write(base.join("Backup_PC_anna_2026-11-10_143005.zip"), "zip").unwrap();
        fs::write(base.join("Urlaub.ZIP"), "noch eins").unwrap();
        fs::write(base.join("notizen.txt"), "kein Archiv").unwrap();
        fs::create_dir_all(base.join("ordner.zip")).unwrap();
        let missing = Target { id: 2, name: "Stick".into(), kind: TargetKind::Local, path: base.join("fehlt").display().to_string(), ..Target::default() };
        let targets = [(folder_target(1, &base), String::new()), (missing, String::new())];

        let (rep, rx, _cancel) = Reporter::new(|| {});
        let found = find_backups(&targets, &rep);
        let mut names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["Backup_PC_anna_2026-11-10_143005.zip", "Urlaub.ZIP"]);
        assert!(found.iter().all(|f| f.target_id == 1 && matches!(&f.place, Place::File(path) if path.is_file())));
        // Das fehlende Ziel wird gemeldet, lässt die Suche aber nicht scheitern.
        assert!(rx.try_iter().any(|m| matches!(m, Msg::Warn(text) if text.contains("Stick"))));
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn recognises_only_exact_backup_names() {
        assert!(backup::is_archive_of("Backup_PC_anna_2026-11-10_143005.zip", "Backup_PC_anna_"));
        assert!(!backup::is_archive_of("Backup_PC_anna_2_2026-11-10_143005.zip", "Backup_PC_anna_"));
        assert!(!backup::is_archive_of("Backup_PC_anna_2026-11-10_143005.zip.part", "Backup_PC_anna_"));
        assert!(!backup::is_archive_of("Backup_PC_anna_2026-11-10.zip", "Backup_PC_anna_"));
        assert!(!backup::is_archive_of("Backup_PC_anna_Urlaub-2026_abcdef.zip", "Backup_PC_anna_"));
    }
}
