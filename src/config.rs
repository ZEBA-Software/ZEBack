//! Einstellungen (JSON unter %APPDATA%\Packup) und Passwort-Ablage
//! in der Windows-Anmeldeinformationsverwaltung.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_DIR: &str = "Packup";
const KEYRING_SERVICE: &str = "Packup";
/// Bis Version 0.4 hieß das Programm ProfileBackup. Einstellungen und gemerkte Passwörter von
/// damals werden weiter gefunden; gespeichert wird nur noch unter dem neuen Namen.
const LEGACY_APP_DIR: &str = "ProfileBackup";
const LEGACY_KEYRING_SERVICE: &str = "ProfileBackup-Nextcloud";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Compression {
    None,
    Fast,
    #[default]
    Normal,
    Max,
}

impl Compression {
    pub const ALL: [Compression; 4] =
        [Compression::None, Compression::Fast, Compression::Normal, Compression::Max];

    pub fn label(self) -> &'static str {
        match self {
            Compression::None => t!("Keine", "None"),
            Compression::Fast => t!("Schnell", "Fast"),
            Compression::Normal => "Normal",
            Compression::Max => t!("Maximal", "Maximum"),
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Compression::None => t!(
                "Nur verpacken: am schnellsten, die ZIP-Datei ist so groß wie die Originale.",
                "Store only: fastest, the ZIP file is as large as the originals."
            ),
            Compression::Fast => t!(
                "Schnell fertig, die ZIP-Datei wird etwas größer.",
                "Finishes quickly, the ZIP file is slightly larger."
            ),
            Compression::Normal => t!(
                "Ausgewogen zwischen Tempo und Größe – empfohlen.",
                "A balance of speed and size – recommended."
            ),
            Compression::Max => t!(
                "Kleinste ZIP-Datei, dauert aber deutlich länger.",
                "Smallest ZIP file, but takes considerably longer."
            ),
        }
    }

    /// Deflate-Stufe; `None` bedeutet "unkomprimiert speichern".
    pub fn deflate_level(self) -> Option<i64> {
        match self {
            Compression::None => None,
            Compression::Fast => Some(1),
            Compression::Normal => Some(6),
            Compression::Max => Some(9),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ExtraFolder {
    pub path: PathBuf,
    pub enabled: bool,
}

/// Art eines Sicherungsziels.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TargetKind {
    /// Ordner auf diesem PC, einer externen Festplatte oder einem USB-Stick
    #[default]
    Local,
    /// Netzwerkfreigabe (SMB), z. B. auf einem NAS oder Server
    Smb,
    /// Nextcloud-Server (WebDAV)
    Nextcloud,
    /// Gewöhnlicher WebDAV-Server
    WebDav,
    /// S3-kompatibler Speicher (Amazon S3, MinIO, Wasabi …)
    S3,
    /// SFTP-Server (über den OpenSSH-Client von Windows)
    Sftp,
}

impl TargetKind {
    pub fn label(self) -> &'static str {
        match self {
            TargetKind::Local => t!("Ordner / USB", "Folder / USB"),
            TargetKind::Smb => t!("Netzwerkfreigabe (SMB)", "Network share (SMB)"),
            TargetKind::Nextcloud => "Nextcloud",
            TargetKind::WebDav => "WebDAV",
            TargetKind::S3 => t!("S3-Speicher", "S3 storage"),
            TargetKind::Sftp => "SFTP",
        }
    }

    /// Ziel im Netz, auf das die fertige ZIP-Datei hochgeladen wird.
    pub fn is_cloud(self) -> bool {
        matches!(self, TargetKind::Nextcloud | TargetKind::WebDav | TargetKind::S3 | TargetKind::Sftp)
    }
}

/// Ein benanntes Sicherungsziel. Passwörter stehen nicht hier, sondern in der
/// Windows-Anmeldeinformationsverwaltung.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Target {
    pub id: u64,
    pub name: String,
    pub kind: TargetKind,
    /// Ordner (lokal) bzw. Freigabe mit Unterordner, z. B. `\\nas\backup\pc01`; S3: der Bucket;
    /// SFTP: die Schlüsseldatei (leer = Anmeldung mit Passwort)
    pub path: String,
    /// Nextcloud, WebDAV, S3: Adresse des Servers
    pub url: String,
    /// Anmeldename (SMB: optional, sonst gilt die Windows-Anmeldung); S3: Zugangsschlüssel
    pub user: String,
    /// Nextcloud, WebDAV, S3: Ordner auf dem Server
    pub remote_dir: String,
    /// S3: Region des Buckets
    pub region: String,
    pub save_password: bool,
    /// Für Sicherungen angehakt
    pub selected: bool,
}

impl Target {
    /// Kurzbeschreibung für Listen, z. B. `Nextcloud · cloud.example.de / Backups`.
    pub fn summary(&self) -> String {
        format!("{} · {}", self.kind.label(), self.place())
    }

    /// Der Ort allein: Ordner, Freigabe oder Server mit Ordner.
    pub fn place(&self) -> String {
        match self.kind {
            TargetKind::Local | TargetKind::Smb if self.path.trim().is_empty() => {
                t!("noch kein Pfad", "no path yet").to_string()
            }
            TargetKind::Local | TargetKind::Smb => self.path.trim().to_string(),
            _ if self.url.trim().is_empty() => t!("noch keine Adresse", "no address yet").to_string(),
            TargetKind::Nextcloud | TargetKind::WebDav | TargetKind::Sftp => {
                format!("{} / {}", self.url.trim().trim_start_matches("https://"), self.remote_dir.trim())
            }
            TargetKind::S3 => format!(
                "{} / {} / {}",
                self.url.trim().trim_start_matches("https://"),
                self.path.trim(),
                self.remote_dir.trim()
            ),
        }
    }

    /// Braucht dieses Ziel ein Passwort?
    pub fn needs_password(&self) -> bool {
        match self.kind {
            TargetKind::Local => false,
            TargetKind::Smb => !self.user.trim().is_empty(),
            TargetKind::Nextcloud | TargetKind::WebDav | TargetKind::S3 => true,
            // Mit Schlüsseldatei (steht in `path`) braucht SFTP kein Passwort.
            TargetKind::Sftp => self.path.trim().is_empty(),
        }
    }

    /// Name, unter dem das Passwort in der Anmeldeinformationsverwaltung liegt.
    pub fn password_key(&self) -> Option<String> {
        let user = self.user.trim();
        match self.kind {
            TargetKind::Nextcloud if !user.is_empty() && !self.url.trim().is_empty() => {
                Some(format!("{user}@{}", self.url.trim()))
            }
            TargetKind::Smb if !user.is_empty() && !self.path.trim().is_empty() => {
                Some(format!("smb:{user}@{}", self.path.trim()))
            }
            TargetKind::WebDav if !user.is_empty() && !self.url.trim().is_empty() => {
                Some(format!("webdav:{user}@{}", self.url.trim()))
            }
            TargetKind::Sftp if self.needs_password() && !user.is_empty() && !self.url.trim().is_empty() => {
                Some(format!("sftp:{user}@{}", self.url.trim()))
            }
            TargetKind::S3 if !user.is_empty() && !self.url.trim().is_empty() => {
                Some(format!("s3:{user}@{}/{}", self.url.trim(), self.path.trim()))
            }
            _ => None,
        }
    }
}

/// Wann ein Auftrag von selbst läuft.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ScheduleKind {
    /// Nur von Hand
    #[default]
    Off,
    Daily,
    Weekly,
}

/// Kurznamen der Wochentage in der eingestellten Sprache, beginnend mit Montag.
pub fn weekdays() -> [&'static str; 7] {
    [
        t!("Mo", "Mon"),
        t!("Di", "Tue"),
        t!("Mi", "Wed"),
        t!("Do", "Thu"),
        t!("Fr", "Fri"),
        t!("Sa", "Sat"),
        t!("So", "Sun"),
    ]
}

/// Ein gespeicherter Sicherungsauftrag: was, wohin, wie oft. Er lässt sich mit einem Klick,
/// nach Zeitplan oder über `Packup.exe --run-job <Kennung>` ausführen.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct SavedJob {
    pub id: u64,
    pub name: String,
    /// Name für Dateiname und Inhaltsbeschreibung (Profilname oder z. B. `Laufwerk-D`)
    pub profile_name: String,
    pub profile_dir: PathBuf,
    /// Die Ordner bzw. das Laufwerk, so wie sie beim Speichern ausgewählt waren
    pub sources: Vec<crate::backup::Source>,
    /// Kennungen der Sicherungsziele
    pub target_ids: Vec<u64>,
    pub compression: Compression,
    pub skip_online_only: bool,
    pub verify: bool,
    /// Verschlüsseln; das Passwort liegt dann in der Windows-Anmeldeinformationsverwaltung.
    pub encrypt: bool,
    /// So viele Sicherungen bleiben je Ziel liegen; 0 = nie etwas löschen.
    /// Mit Teilsicherungen zählen Vollsicherungen samt der Teilsicherungen, die auf ihnen aufbauen.
    pub keep_last: u32,
    /// Nur Geändertes sichern: zwischen den Vollsicherungen entstehen Teilsicherungen.
    pub incremental: bool,
    /// Jede wievielte Sicherung wieder vollständig ist.
    pub full_every: u32,
    pub schedule: ScheduleKind,
    /// Wochentag bei wöchentlichem Zeitplan (0 = Montag)
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
}

impl Default for SavedJob {
    fn default() -> Self {
        Self {
            id: 0,
            name: String::new(),
            profile_name: String::new(),
            profile_dir: PathBuf::new(),
            sources: Vec::new(),
            target_ids: Vec::new(),
            compression: Compression::Normal,
            skip_online_only: true,
            verify: true,
            encrypt: false,
            keep_last: 0,
            incremental: false,
            full_every: 7,
            schedule: ScheduleKind::Off,
            weekday: 0,
            hour: 12,
            minute: 0,
        }
    }
}

impl SavedJob {
    /// Name, unter dem das Verschlüsselungs-Passwort des Auftrags abgelegt ist.
    pub fn password_key(&self) -> String {
        format!("job:{}", self.id)
    }

    /// Zeitplan in Worten, z. B. `täglich 12:00`.
    pub fn schedule_text(&self) -> String {
        let time = format!("{:02}:{:02}", self.hour.min(23), self.minute.min(59));
        match self.schedule {
            ScheduleKind::Off => t!("nur von Hand", "manual only").to_string(),
            ScheduleKind::Daily => tr!("täglich {time}", "daily {time}"),
            ScheduleKind::Weekly => format!("{} {time}", weekdays()[usize::from(self.weekday.min(6))]),
        }
    }
}

/// Sprache der Oberfläche.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Language {
    /// Wie Windows: Deutsch auf einem deutschen System, sonst Englisch
    #[default]
    Auto,
    German,
    English,
}

/// Wann nach einem Auftrag berichtet wird.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReportMode {
    Never,
    /// Nur wenn der Auftrag fehlgeschlagen ist
    #[default]
    Errors,
    Always,
}

impl ReportMode {
    /// Soll über einen Lauf mit diesem Ausgang berichtet werden?
    pub fn applies(self, ok: bool) -> bool {
        match self {
            ReportMode::Never => false,
            ReportMode::Errors => !ok,
            ReportMode::Always => true,
        }
    }
}

/// E-Mail-Bericht nach Aufträgen. Das Passwort liegt in der Windows-Anmeldeinformationsverwaltung.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MailSettings {
    pub mode: ReportMode,
    pub server: String,
    pub port: u16,
    /// STARTTLS (üblich auf Port 587) statt TLS von Anfang an (Port 465)
    pub starttls: bool,
    pub user: String,
    pub from: String,
    pub to: String,
}

impl Default for MailSettings {
    fn default() -> Self {
        Self {
            mode: ReportMode::Never,
            server: String::new(),
            port: 465,
            starttls: false,
            user: String::new(),
            from: String::new(),
            to: String::new(),
        }
    }
}

impl MailSettings {
    /// Name, unter dem das Passwort des Postfachs abgelegt ist.
    pub fn password_key(&self) -> Option<String> {
        let (user, server) = (self.user.trim(), self.server.trim());
        (!user.is_empty() && !server.is_empty()).then(|| format!("smtp:{user}@{server}"))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    pub language: Language,
    /// Beim Start beim Hersteller nach einer neueren Version fragen
    pub check_updates: bool,
    /// Windows-Benachrichtigung nach Aufträgen, die ohne Fenster laufen
    pub notify: ReportMode,
    pub mail: MailSettings,
    /// Gespeicherte Sicherungsaufträge
    pub jobs: Vec<SavedJob>,
    /// Schlüssel der Standardordner, die der Benutzer abgewählt hat.
    pub unchecked_standard: Vec<String>,
    /// Schlüssel der Zusatzordner (Browser, Mail, …), die der Benutzer angehakt hat.
    pub checked_optional: Vec<String>,
    pub extra_folders: Vec<ExtraFolder>,
    /// Die eingerichteten Sicherungsziele
    pub targets: Vec<Target>,
    /// Einstellungen aus Versionen vor 0.4 (ein Ordner und ein Nextcloud-Zugang); werden beim
    /// Laden in `targets` übernommen und danach nicht mehr gespeichert.
    #[serde(skip_serializing)]
    pub use_local: bool,
    #[serde(skip_serializing)]
    pub local_dir: String,
    #[serde(skip_serializing)]
    pub use_nextcloud: bool,
    pub compression: Compression,
    pub skip_online_only: bool,
    /// Die fertige ZIP-Datei nach dem Backup vollständig prüfen.
    pub verify_after_backup: bool,
    /// Sicherungen mit Passwort verschlüsseln. Das Passwort selbst wird nie gespeichert.
    pub encrypt: bool,
    /// Laufwerkssicherung: Windows und Programme auslassen, nur Daten sichern.
    pub drive_data_only: bool,
    #[serde(skip_serializing)]
    pub nc_url: String,
    #[serde(skip_serializing)]
    pub nc_user: String,
    #[serde(skip_serializing)]
    pub nc_dir: String,
    #[serde(skip_serializing)]
    pub nc_save_password: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: Language::Auto,
            check_updates: true,
            notify: ReportMode::Errors,
            mail: MailSettings::default(),
            jobs: Vec::new(),
            unchecked_standard: Vec::new(),
            checked_optional: Vec::new(),
            extra_folders: Vec::new(),
            targets: Vec::new(),
            use_local: true,
            local_dir: String::new(),
            use_nextcloud: false,
            compression: Compression::Normal,
            skip_online_only: true,
            verify_after_backup: true,
            encrypt: false,
            drive_data_only: true,
            nc_url: String::new(),
            nc_user: String::new(),
            nc_dir: "Backups".into(),
            nc_save_password: true,
        }
    }
}

/// Der Ordner, unter dem die Einstellungen liegen (`%APPDATA%`). Entwicklungs-Builds lassen
/// sich für Testläufe über `PACKUP_DATA_DIR` auf einen anderen Ordner umlenken.
fn base_dir() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("PACKUP_DATA_DIR") {
        return Some(dir.into());
    }
    dirs::config_dir()
}

fn config_path() -> Option<PathBuf> {
    Some(base_dir()?.join(APP_DIR).join("config.json"))
}

fn legacy_config_path() -> Option<PathBuf> {
    Some(base_dir()?.join(LEGACY_APP_DIR).join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        let mut config = Self::read_first(&[config_path(), legacy_config_path()]);
        config.migrate_legacy_targets();
        config
    }

    /// Liest die erste vorhandene, lesbare Einstellungsdatei; sonst gelten die Vorgaben.
    fn read_first(paths: &[Option<PathBuf>]) -> Self {
        paths
            .iter()
            .flatten()
            .find_map(|path| serde_json::from_str(&fs::read_to_string(path).ok()?).ok())
            .unwrap_or_default()
    }

    /// Nächste freie Kennung für einen neuen Auftrag.
    pub fn next_job_id(&self) -> u64 {
        self.jobs.iter().map(|j| j.id).max().unwrap_or(0) + 1
    }

    /// Ordner mit Einstellungen, Verlauf und Protokollen der Aufträge.
    pub fn data_dir() -> Option<PathBuf> {
        Some(base_dir()?.join(APP_DIR))
    }

    /// Nächste freie Kennung für ein neues Ziel.
    pub fn next_target_id(&self) -> u64 {
        self.targets.iter().map(|t| t.id).max().unwrap_or(0) + 1
    }

    /// Übernimmt den Zielordner und den Nextcloud-Zugang älterer Versionen als Sicherungsziele.
    pub fn migrate_legacy_targets(&mut self) {
        if !self.targets.is_empty() {
            return;
        }
        let folder = self.local_dir.trim().to_string();
        if !folder.is_empty() {
            let name = PathBuf::from(&folder)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| folder.clone());
            let id = self.next_target_id();
            self.targets.push(Target {
                id,
                name,
                kind: if folder.starts_with("\\\\") { TargetKind::Smb } else { TargetKind::Local },
                path: folder,
                selected: self.use_local,
                ..Target::default()
            });
        }
        if !self.nc_url.trim().is_empty() {
            let id = self.next_target_id();
            self.targets.push(Target {
                id,
                name: "Nextcloud".to_string(),
                kind: TargetKind::Nextcloud,
                url: self.nc_url.trim().to_string(),
                user: self.nc_user.trim().to_string(),
                remote_dir: self.nc_dir.trim().to_string(),
                save_password: self.nc_save_password,
                selected: self.use_nextcloud,
                ..Target::default()
            });
        }
        self.local_dir.clear();
        self.nc_url.clear();
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context(t!("Einstellungsordner nicht gefunden", "Settings folder not found"))?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| {
                tr!(
                    "Einstellungen konnten nicht gespeichert werden: {}",
                    "The settings could not be saved: {}",
                    path.display()
                )
            })
    }
}

/// Eintrag in der Windows-Anmeldeinformationsverwaltung; `key` kommt von [`Target::password_key`].
fn keyring_entry(service: &str, key: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service, key).context(t!(
        "Zugriff auf die Windows-Anmeldeinformationsverwaltung fehlgeschlagen",
        "Could not access Windows Credential Manager"
    ))
}

/// Liest ein gemerktes Passwort – auch eines, das noch unter dem alten Programmnamen liegt.
pub fn load_password(key: &str) -> Option<String> {
    [KEYRING_SERVICE, LEGACY_KEYRING_SERVICE]
        .into_iter()
        .find_map(|service| keyring_entry(service, key).ok()?.get_password().ok())
}

pub fn save_password(key: &str, password: &str) -> Result<()> {
    keyring_entry(KEYRING_SERVICE, key)?
        .set_password(password)
        .context(t!("Passwort konnte nicht gespeichert werden", "The password could not be saved"))
}

/// Entfernt das Passwort unter dem neuen und dem alten Programmnamen, damit es nicht über den
/// alten Eintrag wieder auftaucht.
pub fn delete_password(key: &str) {
    for service in [KEYRING_SERVICE, LEGACY_KEYRING_SERVICE] {
        if let Ok(entry) = keyring_entry(service, key) {
            let _ = entry.delete_credential();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_folder_and_nextcloud_from_older_versions() {
        let old = r#"{
            "use_local": true, "local_dir": "E:\\Backups", "use_nextcloud": false,
            "nc_url": "https://cloud.example.de", "nc_user": "anna", "nc_dir": "Backups", "nc_save_password": true,
            "compression": "Fast"
        }"#;
        let mut config: Config = serde_json::from_str(old).unwrap();
        config.migrate_legacy_targets();
        assert_eq!(config.compression, Compression::Fast);
        assert_eq!(config.targets.len(), 2);

        let folder = &config.targets[0];
        assert_eq!((folder.kind, folder.name.as_str(), folder.path.as_str()), (TargetKind::Local, "Backups", "E:\\Backups"));
        assert!(folder.selected && folder.password_key().is_none() && !folder.needs_password());

        let cloud = &config.targets[1];
        assert_eq!((cloud.kind, cloud.id, cloud.selected), (TargetKind::Nextcloud, 2, false));
        // Derselbe Schlüssel wie in den Vorversionen: das gespeicherte Passwort wird wiedergefunden.
        assert_eq!(cloud.password_key().as_deref(), Some("anna@https://cloud.example.de"));
        assert_eq!(cloud.summary(), "Nextcloud · cloud.example.de / Backups");

        // Nach dem Speichern stehen die alten Felder nicht mehr in der Datei.
        let saved = serde_json::to_string(&config).unwrap();
        assert!(saved.contains("\"targets\"") && !saved.contains("nc_url") && !saved.contains("local_dir"));
        // Erneutes Laden legt nichts doppelt an.
        let mut again: Config = serde_json::from_str(&saved).unwrap();
        again.migrate_legacy_targets();
        assert_eq!(again.targets.len(), 2);
    }

    #[test]
    fn settings_from_the_old_program_name_are_taken_over() {
        let base = std::env::temp_dir().join(format!("packup-test-config-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        let (new, old) = (base.join("neu.json"), base.join("alt.json"));
        fs::write(&old, r#"{"compression":"Max"}"#).unwrap();

        // Nur die alte Datei vorhanden: sie gilt.
        let paths = [Some(new.clone()), Some(old.clone())];
        assert_eq!(Config::read_first(&paths).compression, Compression::Max);
        // Sobald unter dem neuen Namen gespeichert wurde, gilt die neue.
        fs::write(&new, r#"{"compression":"Fast"}"#).unwrap();
        assert_eq!(Config::read_first(&paths).compression, Compression::Fast);
        // Gar keine Datei: Vorgaben.
        assert_eq!(Config::read_first(&[None, Some(base.join("fehlt.json"))]), Config::default());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn network_paths_become_smb_targets() {
        let mut config = Config { local_dir: "\\\\nas\\backup\\pc01".into(), ..Config::default() };
        config.migrate_legacy_targets();
        let target = &config.targets[0];
        assert_eq!(target.kind, TargetKind::Smb);
        assert!(!target.needs_password());
        let with_user = Target { user: "admin".into(), ..target.clone() };
        assert!(with_user.needs_password());
        assert_eq!(with_user.password_key().as_deref(), Some("smb:admin@\\\\nas\\backup\\pc01"));
    }
}
