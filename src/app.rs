//! Oberfläche: Kopfleiste, Seitenleiste, geführte Abläufe (Sichern, Wiederherstellen),
//! Einstellungen und Protokoll.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Local};
use eframe::egui::{
    self, Align, Align2, Color32, CornerRadius, Layout, Margin, Rect, RichText, Sense, Shape,
    Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::backup::{self, BackupJob, BackupKind, BackupSummary, Destination, Manifest, ManifestRoot, Source};
use crate::config::{self, Compression, Config, ExtraFolder, Language, ReportMode, SavedJob, ScheduleKind, Target, TargetKind};
use crate::jobs::{self, FoundBackup, Place, RunRecord};
use crate::schedule;
use crate::disk::{self, DiskProbe, Drive, Probe};
use crate::sizes::SizeScanner;
use crate::sysimage::{self, Answer, ExistingImage, ImageJob, ImageSummary, SystemFacts};
use crate::job::{Cancelled, Msg, Reporter};
use crate::logo;
use crate::nextcloud::{self, BrowserLogin, RemoteFile};
use crate::profiles::{self, Group, OptionalFolder, Profile, StdFolder};
use crate::restore::{self, ArchiveInfo, Conflict, Mapping, RestoreJob, RestoreSummary, Tree};
use crate::rate::RateMeter;
use crate::theme::{self, *};
use crate::update::Release;
use crate::util::{fmt_bytes, fmt_duration};
use crate::verify::{self, VerifyReport};
use crate::widgets::{self, Icon, Kind, Mark};

const MAX_LOG_LINES: usize = 2000;
/// Website des Herstellers; öffnet sich beim Klick auf den Herstellernamen.
const VENDOR_URL: &str = "https://zeba.xyz";
const SIDEBAR_WIDTH: f32 = 208.0;
const TOPBAR_HEIGHT: f32 = 52.0;
const CONTENT_WIDTH: f32 = 920.0;
/// Ab dieser Breite stehen Karten zweispaltig nebeneinander.
const TWO_COLUMNS_FROM: f32 = 740.0;
const GAP: f32 = 16.0;
const GRID_STEP: f32 = 34.0;

fn backup_steps() -> [&'static str; 4] {
    [t!("Auswahl", "Selection"), t!("Ordner", "Folders"), t!("Ziel", "Target"), t!("Start", "Start")]
}
/// Schritte für Laufwerkssicherung und Systemabbild
fn scope_steps() -> [&'static str; 4] {
    [t!("Auswahl", "Selection"), t!("Umfang", "Scope"), t!("Ziel", "Target"), t!("Start", "Start")]
}
fn restore_steps() -> [&'static str; 4] {
    [t!("Sicherung", "Backup"), t!("Inhalt", "Contents"), t!("Ziel", "Target"), t!("Start", "Start")]
}
const LAST_STEP: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Tab {
    Backup,
    Restore,
    Jobs,
    Reports,
    Targets,
    Log,
}

/// Was gesichert wird.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BackupMode {
    /// Ordner eines Benutzerprofils als ZIP
    Profile,
    /// Alle Dateien eines Laufwerks als ZIP
    Drive,
    /// Startfähiges Abbild des ganzen Systems (Windows-Abbildsicherung)
    Image,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RestoreMode {
    /// In die Ordner eines Benutzerprofils
    Profile,
    /// Alles in einen frei gewählten Ordner
    Folder,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Error,
}

/// Ergebnis eines Hintergrund-Vorgangs.
enum Outcome {
    Backup(BackupSummary),
    Restore(RestoreSummary),
    /// Gefundene Sicherungen, jeweils mit der Kennung des Nextcloud-Ziels
    Found(Vec<FoundBackup>),
    /// Inhalt der geöffneten Sicherung als Baum, für die Auswahl einzelner Dateien
    Listed(Tree),
    /// Ergebnis von "Ziel prüfen"
    TargetTest(String),
    /// Anmeldung im Browser bestätigt: Zugangsdaten für das Nextcloud-Ziel mit dieser Kennung
    NcLogin(u64, BrowserLogin),
    Inspected(ArchiveInfo),
    Verified(VerifyReport),
    Image(ImageSummary),
}

struct Running {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    handle: JoinHandle<Result<Outcome>>,
}

struct LogLine {
    warn: bool,
    text: String,
}

/// Standardordner des gewählten Profils mit Häkchen. Der Ordner selbst bleibt dabei,
/// damit sein Anzeigename immer in der gerade eingestellten Sprache erscheint.
struct StdRow {
    folder: StdFolder,
    checked: bool,
}

/// Zusatzordner (Browser, Mail, …) des gewählten Profils mit Häkchen.
struct OptRow {
    folder: OptionalFolder,
    checked: bool,
}

pub struct App {
    ctx: egui::Context,
    cfg: Config,
    logo: egui::TextureHandle,
    user_name: String,
    computer_name: String,
    profiles: Vec<Profile>,

    tab: Tab,
    /// Zeitpunkt des letzten Seiten- oder Schrittwechsels (für das Einblenden).
    page_changed: f64,
    /// Richtung des letzten Schrittwechsels: 1 = vor, -1 = zurück, 0 = Seitenwechsel.
    slide: f32,

    backup_step: usize,
    backup_mode: BackupMode,
    /// Läuft das Programm mit Administratorrechten?
    elevated: bool,
    /// Laufwerke dieses PCs
    drives: Vec<Drive>,
    /// Laufwerkssicherung: Index des Quelllaufwerks und die dafür geltenden Ausschlüsse
    drive_source: usize,
    drive_excludes: Vec<PathBuf>,
    /// Systemabbild: zusätzliche Laufwerke, Ziellaufwerk und/oder Netzwerkfreigabe
    image_include: Vec<String>,
    image_drive: Option<String>,
    /// Kennung der gewählten Netzwerkfreigabe (aus den Sicherungszielen)
    image_smb: Option<u64>,
    /// Auf der Seite "Aufträge" ausgewählter Auftrag
    job_edit: usize,
    /// Auftrag, dessen Löschen noch bestätigt werden muss (Kennung)
    job_delete: Option<u64>,
    /// Passwort des Postfachs für E-Mail-Berichte (dauerhaft in der Anmeldeinformationsverwaltung)
    mail_password: String,
    /// Bisherige Läufe der Aufträge (aus der Verlaufsdatei)
    history: Vec<RunRecord>,
    /// Angaben zum PC für das Systemabbild (werden im Hintergrund ermittelt)
    facts: Option<SystemFacts>,
    facts_rx: Option<Receiver<SystemFacts>>,
    /// Neuere Version, die beim Hersteller bereitliegt
    update: Option<Release>,
    /// Laufende Update-Abfrage; `true` = vom Benutzer angestoßen
    update_rx: Option<(Receiver<Result<Option<Release>, String>>, bool)>,
    /// Vorhandene Abbilder auf dem zuletzt abgefragten Ziel
    images: Option<(String, Vec<ExistingImage>)>,
    images_rx: Option<Receiver<(String, Vec<ExistingImage>)>>,
    backup_profile: usize,
    std_rows: Vec<StdRow>,
    opt_rows: Vec<OptRow>,
    /// Passwörter der Sicherungsziele (Kennung → Passwort), nur im Arbeitsspeicher;
    /// dauerhaft liegen sie in der Windows-Anmeldeinformationsverwaltung.
    target_passwords: HashMap<u64, String>,
    /// Auf der Seite "Sicherungsziele" ausgewähltes Ziel
    target_edit: usize,
    /// Ziel, dessen Löschen noch bestätigt werden muss (Kennung)
    target_delete: Option<u64>,
    /// Passwort für die Verschlüsselung (nur im Arbeitsspeicher, wird nie gespeichert)
    backup_password: String,
    backup_password_repeat: String,
    /// Ordnergrößen (werden im Hintergrund berechnet)
    sizes: SizeScanner,
    /// Freier Platz am Ziel bzw. im Temp-Ordner
    disk_target: DiskProbe,
    disk_temp: DiskProbe,

    restore_step: usize,
    archive: Option<ArchiveInfo>,
    root_checked: Vec<bool>,
    restore_mode: RestoreMode,
    restore_profile: usize,
    restore_dir: String,
    conflict: Conflict,
    /// Auf den Sicherungszielen gefundene Sicherungen (solange der Auswahldialog offen ist)
    found_backups: Option<Vec<FoundBackup>>,
    /// Inhalt der geöffneten Sicherung als Baum (wird erst bei Bedarf gelesen)
    tree: Option<Arc<Tree>>,
    /// Einzeln ausgewählte Dateien und Ordner (Pfade im Archiv); leer = ganze Ordner
    picked: HashSet<String>,
    /// Im Auswahldialog geöffneter Ordner; `None` = Dialog geschlossen
    browse_dir: Option<String>,
    /// Ist die geöffnete Sicherung eine Teilsicherung und liegt ihre Kette vollständig daneben:
    /// Vollsicherung und Teilsicherungen bis zu ihr, älteste zuerst.
    chain: Option<Vec<PathBuf>>,
    /// Passwort der geöffneten, verschlüsselten Sicherung
    restore_password: String,
    /// Zuletzt erfolgreich geprüftes Passwort (spart die erneute Prüfung)
    restore_password_ok: Option<String>,

    running: Option<Running>,
    status: String,
    progress: Option<(u64, u64)>,
    /// Der Fortschritt ist eine Prozentangabe statt einer Datenmenge (Systemabbild).
    progress_percent: bool,
    /// Tempo des laufenden Vorgangs, aus den Fortschrittsmeldungen der letzten Sekunden
    rate: RateMeter,
    /// Wann der laufende Vorgang begonnen hat (Zeit der Oberfläche in Sekunden)
    started_at: f64,
    log: Vec<LogLine>,
    unseen_warnings: usize,
    result: Option<(Level, String)>,
    result_at: f64,
}

fn std_icon(key: Option<&str>) -> Icon {
    match key {
        Some("desktop") => Icon::Desktop,
        Some("documents") => Icon::Documents,
        Some("downloads") => Icon::Downloads,
        Some("pictures") => Icon::Pictures,
        Some("music") => Icon::Music,
        Some("videos") => Icon::Videos,
        Some("firefox" | "chrome" | "edge") => Icon::Globe,
        Some("thunderbird" | "outlook_signatures") => Icon::Mail,
        Some("office_templates" | "office_dictionary") => Icon::Documents,
        Some("ssh") => Icon::Key,
        Some("vscode") => Icon::Settings,
        _ => Icon::Folder,
    }
}

/// Zahl mit Schaltflächen zum Verringern und Erhöhen; läuft am Ende wieder bei 0 los.
fn number_stepper(ui: &mut Ui, salt: &str, value: &mut u8, modulus: u8, step: u8) {
    ui.push_id(salt, |ui| {
        if widgets::button(ui, "-", Kind::Secondary, None).clicked() {
            *value = (*value + modulus - step) % modulus;
        }
        widgets::text(ui, format!("{:02}", *value), theme::bold(15.0), TEXT_BRIGHT);
        if widgets::button(ui, "+", Kind::Secondary, None).clicked() {
            *value = (*value + step) % modulus;
        }
    });
}

fn target_icon(kind: TargetKind) -> Icon {
    match kind {
        TargetKind::Local => Icon::Usb,
        TargetKind::Smb => Icon::Globe,
        TargetKind::Nextcloud | TargetKind::WebDav => Icon::Cloud,
        TargetKind::S3 => Icon::Zip,
        TargetKind::Sftp => Icon::Key,
    }
}

fn pick_folder(title: &str) -> Option<PathBuf> {
    rfd::FileDialog::new().set_title(title).pick_folder()
}

/// Hinweiszeile mit Warnsymbol.
fn warning_line(ui: &mut Ui, message: &str) {
    ui.horizontal(|ui| {
        widgets::inline_icon(ui, Icon::Warning, 14.0, YELLOW);
        ui.add(egui::Label::new(RichText::new(message).font(theme::regular(13.0)).color(YELLOW)).wrap());
    });
}

/// Überschrift eines Schritts im geführten Ablauf.
fn step_heading(ui: &mut Ui, step: usize, title: &str, description: &str) {
    ui.add_space(14.0);
    widgets::text(ui, tr!("Schritt {} von {}", "Step {} of {}", step + 1, LAST_STEP + 1), theme::regular(12.0), ACCENT);
    widgets::text(ui, title, theme::bold(19.0), TEXT_BRIGHT);
    widgets::text(ui, description, theme::regular(13.0), TEXT_MUTED);
    ui.add_space(8.0);
}

/// Liste der Benutzerprofile zur Auswahl. Rückgabe: Auswahl geändert.
fn profile_rows(ui: &mut Ui, profiles: &[Profile], selected: &mut usize) -> bool {
    let mut changed = false;
    for (index, profile) in profiles.iter().enumerate() {
        let title = if profile.current { tr!("{} (angemeldet)", "{} (signed in)", profile.name) } else { profile.name.clone() };
        let description = if profile.accessible {
            profile.dir.display().to_string()
        } else {
            t!("Kein Zugriff – nur mit Administratorrechten lesbar", "No access – readable only with administrator rights").to_string()
        };
        let clicked = ui
            .push_id(("profile", index), |ui| {
                widgets::option_row(ui, *selected == index, Mark::Radio, Icon::User, &title, &description).clicked()
            })
            .inner;
        if clicked && *selected != index {
            *selected = index;
            changed = true;
        }
    }
    if profiles.iter().any(|p| !p.accessible) {
        warning_line(
            ui,
            t!(
                "Für fremde Profile ZEBack als Administrator starten (Rechtsklick → „Als Administrator ausführen“).",
                "For other users' profiles, start ZEBack as administrator (right-click → “Run as administrator”)."
            ),
        );
    }
    changed
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let ctx = cc.egui_ctx.clone();
        theme::install(&ctx);
        // Titelleiste des Fensters passend zum dunklen Design.
        ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(egui::SystemTheme::Dark));

        const LOGO_SIZE: u32 = 128;
        let logo = ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied(
                [LOGO_SIZE as usize, LOGO_SIZE as usize],
                &logo::render_rgba(LOGO_SIZE),
            ),
            egui::TextureOptions::LINEAR,
        );

        let cfg = Config::load();
        let mail_password = cfg.mail.password_key().and_then(|key| config::load_password(&key)).unwrap_or_default();
        // Gemerkte Passwörter der Ziele aus der Windows-Anmeldeinformationsverwaltung holen.
        let target_passwords: HashMap<u64, String> = cfg
            .targets
            .iter()
            .filter(|t| t.save_password)
            .filter_map(|t| Some((t.id, config::load_password(&t.password_key()?)?)))
            .collect();

        let repaint = |ctx: &egui::Context| {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let sizes = SizeScanner::new(repaint(&ctx));
        let disk_target = DiskProbe::new(repaint(&ctx));
        let disk_temp = DiskProbe::new(repaint(&ctx));

        let mut app = Self {
            ctx,
            cfg,
            logo,
            user_name: std::env::var("USERNAME").unwrap_or_else(|_| t!("Benutzer", "User").into()),
            computer_name: std::env::var("COMPUTERNAME").unwrap_or_default(),
            profiles: profiles::list(),
            tab: Tab::Backup,
            page_changed: 0.0,
            slide: 0.0,
            backup_step: 0,
            backup_mode: BackupMode::Profile,
            elevated: sysimage::is_elevated(),
            drives: Vec::new(),
            drive_source: 0,
            drive_excludes: Vec::new(),
            image_include: Vec::new(),
            image_drive: None,
            image_smb: None,
            job_edit: 0,
            job_delete: None,
            mail_password,
            history: jobs::load_history(),
            facts: None,
            facts_rx: None,
            update: None,
            update_rx: None,
            images: None,
            images_rx: None,
            backup_profile: 0,
            std_rows: Vec::new(),
            opt_rows: Vec::new(),
            target_passwords,
            target_edit: 0,
            target_delete: None,
            backup_password: String::new(),
            backup_password_repeat: String::new(),
            sizes,
            disk_target,
            disk_temp,
            restore_password: String::new(),
            restore_password_ok: None,
            restore_step: 0,
            archive: None,
            root_checked: Vec::new(),
            restore_mode: RestoreMode::Profile,
            restore_profile: 0,
            restore_dir: String::new(),
            conflict: Conflict::Skip,
            found_backups: None,
            tree: None,
            picked: HashSet::new(),
            browse_dir: None,
            chain: None,
            running: None,
            status: String::new(),
            progress: None,
            progress_percent: false,
            rate: RateMeter::default(),
            started_at: 0.0,
            log: Vec::new(),
            unseen_warnings: 0,
            result: None,
            result_at: 0.0,
        };
        app.reload_std_rows();
        app.refresh_drives();

        // Beim Neustart mit Administratorrechten wird die gewählte Sicherungsart mitgegeben.
        for argument in std::env::args().skip(1) {
            match argument.as_str() {
                "--mode=drive" => app.backup_mode = BackupMode::Drive,
                "--mode=image" => app.backup_mode = BackupMode::Image,
                _ => {}
            }
        }

        // Nur in Entwicklungs-Builds: Startseite, Schritt und Aktionen per Umgebungsvariable,
        // um die Oberfläche ohne Mausklicks prüfen zu können.
        #[cfg(debug_assertions)]
        {
            match std::env::var("ZEBACK_TAB").as_deref() {
                Ok("restore") => app.tab = Tab::Restore,
                Ok("targets") => app.tab = Tab::Targets,
                Ok("jobs") => app.tab = Tab::Jobs,
                Ok("reports") => app.tab = Tab::Reports,
                Ok("log") => app.tab = Tab::Log,
                _ => {}
            }
            if let Some(step) = std::env::var("ZEBACK_STEP").ok().and_then(|s| s.parse::<usize>().ok()) {
                app.backup_step = step.min(LAST_STEP);
                app.restore_step = step.min(LAST_STEP);
            }
            if let Ok(password) = std::env::var("ZEBACK_PASSWORD") {
                app.cfg.encrypt = true;
                app.backup_password = password.clone();
                app.backup_password_repeat = password.clone();
                app.restore_password = password;
            }
            if let Some(dir) = std::env::var_os("ZEBACK_ONLY") {
                // Nur diesen einen Ordner sichern (für Testläufe mit Beispieldaten).
                app.std_rows.iter_mut().for_each(|r| r.checked = false);
                app.opt_rows.iter_mut().for_each(|r| r.checked = false);
                app.cfg.extra_folders = vec![ExtraFolder { path: dir.into(), enabled: true }];
                app.request_sizes();
            }
            if let Ok(dir) = std::env::var("ZEBACK_LOCAL_DIR") {
                // Nur dieses eine Ziel verwenden.
                app.cfg.targets.iter_mut().for_each(|t| t.selected = false);
                let id = app.cfg.next_target_id();
                app.cfg.targets.push(Target {
                    id,
                    name: "Testziel".into(),
                    kind: TargetKind::Local,
                    path: dir,
                    selected: true,
                    ..Target::default()
                });
            }
            if let Ok(edit) = std::env::var("ZEBACK_DEMO_TARGETS") {
                // Beispielziele für Bildschirmfotos der Seite "Sicherungsziele".
                app.cfg.targets.clear();
                for (index, (name, kind, path, url, user)) in [
                    ("USB-Festplatte", TargetKind::Local, "E:\\Backups", "", ""),
                    ("NAS im Büro", TargetKind::Smb, "\\\\nas\\backup\\pc01", "", "backup"),
                    ("Nextcloud", TargetKind::Nextcloud, "", "https://cloud.example.de", "anna"),
                ]
                .into_iter()
                .enumerate()
                {
                    app.cfg.targets.push(Target {
                        id: index as u64 + 1,
                        name: name.into(),
                        kind,
                        path: path.into(),
                        url: url.into(),
                        user: user.into(),
                        remote_dir: "Backups".into(),
                        save_password: true,
                        selected: index != 1,
                        ..Target::default()
                    });
                }
                app.target_edit = edit.parse().unwrap_or(2);
            }
            if let Some(index) = std::env::var("ZEBACK_EDIT").ok().and_then(|s| s.parse::<usize>().ok()) {
                // Dieses Ziel bzw. diesen Auftrag auf der jeweiligen Seite zum Bearbeiten öffnen.
                app.target_edit = index;
                app.job_edit = index;
            }
            if std::env::var_os("ZEBACK_SEARCH").is_some() {
                // Sofort auf den Sicherungszielen suchen (für Bildschirmfotos des Dialogs).
                app.tab = Tab::Restore;
                app.start_backup_search();
            }
            if let Some(path) = std::env::var_os("ZEBACK_OPEN") {
                app.open_archive(path.into());
            }
            if std::env::var_os("ZEBACK_AUTORUN").is_some() {
                app.backup_step = LAST_STEP;
                app.start_backup();
            }
            if let Ok(version) = std::env::var("ZEBACK_UPDATE") {
                // So tun, als läge diese Version bereit (für Bildschirmfotos des Hinweises).
                app.update = Some(Release {
                    version,
                    url: crate::update::DOWNLOAD_URL.to_string(),
                    notes: "Schnellere Sicherungen und kleinere Korrekturen.".to_string(),
                });
            }
        }
        // Zeitpläne, die noch unter dem früheren Programmnamen eingetragen sind, umziehen.
        if !app.no_save()
            && app.cfg.jobs.iter().any(|job| job.schedule != ScheduleKind::Off)
            && let Ok(exe) = std::env::current_exe()
        {
            let moved = schedule::adopt_previous(&exe, &app.cfg.jobs);
            if moved > 0 {
                app.push_log(
                    false,
                    tr!(
                        "{moved} Zeitplan/Zeitpläne vom früheren Programmnamen übernommen.",
                        "Took over {moved} schedule(s) from the previous program name."
                    ),
                );
            }
        }
        // Beim Start nach einer neueren Version fragen – nicht bei Testläufen der Entwicklung.
        if app.cfg.check_updates && !app.no_save() {
            app.start_update_check(false);
        }
        app
    }

    fn now(&self) -> f64 {
        self.ctx.input(|i| i.time)
    }

    fn switch_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.page_changed = self.now();
            self.slide = 0.0;
        }
        if tab == Tab::Log {
            self.unseen_warnings = 0;
        }
    }

    /// Wechselt im geführten Ablauf der aktuellen Seite zu einem Schritt.
    fn go_to_step(&mut self, step: usize) {
        let step = step.min(LAST_STEP);
        let current = match self.tab {
            Tab::Backup => &mut self.backup_step,
            Tab::Restore => &mut self.restore_step,
            _ => return,
        };
        if *current == step {
            return;
        }
        self.slide = if step > *current { 1.0 } else { -1.0 };
        *current = step;
        self.page_changed = self.now();
        self.result = None;
    }

    /// Standardordner des gewählten Profils neu einlesen (Häkchen laut Einstellungen).
    fn reload_std_rows(&mut self) {
        let Some(profile) = self.profiles.get(self.backup_profile) else { return };
        self.std_rows = profile
            .folders
            .iter()
            .map(|f| StdRow { checked: !self.cfg.unchecked_standard.iter().any(|k| k == f.key), folder: f.clone() })
            .collect();
        self.opt_rows = profile
            .optional
            .iter()
            .map(|f| OptRow { checked: self.cfg.checked_optional.iter().any(|k| k == f.key), folder: f.clone() })
            .collect();
        // Messungen des vorherigen Profils abbrechen, die neuen Ordner anfordern.
        self.sizes.cancel_pending();
        self.request_sizes();
    }

    /// Fordert die Größe aller angebotenen Ordner an (bereits bekannte werden nicht neu gemessen).
    fn request_sizes(&mut self) {
        for row in &self.std_rows {
            self.sizes.request(&row.folder.path, &[]);
        }
        for row in &self.opt_rows {
            self.sizes.request(&row.folder.path, row.folder.exclude);
        }
        for folder in &self.cfg.extra_folders {
            self.sizes.request(&folder.path, &[]);
        }
    }

    /// Größe eines Ordners für die Anzeige; "…" solange noch gerechnet wird.
    fn size_label(&self, path: &Path) -> Option<String> {
        Some(match self.sizes.get(path)? {
            Some(info) => fmt_bytes(info.bytes_to_back_up(self.cfg.skip_online_only)),
            None => "…".to_string(),
        })
    }

    /// Belegter Platz der Laufwerke, die ins Systemabbild kommen.
    fn image_size(&self) -> u64 {
        self.drives
            .iter()
            .filter(|d| d.system || self.image_include.iter().any(|l| l == d.letter()))
            .map(Drive::used)
            .sum()
    }

    /// Datenmenge der ausgewählten Quelle; `true`, wenn noch nicht alles gemessen ist.
    fn selected_size(&self) -> (u64, bool) {
        match self.backup_mode {
            BackupMode::Image => return (self.image_size(), false),
            BackupMode::Drive => {
                let measured = self
                    .source_drive()
                    .and_then(|d| self.sizes.get_with(Path::new(&d.root), &self.drive_excludes))
                    .flatten();
                return match measured {
                    Some(info) => (info.bytes_to_back_up(self.cfg.skip_online_only), false),
                    None => (0, true),
                };
            }
            BackupMode::Profile => {}
        }
        let standard = self.std_rows.iter().filter(|r| r.checked).map(|r| &r.folder.path);
        let optional = self.opt_rows.iter().filter(|r| r.checked).map(|r| &r.folder.path);
        let extra = self.cfg.extra_folders.iter().filter(|f| f.enabled).map(|f| &f.path);
        let (mut bytes, mut pending) = (0u64, false);
        for path in standard.chain(optional).chain(extra) {
            match self.sizes.get(path) {
                Some(Some(info)) => bytes += info.bytes_to_back_up(self.cfg.skip_online_only),
                _ => pending = true,
            }
        }
        (bytes, pending)
    }

    fn selected_size_text(&self) -> String {
        match self.selected_size() {
            (bytes, false) => fmt_bytes(bytes),
            (0, true) => t!("wird noch berechnet …", "still calculating …").to_string(),
            (bytes, true) => tr!("über {} (wird noch berechnet)", "over {} (still calculating)", fmt_bytes(bytes)),
        }
    }

    // ------------------------------------------------------------------ Hintergrund-Vorgänge

    fn start<F>(&mut self, title: &str, work: F)
    where
        F: FnOnce(&Reporter) -> Result<Outcome> + Send + 'static,
    {
        let ctx = self.ctx.clone();
        let (rep, rx, cancel) = Reporter::new(move || ctx.request_repaint());
        self.status = format!("{title} …");
        self.progress = None;
        self.progress_percent = false;
        self.rate.clear();
        self.started_at = self.now();
        self.result = None;
        self.push_log(false, tr!("[{}] {title} gestartet", "[{}] {title} started", Local::now().format("%H:%M:%S")));
        let handle = std::thread::spawn(move || {
            // Solange der Vorgang läuft, geht der PC nicht von selbst in den Ruhezustand.
            let _awake = sysimage::KeepAwake::new();
            work(&rep)
        });
        self.running = Some(Running { rx, cancel, handle });
    }

    fn push_log(&mut self, warn: bool, text: String) {
        if self.log.len() >= MAX_LOG_LINES {
            self.log.drain(..MAX_LOG_LINES / 4);
        }
        if warn && self.tab != Tab::Log {
            self.unseen_warnings += 1;
        }
        self.log.push(LogLine { warn, text });
    }

    fn set_result(&mut self, level: Level, text: impl Into<String>) {
        self.result = Some((level, text.into()));
        self.result_at = self.now();
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.set_result(Level::Error, text);
    }

    fn drain_messages(&mut self) {
        let Some(running) = &self.running else { return };
        let messages: Vec<Msg> = running.rx.try_iter().collect();
        let now = self.now();
        for msg in messages {
            match msg {
                Msg::Status(text) => self.status = text,
                Msg::Progress { done, total } => {
                    self.progress = Some((done, total));
                    self.rate.add(now, done);
                }
                Msg::Log(text) => self.push_log(false, text),
                Msg::Warn(text) => self.push_log(true, text),
            }
        }
    }

    /// Ermittelt einmalig im Hintergrund, was vor einem Systemabbild wissenswert ist.
    /// Fragt im Hintergrund beim Hersteller nach einer neueren Version. `manual`: vom Benutzer
    /// angestoßen – dann wird auch "ist aktuell" oder ein Fehler gemeldet, sonst nur ein Fund.
    fn start_update_check(&mut self, manual: bool) {
        if self.update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::update::check().map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
        self.update_rx = Some((rx, manual));
    }

    fn open_download_page(&mut self, release: &Release) {
        if let Err(e) = sysimage::open_in_browser(&release.url) {
            self.fail(tr!("Die Download-Seite konnte nicht geöffnet werden\n{e:#}", "Could not open the download page\n{e:#}"));
        }
    }

    fn ensure_system_facts(&mut self) {
        if self.facts.is_some() || self.facts_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(sysimage::system_facts());
            ctx.request_repaint();
        });
        self.facts_rx = Some(rx);
    }

    /// Sucht im Hintergrund nach vorhandenen Abbildern auf dem Ziel (einmal pro Ziel).
    fn ensure_images(&mut self, target: &str) {
        if self.images.as_ref().is_some_and(|(known, _)| known == target) || self.images_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctx, target) = (self.ctx.clone(), target.to_string());
        std::thread::spawn(move || {
            let images = sysimage::list_images(&target);
            let _ = tx.send((target, images));
            ctx.request_repaint();
        });
        self.images_rx = Some(rx);
    }

    fn open_recovery_wizard(&mut self) {
        match sysimage::open_recovery_drive_wizard() {
            Ok(()) => self.set_result(
                Level::Ok,
                t!(
                    "Windows-Assistent geöffnet\nFolge dem Assistenten „Wiederherstellungslaufwerk erstellen“. Der USB-Stick wird dabei gelöscht.",
                    "Windows wizard opened\nFollow the “Create a recovery drive” wizard. The USB stick will be erased in the process."
                ),
            ),
            Err(e) => self.fail(tr!("Assistent nicht gestartet\n{e:#}", "Wizard not started\n{e:#}")),
        }
    }

    fn poll(&mut self) {
        if let Some(facts) = self.facts_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.facts = Some(facts);
            self.facts_rx = None;
        }
        if let Some((answer, manual)) = self.update_rx.as_ref().and_then(|(rx, manual)| Some((rx.try_recv().ok()?, *manual))) {
            self.update_rx = None;
            match answer {
                Ok(Some(release)) => {
                    if manual {
                        self.set_result(
                            Level::Ok,
                            tr!(
                                "Version {} ist verfügbar\nDu verwendest {}. Der Hinweis unten links führt zur Download-Seite.",
                                "Version {} is available\nYou are using {}. The note at the bottom left opens the download page.",
                                release.version,
                                env!("CARGO_PKG_VERSION")
                            ),
                        );
                    }
                    self.update = Some(release);
                }
                Ok(None) if manual => self.set_result(
                    Level::Ok,
                    tr!("ZEBack ist aktuell\nVersion {} ist die neueste.", "ZEBack is up to date\nVersion {} is the latest.", env!("CARGO_PKG_VERSION")),
                ),
                Err(e) if manual => self.fail(tr!("Update-Abfrage nicht möglich\n{e}", "Could not check for updates\n{e}")),
                // Eine stille Abfrage beim Start meldet nichts, wenn sie scheitert oder nichts findet.
                _ => {}
            }
        }
        if let Some(images) = self.images_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.images = Some(images);
            self.images_rx = None;
        }
        self.sizes.poll();
        self.drain_messages();
        if !self.running.as_ref().is_some_and(|r| r.handle.is_finished()) {
            if self.running.is_some() {
                self.ctx.request_repaint_after(Duration::from_millis(100));
            }
            return;
        }
        let running = self.running.take().expect("geprüft");
        let outcome = running.handle.join();
        for msg in running.rx.try_iter() {
            match msg {
                Msg::Log(text) => self.push_log(false, text),
                Msg::Warn(text) => self.push_log(true, text),
                _ => {}
            }
        }
        self.progress = None;
        // Ein Auftrag hat sein Ergebnis in den Verlauf geschrieben.
        self.history = jobs::load_history();
        let (level, text) = match outcome {
            Ok(Ok(outcome)) => self.finish(outcome),
            Ok(Err(e)) if e.is::<Cancelled>() => (Level::Warn, t!("Vorgang abgebrochen.", "Operation cancelled.").to_string()),
            Ok(Err(e)) => (Level::Error, tr!("Das hat nicht geklappt\n{e:#}", "That did not work\n{e:#}")),
            Err(_) => (
                Level::Error,
                t!(
                    "Interner Fehler\nDer Vorgang wurde unerwartet beendet.",
                    "Internal error\nThe operation ended unexpectedly."
                )
                .to_string(),
            ),
        };
        self.push_log(level == Level::Error, text.replace('\n', " – "));
        self.set_result(level, text);
    }

    fn finish(&mut self, outcome: Outcome) -> (Level, String) {
        match outcome {
            Outcome::Backup(s) if s.kind == BackupKind::Unchanged => (
                Level::Ok,
                t!(
                    "Keine Änderungen\nSeit der letzten Sicherung hat sich nichts geändert – es wurde keine neue Datei angelegt.",
                    "No changes\nNothing has changed since the last backup – no new file was created."
                )
                .to_string(),
            ),
            Outcome::Backup(s) => {
                let mut text = tr!(
                    "{} abgeschlossen\n{} Dateien ({}) als ZIP mit {} in {}.",
                    "{} complete\n{} files ({}) packed into a {} ZIP in {}.",
                    if s.kind == BackupKind::Partial { t!("Teilsicherung", "Partial backup") } else { t!("Backup", "Backup") },
                    s.files,
                    fmt_bytes(s.bytes),
                    fmt_bytes(s.zip_size),
                    fmt_duration(s.duration)
                );
                for place in &s.saved {
                    text.push_str(&tr!("\nGespeichert – {place}", "\nSaved – {place}"));
                }
                if s.encrypted {
                    text.push_str(t!(
                        "\nVerschlüsselt mit AES-256 – ohne das Passwort ist die Sicherung nicht wiederherstellbar.",
                        "\nEncrypted with AES-256 – without the password the backup cannot be restored."
                    ));
                }
                if s.verified {
                    text.push_str(t!(
                        "\nGeprüft: Die ZIP-Datei wurde vollständig gelesen, alle Prüfsummen stimmen.",
                        "\nVerified: the ZIP file was read completely, all checksums match."
                    ));
                }
                if s.skipped_online > 0 {
                    text.push_str(&tr!(
                        "\n{} Dateien liegen nur online (OneDrive) und wurden ausgelassen.",
                        "\n{} files are online-only (OneDrive) and were skipped.",
                        s.skipped_online
                    ));
                }
                if s.skipped_errors > 0 {
                    text.push_str(&tr!(
                        "\nAchtung: {} Dateien/Ordner konnten nicht gelesen werden (siehe Protokoll).",
                        "\nWarning: {} files/folders could not be read (see log).",
                        s.skipped_errors
                    ));
                    return (Level::Warn, text);
                }
                (Level::Ok, text)
            }
            Outcome::Restore(s) => {
                let mut text = tr!(
                    "Wiederherstellung abgeschlossen\n{} Dateien ({}) in {}.",
                    "Restore complete\n{} files ({}) in {}.",
                    s.restored,
                    fmt_bytes(s.bytes),
                    fmt_duration(s.duration)
                );
                if s.skipped_existing > 0 {
                    text.push_str(&tr!(
                        "\n{} vorhandene Dateien wurden beibehalten.",
                        "\n{} existing files were kept.",
                        s.skipped_existing
                    ));
                }
                if s.failed > 0 {
                    text.push_str(&tr!(
                        "\nAchtung: {} Einträge konnten nicht wiederhergestellt werden (siehe Protokoll).",
                        "\nWarning: {} entries could not be restored (see log).",
                        s.failed
                    ));
                    return (Level::Warn, text);
                }
                (Level::Ok, text)
            }
            Outcome::TargetTest(text) => (Level::Ok, text),
            Outcome::NcLogin(target_id, login) => {
                let Some(target) = self.cfg.targets.iter_mut().find(|t| t.id == target_id) else {
                    return (
                        Level::Warn,
                        t!(
                            "Anmeldung bestätigt, aber das Ziel wurde inzwischen gelöscht.",
                            "Sign-in confirmed, but the target has been deleted in the meantime."
                        )
                        .to_string(),
                    );
                };
                // Das alte gemerkte Passwort gehört zu den bisherigen Angaben – nicht liegen lassen.
                let old_key = target.password_key();
                target.url = login.server;
                target.user = login.user;
                let text = tr!(
                    "Angemeldet als „{}“\nNextcloud hat ein App-Passwort für dieses Gerät erstellt; es ist bei „{}“ eingetragen{}.",
                    "Signed in as “{}”\nNextcloud has created an app password for this device; it has been entered for “{}”{}.",
                    target.user,
                    target.name,
                    if target.save_password { t!(" und gemerkt", " and remembered") } else { "" }
                );
                let new_key = target.password_key();
                self.target_passwords.insert(target_id, login.app_password);
                #[cfg(debug_assertions)]
                let keep_stored = std::env::var_os("ZEBACK_NO_SAVE").is_some();
                #[cfg(not(debug_assertions))]
                let keep_stored = false;
                if !keep_stored && old_key != new_key && let Some(key) = old_key {
                    config::delete_password(&key);
                }
                self.persist();
                self.persist_target_passwords();
                (Level::Ok, text)
            }
            Outcome::Image(s) => (
                Level::Ok,
                tr!(
                    "Systemabbild erstellt\nGespeichert im Ordner „WindowsImageBackup“ auf {} in {}.\nZurückgespielt wird es über die Windows-Wiederherstellungsumgebung (siehe „Wiederherstellen“).",
                    "System image created\nSaved in the “WindowsImageBackup” folder on {} in {}.\nIt is restored through the Windows Recovery Environment (see “Restore”).",
                    s.target,
                    fmt_duration(s.duration)
                ),
            ),
            Outcome::Verified(report) => (
                Level::Ok,
                tr!(
                    "Sicherung ist in Ordnung\n{} Dateien ({}) gelesen, alle Prüfsummen stimmen.",
                    "Backup is intact\n{} files ({}) read, all checksums match.",
                    report.files,
                    fmt_bytes(report.bytes)
                ),
            ),
            Outcome::Found(files) => {
                let text = match files.len() {
                    0 => t!(
                        "Keine Sicherungen gefunden\nAuf den erreichbaren Sicherungszielen liegt keine ZIP-Datei. Nicht erreichbare Ziele stehen im Protokoll.",
                        "No backups found\nThere is no ZIP file on the reachable backup targets. Unreachable targets are listed in the log."
                    )
                    .to_string(),
                    n => tr!(
                        "{n} Sicherung(en) auf deinen Sicherungszielen gefunden.",
                        "{n} backup(s) found on your backup targets."
                    ),
                };
                let level = if files.is_empty() { Level::Warn } else { Level::Ok };
                self.found_backups = (!files.is_empty()).then_some(files);
                (level, text)
            }
            Outcome::Listed(tree) => {
                self.tree = Some(Arc::new(tree));
                self.browse_dir = Some(String::new());
                // Entwicklungs-Builds: Ordner und Auswahl für Bildschirmfotos vorgeben.
                #[cfg(debug_assertions)]
                if let Ok(dir) = std::env::var("ZEBACK_BROWSE") {
                    self.browse_dir = Some(dir);
                    let picks = std::env::var("ZEBACK_PICK").unwrap_or_default();
                    self.picked.extend(picks.split(';').filter(|p| !p.is_empty()).map(str::to_string));
                    // … und auf Wunsch die Auswahl gleich in einen Ordner wiederherstellen.
                    if let Ok(dir) = std::env::var("ZEBACK_RESTORE_TO") {
                        self.browse_dir = None;
                        self.restore_mode = RestoreMode::Folder;
                        self.restore_dir = dir;
                        self.start_restore();
                    }
                }
                (Level::Ok, t!("Inhalt der Sicherung gelesen.", "Backup contents read.").to_string())
            }
            Outcome::Inspected(info) => {
                let mut text = tr!("Sicherung geladen\n{}", "Backup loaded\n{}", info.path.display());
                if info.encrypted {
                    text.push_str(t!(
                        "\nDiese Sicherung ist verschlüsselt – bitte das Passwort eingeben.",
                        "\nThis backup is encrypted – please enter the password."
                    ));
                }
                // Passwort einer vorher geöffneten Sicherung nicht weiterverwenden
                // (in Entwicklungs-Builds kann es per Umgebungsvariable vorbelegt sein).
                if self.archive.is_some() {
                    self.restore_password.clear();
                }
                self.restore_password_ok = None;
                self.forget_file_selection();
                self.chain = restore::chain_for(&info);
                self.root_checked = vec![true; info.manifest.as_ref().map_or(0, |m| m.roots.len())];
                self.restore_mode = if info.fits_profile() { RestoreMode::Profile } else { RestoreMode::Folder };
                self.archive = Some(info);
                self.switch_tab(Tab::Restore);
                #[cfg(debug_assertions)]
                if std::env::var_os("ZEBACK_BROWSE").is_some() {
                    self.start_browse();
                }
                (Level::Ok, text)
            }
        }
    }

    // ------------------------------------------------------------------ Einstellungen

    fn persist(&mut self) {
        // Testläufe in Entwicklungs-Builds sollen die echten Einstellungen nicht überschreiben.
        #[cfg(debug_assertions)]
        if std::env::var_os("ZEBACK_NO_SAVE").is_some() {
            return;
        }
        // Nur Ordner merken, die im aktuell gewählten Profil sichtbar sind; den Rest beibehalten.
        let visible: Vec<&str> = self.std_rows.iter().map(|r| r.folder.key).collect();
        self.cfg.unchecked_standard.retain(|k| !visible.contains(&k.as_str()));
        self.cfg
            .unchecked_standard
            .extend(self.std_rows.iter().filter(|r| !r.checked).map(|r| r.folder.key.to_string()));
        let visible: Vec<&str> = self.opt_rows.iter().map(|r| r.folder.key).collect();
        self.cfg.checked_optional.retain(|k| !visible.contains(&k.as_str()));
        self.cfg
            .checked_optional
            .extend(self.opt_rows.iter().filter(|r| r.checked).map(|r| r.folder.key.to_string()));
        if let Err(e) = self.cfg.save() {
            self.push_log(true, format!("{e:#}"));
        }
    }

    // ------------------------------------------------------------------ Sicherungsziele

    fn target_password(&self, target: &Target) -> String {
        self.target_passwords.get(&target.id).cloned().unwrap_or_default()
    }

    /// Legt die Passwörter aller Ziele in der Windows-Anmeldeinformationsverwaltung ab
    /// (oder entfernt sie, wenn "Passwort merken" aus ist).
    fn persist_target_passwords(&mut self) {
        #[cfg(debug_assertions)]
        if std::env::var_os("ZEBACK_NO_SAVE").is_some() {
            return;
        }
        let mut problems = Vec::new();
        for target in &self.cfg.targets {
            let Some(key) = target.password_key() else { continue };
            let password = self.target_passwords.get(&target.id).map(String::as_str).unwrap_or_default();
            if target.save_password && !password.is_empty() {
                if let Err(e) = config::save_password(&key, password) {
                    problems.push(format!("{e:#}"));
                }
            } else {
                config::delete_password(&key);
            }
        }
        for problem in problems {
            self.push_log(true, problem);
        }
    }

    /// Ist das Ziel vollständig eingerichtet? Sonst eine Meldung, was fehlt.
    fn check_target(&self, target: &Target) -> Result<(), String> {
        jobs::check_target(target, &self.target_password(target))
    }

    /// Macht aus einem eingerichteten Ziel das, was die Sicherung braucht.
    fn destination(&self, target: &Target) -> Result<Destination, String> {
        jobs::destination(target, &self.target_password(target))
    }

    // ------------------------------------------------------------------ Aufträge

    /// Speichert die aktuelle Auswahl des Sichern-Ablaufs als Auftrag und öffnet ihn.
    fn save_as_job(&mut self) {
        let prepared = (|| {
            let (profile_name, profile_dir) = self.backup_identity()?;
            let sources = self.check_backup_sources()?;
            self.check_backup_target()?;
            let password = self.check_backup_password()?;
            Ok::<_, String>((profile_name, profile_dir, sources, password))
        })();
        let (profile_name, profile_dir, sources, password) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => return self.fail(message),
        };
        let id = self.cfg.next_job_id();
        let name = if self.backup_mode == BackupMode::Drive {
            // Der Name im Archiv bleibt deutsch ("Laufwerk-C"); der Auftrag heißt in der Sprache der Oberfläche.
            t!(profile_name.replace('-', " "), profile_name.replace("Laufwerk-", "Drive "))
        } else {
            tr!("Profil {profile_name}", "Profile {profile_name}")
        };
        let job = SavedJob {
            id,
            name,
            profile_name,
            profile_dir,
            sources,
            target_ids: self.cfg.targets.iter().filter(|t| t.selected).map(|t| t.id).collect(),
            compression: self.cfg.compression,
            skip_online_only: self.cfg.skip_online_only,
            verify: self.cfg.verify_after_backup,
            encrypt: password.is_some(),
            ..SavedJob::default()
        };
        // Ein Auftrag soll ohne Nachfrage laufen können: sein Verschlüsselungs-Passwort kommt
        // deshalb in die Windows-Anmeldeinformationsverwaltung.
        if let Some(password) = &password
            && !self.no_save()
            && let Err(e) = config::save_password(&job.password_key(), password)
        {
            return self.fail(tr!("Passwort konnte nicht gespeichert werden\n{e:#}", "Password could not be saved\n{e:#}"));
        }
        self.cfg.jobs.push(job);
        self.job_edit = self.cfg.jobs.len() - 1;
        self.persist();
        self.persist_target_passwords();
        self.switch_tab(Tab::Jobs);
        self.set_result(
            Level::Ok,
            t!(
                "Auftrag gespeichert\nHier legst du fest, wie oft er läuft und wie viele Sicherungen aufbewahrt werden.",
                "Job saved\nHere you set how often it runs and how many backups are kept."
            ),
        );
    }

    /// Entwicklungs-Builds: Bei Testläufen nichts dauerhaft speichern.
    fn no_save(&self) -> bool {
        cfg!(debug_assertions) && std::env::var_os("ZEBACK_NO_SAVE").is_some()
    }

    /// Führt den Auftrag an der Position `index` jetzt aus.
    fn start_job(&mut self, index: usize) {
        let Some(saved) = self.cfg.jobs.get(index).cloned() else { return };
        let job_password = saved.encrypt.then(|| config::load_password(&saved.password_key())).flatten();
        let password_of = |target: &Target| self.target_password(target);
        let job = match jobs::build(&saved, &self.cfg, &password_of, job_password) {
            Ok(job) => job,
            Err(message) => return self.fail(message),
        };
        let mail = self.mail_report();
        self.start(&tr!("Auftrag „{}“", "Job “{}”", saved.name), move |rep| {
            jobs::execute(&saved, &job, &mail, rep).map(Outcome::Backup)
        });
    }

    /// Der eingerichtete E-Mail-Bericht mit dem Passwort aus dem Eingabefeld.
    fn mail_report(&self) -> jobs::MailReport {
        let settings = &self.cfg.mail;
        (settings.mode != ReportMode::Never && !settings.server.trim().is_empty())
            .then(|| (settings.mode, jobs::mail_from(settings, self.mail_password.clone())))
    }

    /// Speichert die Einstellungen der Berichte; das Passwort des Postfachs kommt in die
    /// Windows-Anmeldeinformationsverwaltung.
    fn save_reports(&mut self) {
        self.persist();
        if !self.no_save()
            && let Some(key) = self.cfg.mail.password_key()
        {
            if self.mail_password.is_empty() {
                config::delete_password(&key);
            } else if let Err(e) = config::save_password(&key, &self.mail_password) {
                return self.fail(tr!("Passwort konnte nicht gespeichert werden\n{e:#}", "Password could not be saved\n{e:#}"));
            }
        }
        self.set_result(Level::Ok, t!("Berichte gespeichert.", "Reports saved."));
    }

    /// Sendet eine Testnachricht mit den eingetragenen Angaben.
    fn start_test_mail(&mut self) {
        let mail = jobs::mail_from(&self.cfg.mail, self.mail_password.clone());
        self.start(t!("Testnachricht wird gesendet", "Sending test message"), move |_| {
            let (subject, body) = jobs::report_text(
                t!("Testnachricht", "Test message"),
                true,
                t!(
                    "Wenn du das liest, funktioniert der E-Mail-Bericht von ZEBack.",
                    "If you are reading this, ZEBack's email report is working."
                ),
                &[],
            );
            crate::mail::send(&mail, &subject, &body)?;
            Ok(Outcome::TargetTest(tr!(
                "Testnachricht gesendet\nSie ist unterwegs an {}.",
                "Test message sent\nIt is on its way to {}.",
                mail.to
            )))
        });
    }

    /// Speichert den Auftrag und trägt seinen Zeitplan in die Windows-Aufgabenplanung ein.
    fn save_job(&mut self, index: usize) {
        let Some(job) = self.cfg.jobs.get_mut(index) else { return };
        if job.name.trim().is_empty() {
            job.name = tr!("Auftrag {}", "Job {}", job.id);
        }
        let job = job.clone();
        self.persist();
        if self.no_save() {
            return self.set_result(
                Level::Ok,
                t!("Auftrag gespeichert (Testlauf: kein Zeitplan eingetragen).", "Job saved (test run: no schedule registered)."),
            );
        }
        let scheduled = std::env::current_exe()
            .map_err(anyhow::Error::from)
            .and_then(|exe| schedule::register(&exe, &job));
        match scheduled {
            Ok(()) if job.schedule == ScheduleKind::Off => {
                self.set_result(
                    Level::Ok,
                    t!(
                        "Auftrag gespeichert\nEr läuft nur, wenn du ihn von Hand startest.",
                        "Job saved\nIt only runs when you start it manually."
                    ),
                );
            }
            Ok(()) => self.set_result(
                Level::Ok,
                tr!(
                    "Auftrag gespeichert\nZeitplan eingetragen: {}. Er läuft, während du an Windows angemeldet bist; ein verpasster Termin wird nachgeholt.",
                    "Job saved\nSchedule registered: {}. It runs while you are signed in to Windows; a missed run is caught up.",
                    job.schedule_text()
                ),
            ),
            Err(e) => self.fail(tr!("Zeitplan nicht eingetragen\n{e:#}", "Schedule not registered\n{e:#}")),
        }
    }

    /// Entfernt den Auftrag samt Zeitplan und gespeichertem Verschlüsselungs-Passwort.
    fn delete_job(&mut self, index: usize) {
        if index >= self.cfg.jobs.len() {
            return;
        }
        let job = self.cfg.jobs.remove(index);
        self.job_delete = None;
        self.job_edit = index.min(self.cfg.jobs.len().saturating_sub(1));
        if !self.no_save() {
            config::delete_password(&job.password_key());
            if let Some(state) = jobs::state_path(job.id) {
                let _ = std::fs::remove_file(state);
            }
            if let Err(e) = schedule::remove(job.id) {
                self.push_log(true, format!("{e:#}"));
            }
        }
        self.persist();
        self.set_result(Level::Ok, tr!("Auftrag „{}“ gelöscht.", "Job “{}” deleted.", job.name));
    }

    /// Legt ein neues Ziel an und gibt seine Position zurück.
    fn add_target(&mut self, kind: TargetKind, path: &str) -> usize {
        let id = self.cfg.next_target_id();
        let folder_name = PathBuf::from(path).file_name().map(|n| n.to_string_lossy().into_owned());
        let name = match kind {
            TargetKind::Local => folder_name
                .unwrap_or_else(|| if path.is_empty() { t!("Neuer Ordner", "New folder").into() } else { path.to_string() }),
            TargetKind::Smb => t!("Netzwerkfreigabe", "Network share").to_string(),
            TargetKind::Nextcloud => "Nextcloud".to_string(),
            TargetKind::WebDav => "WebDAV".to_string(),
            TargetKind::S3 => t!("S3-Speicher", "S3 storage").to_string(),
            TargetKind::Sftp => t!("SFTP-Server", "SFTP server").to_string(),
        };
        self.cfg.targets.push(Target {
            id,
            name,
            kind,
            path: path.to_string(),
            remote_dir: if kind.is_cloud() { "Backups".into() } else { String::new() },
            save_password: true,
            selected: true,
            ..Target::default()
        });
        self.cfg.targets.len() - 1
    }

    /// Prüft das Ziel an der Position `index`: Ordner erreichbar, Anmeldung möglich.
    fn start_target_test(&mut self, index: usize) {
        let Some(target) = self.cfg.targets.get(index).cloned() else { return };
        let destination = match self.destination(&target) {
            Ok(destination) => destination,
            Err(message) => return self.fail(message),
        };
        self.start(&tr!("Ziel „{}“ wird geprüft", "Checking target “{}”", target.name), move |_| {
            let text = match destination {
                Destination::Cloud { name, settings } => {
                    tr!("Ziel „{name}“ ist erreichbar\n{}", "Target “{name}” is reachable\n{}", settings.connect()?.describe())
                }
                Destination::Folder { name, path, login } => {
                    if let Some(login) = &login {
                        crate::smb::connect(&path.to_string_lossy(), &login.user, &login.password)?;
                    }
                    if !path.is_dir() {
                        anyhow::bail!(
                            "{}",
                            tr!("Der Ordner {} ist nicht erreichbar.", "The folder {} is not reachable.", path.display())
                        );
                    }
                    match disk::info(&path) {
                        Some(info) => tr!(
                            "Ziel „{name}“ ist erreichbar\n{} frei von {}{}.",
                            "Target “{name}” is reachable\n{} free of {}{}.",
                            fmt_bytes(info.free),
                            fmt_bytes(info.total),
                            if info.file_system.is_empty() { String::new() } else { format!(" · {}", info.file_system) }
                        ),
                        None => tr!("Ziel „{name}“ ist erreichbar", "Target “{name}” is reachable"),
                    }
                }
            };
            Ok(Outcome::TargetTest(text))
        });
    }

    /// Meldet das Nextcloud-Ziel an der Position `index` über den Browser an: Die Anmeldeseite
    /// des Servers öffnet sich, nach der Bestätigung dort liefert Nextcloud ein App-Passwort.
    fn start_browser_login(&mut self, index: usize) {
        let Some(target) = self.cfg.targets.get(index) else { return };
        let (id, url) = (target.id, target.url.trim().to_string());
        if url.is_empty() {
            return self.fail(t!(
                "Server-Adresse fehlt\nTrage zuerst die Adresse deiner Nextcloud ein, z. B. https://cloud.example.de.",
                "Server address missing\nFirst enter the address of your Nextcloud, e.g. https://cloud.example.de."
            ));
        }
        self.start(t!("Anmeldung im Browser", "Sign-in in the browser"), move |rep| {
            let login = nextcloud::login_in_browser(&url, sysimage::open_in_browser, rep)?;
            Ok(Outcome::NcLogin(id, login))
        });
    }

    // ------------------------------------------------------------------ Sichern: Prüfen und Starten

    fn selected_source_count(&self) -> usize {
        if self.backup_mode != BackupMode::Profile {
            return 1;
        }
        self.std_rows.iter().filter(|r| r.checked).count()
            + self.opt_rows.iter().filter(|r| r.checked).count()
            + self.cfg.extra_folders.iter().filter(|f| f.enabled).count()
    }

    /// Das Laufwerk, das als Ganzes gesichert wird (Sicherungsart "Ganzes Laufwerk").
    fn source_drive(&self) -> Option<&Drive> {
        self.drives.get(self.drive_source)
    }

    /// Ausschlüsse für die Laufwerkssicherung neu bestimmen und die Größe anfordern.
    fn refresh_drive_scope(&mut self) {
        let Some(root) = self.source_drive().map(|d| PathBuf::from(&d.root)) else { return };
        self.drive_excludes = backup::drive_excludes(&root, self.cfg.drive_data_only);
        self.sizes.request_with(&root, &[], &self.drive_excludes);
    }

    /// Laufwerke neu einlesen (z. B. nach dem Anstecken einer USB-Festplatte).
    fn refresh_drives(&mut self) {
        let selected = self.source_drive().map(|d| d.root.clone());
        self.drives = disk::list_drives();
        self.drive_source = selected
            .and_then(|root| self.drives.iter().position(|d| d.root == root))
            .or_else(|| self.drives.iter().position(|d| d.system))
            .unwrap_or(0);
        // Verschwundene Laufwerke aus der Abbild-Auswahl nehmen.
        let known = |letter: &String| self.drives.iter().any(|d| d.letter() == letter);
        self.image_include.retain(known);
        if self.image_drive.as_ref().is_some_and(|letter| !known(letter)) {
            self.image_drive = None;
        }
        self.refresh_drive_scope();
    }

    /// Namen aller ausgewählten Ordner (für die Zusammenfassung).
    fn selected_source_names(&self) -> Vec<String> {
        if self.backup_mode == BackupMode::Drive {
            return self.source_drive().map(|d| vec![d.title()]).unwrap_or_default();
        }
        let standard = self.std_rows.iter().filter(|r| r.checked).map(|r| r.folder.title().to_string());
        let optional = self.opt_rows.iter().filter(|r| r.checked).map(|r| r.folder.title().to_string());
        let extra = self.cfg.extra_folders.iter().filter(|f| f.enabled).map(|f| {
            f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| f.path.display().to_string())
        });
        standard.chain(optional).chain(extra).collect()
    }

    fn check_backup_profile(&self) -> Result<&Profile, String> {
        match self.profiles.get(self.backup_profile) {
            Some(profile) if profile.accessible => Ok(profile),
            Some(profile) => Err(tr!(
                "Kein Zugriff auf das Profil „{}“\nStarte ZEBack als Administrator, um fremde Profile zu sichern.",
                "No access to the profile “{}”\nStart ZEBack as administrator to back up other users' profiles.",
                profile.name
            )),
            None => Err(t!("Kein Profil ausgewählt", "No profile selected").to_string()),
        }
    }

    /// Name und Ordner für Dateiname und Inhaltsbeschreibung der Sicherung.
    fn backup_identity(&self) -> Result<(String, PathBuf), String> {
        match self.backup_mode {
            BackupMode::Drive => {
                let drive = self.source_drive().ok_or(t!("Kein Laufwerk ausgewählt", "No drive selected"))?;
                Ok((format!("Laufwerk-{}", drive.letter().trim_end_matches(':')), PathBuf::new()))
            }
            _ => self.check_backup_profile().map(|p| (p.name.clone(), p.dir.clone())),
        }
    }

    fn check_backup_sources(&self) -> Result<Vec<Source>, String> {
        if self.backup_mode == BackupMode::Drive {
            let drive = self
                .source_drive()
                .ok_or(t!("Kein Laufwerk ausgewählt\nBitte ein Laufwerk wählen.", "No drive selected\nPlease choose a drive."))?;
            let root = PathBuf::from(&drive.root);
            let mut exclude_paths = backup::drive_excludes(&root, self.cfg.drive_data_only);
            // Liegt ein Zielordner auf demselben Laufwerk, darf er sich nicht selbst sichern.
            for target in self.cfg.targets.iter().filter(|t| t.selected && t.kind == TargetKind::Local) {
                let path = target.path.trim().trim_end_matches('\\');
                if !path.is_empty() {
                    exclude_paths.push(PathBuf::from(path));
                }
            }
            return Ok(vec![Source {
                path: root,
                kind: Some("drive".to_string()),
                name: Some(format!("Laufwerk {}", drive.letter().trim_end_matches(':'))),
                exclude: Vec::new(),
                exclude_paths,
            }]);
        }
        let mut sources: Vec<Source> = self
            .std_rows
            .iter()
            .filter(|r| r.checked)
            .map(|r| Source::plain(r.folder.path.clone(), Some(r.folder.key.to_string())))
            .collect();
        // Zusatzordner bekommen im Archiv ihren Anzeigenamen ("Chrome" statt "User Data")
        // und lassen Zwischenspeicher aus.
        sources.extend(self.opt_rows.iter().filter(|r| r.checked).map(|r| Source {
            path: r.folder.path.clone(),
            kind: Some(r.folder.key.to_string()),
            name: Some(r.folder.label.to_string()),
            exclude: r.folder.exclude.iter().map(|s| s.to_string()).collect(),
            exclude_paths: Vec::new(),
        }));
        sources.extend(
            self.cfg.extra_folders.iter().filter(|f| f.enabled).map(|f| Source::plain(f.path.clone(), None)),
        );
        if sources.is_empty() {
            return Err(t!(
                "Kein Ordner ausgewählt\nBitte mindestens einen Ordner anhaken.",
                "No folder selected\nPlease tick at least one folder."
            )
            .to_string());
        }
        Ok(sources)
    }

    /// Die angehakten Sicherungsziele, fertig für die Sicherung. Ordner stehen vor Nextcloud,
    /// damit die ZIP-Datei möglichst direkt auf einem Datenträger entsteht.
    fn check_backup_target(&self) -> Result<Vec<Destination>, String> {
        let selected: Vec<&Target> = self.cfg.targets.iter().filter(|t| t.selected).collect();
        if selected.is_empty() {
            return Err(t!(
                "Kein Ziel ausgewählt\nBitte mindestens ein Sicherungsziel anhaken oder ein neues anlegen.",
                "No target selected\nPlease tick at least one backup target or create a new one."
            )
            .to_string());
        }
        jobs::destinations(&selected, &|target| self.target_password(target))
    }

    /// Passwort für die Verschlüsselung; `None`, wenn nicht verschlüsselt wird.
    fn check_backup_password(&self) -> Result<Option<String>, String> {
        if !self.cfg.encrypt {
            return Ok(None);
        }
        if self.backup_password.chars().count() < 8 {
            return Err(t!(
                "Passwort zu kurz\nBitte mindestens 8 Zeichen für die Verschlüsselung verwenden.",
                "Password too short\nPlease use at least 8 characters for the encryption."
            )
            .to_string());
        }
        if self.backup_password != self.backup_password_repeat {
            return Err(t!(
                "Passwörter stimmen nicht überein\nBitte das Passwort zweimal gleich eingeben.",
                "Passwords do not match\nPlease enter the same password twice."
            )
            .to_string());
        }
        Ok(Some(self.backup_password.clone()))
    }

    /// Die für das Systemabbild gewählte Netzwerkfreigabe (aus den Sicherungszielen).
    fn image_share_target(&self) -> Option<&Target> {
        let id = self.image_smb?;
        self.cfg.targets.iter().find(|t| t.id == id && t.kind == TargetKind::Smb)
    }

    /// Ziel des Systemabbilds: gewählte Netzwerkfreigabe oder gewähltes Laufwerk.
    fn image_target(&self) -> Option<String> {
        match self.image_share_target() {
            Some(target) => Some(target.path.trim().to_string()),
            None => self.image_drive.clone(),
        }
    }

    /// Ziel und (bei Freigaben) Anmeldung für das Systemabbild.
    fn check_image_target(&self) -> Result<(String, Option<(String, String)>), String> {
        let login = match self.image_share_target() {
            Some(target) => {
                self.check_target(target)?;
                target.needs_password().then(|| (target.user.trim().to_string(), self.target_password(target)))
            }
            None => None,
        };
        let target = self
            .image_target()
            .ok_or(t!(
                "Kein Ziel ausgewählt\nBitte ein Laufwerk oder eine Netzwerkfreigabe wählen.",
                "No target selected\nPlease choose a drive or a network share."
            ))?;
        let target =
            sysimage::normalize_target(&target).map_err(|e| tr!("Ziel ungültig\n{e}", "Invalid target\n{e}"))?;
        if self.drives.iter().any(|d| d.system && d.letter().eq_ignore_ascii_case(&target)) {
            return Err(t!(
                "Ziel ungültig\nDas Abbild kann nicht auf dem Windows-Laufwerk selbst liegen.",
                "Invalid target\nThe image cannot be stored on the Windows drive itself."
            )
            .to_string());
        }
        Ok((target, login))
    }

    /// "Weiter" bzw. Start im Sichern-Ablauf.
    fn backup_next(&mut self) {
        let check = match (self.backup_step, self.backup_mode) {
            (0, BackupMode::Profile) => self.check_backup_profile().map(|_| ()),
            (0, BackupMode::Drive) => {
                self.source_drive().map(|_| ()).ok_or(
                    t!("Kein Laufwerk ausgewählt\nBitte ein Laufwerk wählen.", "No drive selected\nPlease choose a drive.").to_string(),
                )
            }
            (0 | 1, BackupMode::Image) => Ok(()),
            (1, _) => self.check_backup_sources().map(|_| ()),
            (2, BackupMode::Image) => self.check_image_target().map(|_| ()),
            (2, _) => self.check_backup_target().map(|_| ()),
            (_, BackupMode::Image) => return self.start_image(),
            _ => return self.start_backup(),
        };
        match check {
            Ok(()) => self.go_to_step(self.backup_step + 1),
            Err(message) => self.fail(message),
        }
    }

    /// Startet das Systemabbild über die Windows-Abbildsicherung.
    fn start_image(&mut self) {
        if !self.elevated {
            return self.fail(t!(
                "Administratorrechte nötig\nEin Systemabbild kann nur mit Administratorrechten erstellt werden. Bitte „Als Administrator neu starten“ wählen.",
                "Administrator rights required\nA system image can only be created with administrator rights. Please choose “Restart as administrator”."
            ));
        }
        let (target, login) = match self.check_image_target() {
            Ok(target) => target,
            Err(message) => return self.fail(message),
        };
        let include: Vec<String> = self.image_include.iter().filter(|l| !l.eq_ignore_ascii_case(&target)).cloned().collect();
        let job = ImageJob { target, include, login, expected_bytes: self.image_size() };
        self.persist();
        self.start(t!("Systemabbild", "System image"), move |rep| sysimage::run(&job, rep).map(Outcome::Image));
        self.progress_percent = true;
    }

    /// Startet ZEBack mit Administratorrechten neu und schließt dieses Fenster.
    fn restart_as_admin(&mut self) {
        let mode = match self.backup_mode {
            BackupMode::Profile => "profile",
            BackupMode::Drive => "drive",
            BackupMode::Image => "image",
        };
        self.persist();
        match sysimage::relaunch_elevated(&format!("--mode={mode}")) {
            Ok(()) => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(e) => self.fail(tr!(
                "Neustart als Administrator nicht möglich\n{e:#}",
                "Could not restart as administrator\n{e:#}"
            )),
        }
    }

    fn start_backup(&mut self) {
        let prepared = (|| {
            let (profile_name, profile_dir) = self.backup_identity()?;
            let sources = self.check_backup_sources()?;
            let destinations = self.check_backup_target()?;
            let password = self.check_backup_password()?;
            Ok::<_, String>(BackupJob {
                password,
                profile_name,
                profile_dir,
                sources,
                destinations,
                compression: self.cfg.compression,
                skip_online_only: self.cfg.skip_online_only,
                verify: self.cfg.verify_after_backup,
                incremental: None,
            })
        })();
        let job = match prepared {
            Ok(job) => job,
            Err(message) => return self.fail(message),
        };
        self.persist();
        self.persist_target_passwords();
        self.start(t!("Backup", "Backup"), move |rep| backup::run(&job, rep).map(Outcome::Backup));
    }

    // ------------------------------------------------------------------ Wiederherstellen: Prüfen und Starten

    fn root_target(&self, manifest: &Manifest, root: &ManifestRoot) -> Option<PathBuf> {
        match self.restore_mode {
            RestoreMode::Profile => {
                self.profiles.get(self.restore_profile).map(|p| restore::target_for(manifest, root, p))
            }
            RestoreMode::Folder => {
                let dir = self.restore_dir.trim();
                (!dir.is_empty()).then(|| PathBuf::from(dir).join(&root.name))
            }
        }
    }

    fn check_restore_archive(&self) -> Result<&ArchiveInfo, String> {
        self.archive
            .as_ref()
            .ok_or_else(|| {
                t!(
                    "Noch keine Sicherung geöffnet\nBitte eine ZIP-Datei öffnen oder aus Nextcloud laden.",
                    "No backup opened yet\nPlease open a ZIP file or load one from Nextcloud."
                )
                .to_string()
            })
    }

    /// Bei verschlüsselten Sicherungen: Passwort vorhanden und richtig?
    fn check_restore_password(&mut self) -> Result<(), String> {
        let Some(info) = &self.archive else { return Ok(()) };
        if !info.encrypted {
            return Ok(());
        }
        if self.restore_password.is_empty() {
            return Err(t!(
                "Passwort fehlt\nDiese Sicherung ist verschlüsselt. Bitte das Passwort eingeben.",
                "Password missing\nThis backup is encrypted. Please enter the password."
            )
            .to_string());
        }
        if self.restore_password_ok.as_deref() == Some(self.restore_password.as_str()) {
            return Ok(());
        }
        match restore::check_password(&info.path, &self.restore_password) {
            Ok(true) => {
                self.restore_password_ok = Some(self.restore_password.clone());
                Ok(())
            }
            Ok(false) => Err(t!(
                "Falsches Passwort\nMit diesem Passwort lässt sich die Sicherung nicht entschlüsseln.",
                "Wrong password\nThe backup cannot be decrypted with this password."
            )
            .to_string()),
            Err(e) => Err(tr!("Sicherung nicht lesbar\n{e:#}", "Backup not readable\n{e:#}")),
        }
    }

    /// Passwort für Prüfen und Wiederherstellen; `None` bei unverschlüsselten Sicherungen.
    fn restore_password_option(&self) -> Option<String> {
        self.archive.as_ref().filter(|a| a.encrypted).map(|_| self.restore_password.clone())
    }

    fn check_restore_selection(&self) -> Result<(), String> {
        let info = self.check_restore_archive()?;
        if self.picked.is_empty() && info.manifest.is_some() && !self.root_checked.iter().any(|c| *c) {
            return Err(t!(
                "Nichts ausgewählt\nBitte mindestens einen Ordner aus der Sicherung anhaken.",
                "Nothing selected\nPlease tick at least one folder from the backup."
            )
            .to_string());
        }
        Ok(())
    }

    fn check_restore_target(&self) -> Result<(), String> {
        let info = self.check_restore_archive()?;
        if self.restore_mode == RestoreMode::Folder || !info.fits_profile() {
            if self.restore_dir.trim().is_empty() {
                return Err(t!(
                    "Zielordner fehlt\nBitte einen Ordner für die Wiederherstellung auswählen.",
                    "Target folder missing\nPlease choose a folder for the restore."
                )
                .to_string());
            }
        } else {
            match self.profiles.get(self.restore_profile) {
                Some(profile) if profile.accessible => {}
                Some(profile) => {
                    return Err(tr!(
                        "Kein Zugriff auf das Profil „{}“\nStarte ZEBack als Administrator, um in fremde Profile zu schreiben.",
                        "No access to the profile “{}”\nStart ZEBack as administrator to write to other users' profiles.",
                        profile.name
                    ));
                }
                None => return Err(t!("Kein Profil ausgewählt", "No profile selected").to_string()),
            }
        }
        Ok(())
    }

    /// "Weiter" bzw. "Wiederherstellen" im Wiederherstellen-Ablauf.
    fn restore_next(&mut self) {
        let check = match self.restore_step {
            0 => self.check_restore_archive().map(|_| ()).and_then(|_| self.check_restore_password()),
            1 => self.check_restore_selection(),
            2 => self.check_restore_target(),
            _ => return self.start_restore(),
        };
        match check {
            Ok(()) => self.go_to_step(self.restore_step + 1),
            Err(message) => self.fail(message),
        }
    }

    fn start_restore(&mut self) {
        if let Err(message) = self
            .check_restore_selection()
            .and_then(|_| self.check_restore_target())
            .and_then(|_| self.check_restore_password())
        {
            return self.fail(message);
        }
        let password = self.restore_password_option();
        let Some(info) = &self.archive else { return };
        // Mit einer Einzelauswahl zählen die Häkchen an den Ordnern nicht: Jeder Ordner bekommt
        // sein Ziel, wiederhergestellt wird aber nur die Auswahl.
        let only = (!self.picked.is_empty()).then(|| self.picked.clone());
        let (mapping, mut total_bytes) = match &info.manifest {
            Some(manifest) => {
                let mut map = HashMap::new();
                let mut total = 0;
                for (root, checked) in manifest.roots.iter().zip(&self.root_checked) {
                    if let (true, Some(target)) = (*checked || only.is_some(), self.root_target(manifest, root)) {
                        map.insert(root.name.clone(), target);
                        total += root.bytes;
                    }
                }
                (Mapping::Roots(map), total)
            }
            None => (Mapping::Everything(PathBuf::from(self.restore_dir.trim())), info.total_bytes),
        };
        if let (Some(only), Some(tree)) = (&only, &self.tree) {
            total_bytes = tree.total(only).1;
        }
        let job = RestoreJob { archive: info.path.clone(), mapping, conflict: self.conflict, total_bytes, password, only };
        // Teilsicherung mit vollständiger Kette: Vollsicherung und Teilsicherungen zusammenführen.
        match self.chain.clone() {
            Some(chain) => self.start(t!("Wiederherstellung", "Restore"), move |rep| {
                restore::run_chain(&chain, &job, rep).map(Outcome::Restore)
            }),
            None => self.start(t!("Wiederherstellung", "Restore"), move |rep| restore::run(&job, rep).map(Outcome::Restore)),
        }
    }

    /// Prüft die geöffnete Sicherung, ohne etwas wiederherzustellen.
    fn start_verify(&mut self) {
        if let Err(message) = self.check_restore_password() {
            return self.fail(message);
        }
        let Some(info) = &self.archive else { return };
        let path = info.path.clone();
        let password = self.restore_password_option();
        self.start(t!("Sicherung wird geprüft", "Verifying backup"), move |rep| {
            verify::verify_archive(&path, None, password.as_deref(), rep).map(Outcome::Verified)
        });
    }

    fn open_archive(&mut self, path: PathBuf) {
        self.start(t!("Sicherung wird geöffnet", "Opening backup"), move |_| restore::inspect(&path).map(Outcome::Inspected));
    }

    /// Sucht auf allen Sicherungszielen (Ordner, Freigaben, Nextcloud) nach Sicherungen.
    fn start_backup_search(&mut self) {
        if self.cfg.targets.is_empty() {
            return self.fail(t!(
                "Kein Sicherungsziel eingerichtet\nLege unter „Sicherungsziele“ ein Ziel an – oder öffne die ZIP-Datei direkt.",
                "No backup target set up\nCreate a target under “Backup targets” – or open the ZIP file directly."
            ));
        }
        let targets: Vec<(Target, String)> =
            self.cfg.targets.iter().map(|t| (t.clone(), self.target_password(t))).collect();
        self.start(t!("Sicherungen werden gesucht", "Searching for backups"), move |rep| {
            Ok(Outcome::Found(jobs::find_backups(&targets, rep)))
        });
    }

    /// Vergisst Baumansicht und Einzelauswahl (beim Wechsel der Sicherung).
    fn forget_file_selection(&mut self) {
        self.tree = None;
        self.picked.clear();
        self.browse_dir = None;
    }

    /// Öffnet den Dialog zur Auswahl einzelner Dateien; beim ersten Mal wird der Inhalt gelesen.
    fn start_browse(&mut self) {
        if self.tree.is_some() {
            self.browse_dir = Some(String::new());
            return;
        }
        let Some(info) = &self.archive else { return };
        let path = info.path.clone();
        self.start(t!("Inhalt der Sicherung wird gelesen", "Reading backup contents"), move |_| {
            restore::list_entries(&path).map(|entries| Outcome::Listed(Tree::build(&entries)))
        });
    }

    fn start_download(&mut self, target_id: u64, file: RemoteFile) {
        let Some(target) = self.cfg.targets.iter().find(|t| t.id == target_id) else { return };
        let settings = match jobs::remote_settings(target, &self.target_password(target)) {
            Ok(Some(settings)) => settings,
            Ok(None) => return,
            Err(e) => return self.fail(e),
        };
        let target_name = target.name.clone();
        let mut dialog = rfd::FileDialog::new()
            .set_title(t!("Sicherung speichern unter", "Save backup as"))
            .set_file_name(&file.name)
            .add_filter(t!("ZIP-Archiv", "ZIP archive"), &["zip"]);
        if let Some(dir) = dirs::download_dir() {
            dialog = dialog.set_directory(dir);
        }
        let Some(dest) = dialog.save_file() else { return };
        self.found_backups = None;
        self.start(&tr!("Download von „{target_name}“", "Download from “{target_name}”"), move |rep| {
            let client = settings.connect()?;
            rep.status(tr!("Lade {} herunter …", "Downloading {} …", file.name));
            client.download(&file, &dest, rep)?;
            rep.log(tr!("Heruntergeladen nach {}", "Downloaded to {}", dest.display()));
            restore::inspect(&dest).map(Outcome::Inspected)
        });
    }

    // ------------------------------------------------------------------ Kopf- und Seitenleiste

    fn ui_topbar(&mut self, ui: &mut Ui) {
        let rect = ui.available_rect_before_wrap();
        ui.allocate_rect(rect, Sense::hover());
        let painter = ui.painter();
        let time = self.now() as f32;
        let busy = self.running.is_some();

        // Logo und Name
        let logo_rect = Rect::from_center_size(pos2(rect.left() + 28.0, rect.center().y), Vec2::splat(24.0));
        painter.image(
            self.logo.id(),
            logo_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.text(
            pos2(logo_rect.right() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            "ZEBack",
            theme::bold(15.0),
            TEXT_BRIGHT,
        );

        // Aktuelle Seite
        let (icon, title, subtitle) = match self.tab {
            Tab::Backup => (
                Icon::Backup,
                t!("Sichern", "Back up"),
                t!(
                    "Profil, Laufwerk oder ganzes System Schritt für Schritt sichern",
                    "Back up a profile, a drive or the whole system step by step"
                ),
            ),
            Tab::Restore => (
                Icon::Restore,
                t!("Wiederherstellen", "Restore"),
                t!("Dateien aus einer Sicherung zurückholen", "Get files back from a backup"),
            ),
            Tab::Jobs => (
                Icon::Sync,
                t!("Aufträge", "Jobs"),
                t!("Gespeicherte Sicherungen: mit einem Klick oder nach Zeitplan", "Saved backups: with one click or on a schedule"),
            ),
            Tab::Reports => (
                Icon::Mail,
                t!("Berichte", "Reports"),
                t!("Bescheid bekommen, wenn ein Auftrag gelaufen ist", "Get notified when a job has run"),
            ),
            Tab::Targets => (
                Icon::Drive,
                t!("Sicherungsziele", "Backup targets"),
                t!(
                    "Wohin gesichert werden kann: Ordner, Netzwerkfreigaben, Nextcloud",
                    "Where backups can go: folders, network shares, Nextcloud"
                ),
            ),
            Tab::Log => (Icon::Log, t!("Protokoll", "Log"), t!("Was zuletzt passiert ist", "What happened recently")),
        };
        let x = rect.left() + SIDEBAR_WIDTH + 24.0;
        widgets::paint_icon(painter, pos2(x + 8.0, rect.center().y), icon, 15.0, TEXT_MUTED);
        let title = painter.layout_no_wrap(title.to_owned(), theme::regular(14.0), TEXT_BRIGHT);
        let title_width = title.size().x;
        painter.galley(pos2(x + 26.0, rect.center().y - title.size().y / 2.0), title, TEXT_BRIGHT);

        // Angemeldeter Benutzer rechts
        let label = if self.computer_name.is_empty() {
            self.user_name.clone()
        } else {
            format!("{} @ {}", self.user_name, self.computer_name)
        };
        let label = if self.elevated { format!("{label} · Administrator") } else { label };
        let label = painter.layout_no_wrap(label, theme::regular(13.0), TEXT);
        let pill = Rect::from_min_max(
            pos2(rect.right() - 20.0 - label.size().x - 38.0, rect.center().y - 15.0),
            pos2(rect.right() - 20.0, rect.center().y + 15.0),
        );
        painter.rect(pill, CornerRadius::ZERO, SURFACE, Stroke::new(1.0, BORDER), StrokeKind::Inside);
        let dot_color = if busy { YELLOW.gamma_multiply(0.5 + 0.5 * (time * 4.0).sin().abs()) } else { GREEN };
        painter.rect_filled(
            Rect::from_center_size(pos2(pill.left() + 15.0, pill.center().y), Vec2::splat(7.0)),
            CornerRadius::ZERO,
            dot_color,
        );
        painter.galley(pos2(pill.left() + 26.0, pill.center().y - label.size().y / 2.0), label, TEXT);

        // Erklärung zur Seite, soweit Platz ist
        let subtitle_left = x + 26.0 + title_width + 14.0;
        let subtitle_width = pill.left() - subtitle_left - 16.0;
        if subtitle_width > 120.0 {
            let subtitle = widgets::elided(ui, subtitle, theme::regular(13.0), TEXT_FAINT, subtitle_width);
            painter.galley(pos2(subtitle_left, rect.center().y - subtitle.size().y / 2.0), subtitle, TEXT_FAINT);
        }

        painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, BORDER));
        if busy {
            ui.ctx().request_repaint();
        }
    }

    fn ui_sidebar(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        let painter = ui.painter().clone();
        painter.vline(rect.right() - 0.5, rect.y_range(), Stroke::new(1.0, BORDER));
        // Fußzeile: Version im Kästchen, darunter der Hersteller als Link zu seiner Website.
        // Gibt es eine neuere Version, steht darüber eine Zeile mit dem Hinweis.
        let update_row = if self.update.is_some() { 26.0 } else { 0.0 };
        let footer_top = rect.bottom() - 70.0 - update_row;
        painter.hline(rect.left()..=rect.right() - 1.0, footer_top, Stroke::new(1.0, BORDER));
        if let Some(release) = self.update.clone() {
            let text = tr!("Update {} verfügbar", "Update {} available", release.version);
            let hint = painter.layout_no_wrap(text, theme::bold(12.0), ACCENT);
            let hint_rect = Rect::from_min_size(pos2(rect.left() + 16.0, footer_top + 11.0), hint.size());
            let link = ui
                .interact(hint_rect.expand(4.0), egui::Id::unique("update_link"), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(t!("Download-Seite öffnen", "Open the download page"));
            let hover = ui.ctx().animate_bool_with_time(link.id.with("hover"), link.hovered(), 0.1);
            painter.galley(hint_rect.min, hint, ACCENT);
            painter.hline(hint_rect.x_range(), hint_rect.bottom() + 1.0, Stroke::new(1.0, ACCENT.gamma_multiply(hover)));
            if link.clicked() {
                self.open_download_page(&release);
            }
        }
        let version = painter.layout_no_wrap(concat!("v", env!("CARGO_PKG_VERSION")).to_owned(), theme::bold(11.0), TEXT_MUTED);
        let tag = Rect::from_min_size(pos2(rect.left() + 16.0, footer_top + 13.0 + update_row), version.size() + vec2(12.0, 6.0));
        painter.rect(tag, CornerRadius::ZERO, SURFACE, Stroke::new(1.0, BORDER), StrokeKind::Inside);
        painter.galley(tag.center() - version.size() / 2.0, version, TEXT_MUTED);

        let vendor = painter.layout_no_wrap("ZEBA Software UG".to_owned(), theme::regular(12.0), TEXT_MUTED);
        let vendor_rect = Rect::from_min_size(pos2(rect.left() + 16.0, tag.bottom() + 9.0), vendor.size());
        let link = ui
            .interact(vendor_rect.expand(4.0), egui::Id::unique("vendor_link"), Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .on_hover_text(t!("Website des Herstellers öffnen", "Open the vendor's website"));
        let hover = ui.ctx().animate_bool_with_time(link.id.with("hover"), link.hovered(), 0.1);
        let link_color = mix(TEXT_MUTED, ACCENT, hover);
        painter.galley_with_override_text_color(vendor_rect.min, vendor, link_color);
        painter.hline(vendor_rect.x_range(), vendor_rect.bottom() + 1.0, Stroke::new(1.0, link_color.gamma_multiply(hover)));
        if link.clicked()
            && let Err(e) = sysimage::open_in_browser(VENDOR_URL)
        {
            self.fail(tr!("Die Website konnte nicht geöffnet werden\n{e:#}", "The website could not be opened\n{e:#}"));
        }

        // Sprachumschalter rechts neben der Version
        let mut right = rect.right() - 17.0;
        for (code, language, active) in [
            ("EN", Language::English, crate::i18n::english()),
            ("DE", Language::German, !crate::i18n::english()),
        ] {
            let color = if active { ACCENT } else { TEXT_MUTED };
            let label = painter.layout_no_wrap(code.to_owned(), theme::bold(11.0), color);
            let size = label.size() + vec2(12.0, 6.0);
            let button = Rect::from_min_size(pos2(right - size.x, tag.top()), size);
            right = button.left() - 4.0;
            let response = ui
                .interact(button, egui::Id::unique(("language", code)), Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            let (fill, border) = if active { (ACCENT_BG, ACCENT) } else { (SURFACE, BORDER) };
            painter.rect(button, CornerRadius::ZERO, fill, Stroke::new(1.0, border), StrokeKind::Inside);
            painter.galley(button.center() - label.size() / 2.0, label, color);
            if response.clicked() && !active {
                self.cfg.language = language;
                crate::i18n::apply(language);
                self.persist();
            }
        }

        let nav = Rect::from_min_max(
            pos2(rect.left() + 8.0, rect.top() + 10.0),
            pos2(rect.right() - 9.0, footer_top - 8.0),
        );
        let entries = [
            (Tab::Backup, Icon::Backup, t!("Sichern", "Back up"), Some(t!("SICHERUNG", "BACKUP"))),
            (Tab::Restore, Icon::Restore, t!("Wiederherstellen", "Restore"), None),
            (Tab::Jobs, Icon::Sync, t!("Aufträge", "Jobs"), None),
            (Tab::Targets, Icon::Drive, t!("Sicherungsziele", "Backup targets"), Some(t!("VERWALTUNG", "MANAGEMENT"))),
            (Tab::Reports, Icon::Mail, t!("Berichte", "Reports"), None),
            (Tab::Log, Icon::Log, t!("Protokoll", "Log"), None),
        ];
        let mut clicked = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(nav), |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            let marker = ui.painter().add(Shape::Noop);
            let mut selected_rect = None;
            for (tab, icon, label, group) in entries {
                if let Some(group) = group {
                    if tab != Tab::Backup {
                        ui.add_space(10.0);
                    }
                    widgets::nav_group(ui, group);
                }
                let badge = (tab == Tab::Log).then_some(self.unseen_warnings);
                let response = widgets::nav_item(ui, self.tab == tab, icon, label, badge);
                if response.clicked() {
                    clicked = Some(tab);
                }
                if self.tab == tab {
                    selected_rect = Some(response.rect);
                }
            }
            // Die Markierung gleitet zum aktiven Eintrag.
            if let Some(target) = selected_rect {
                let top = ui.ctx().animate_value_with_time(egui::Id::unique("nav_marker"), target.top(), 0.18);
                let moved = Rect::from_min_size(pos2(target.left(), top), target.size());
                ui.painter().set(
                    marker,
                    egui::epaint::RectShape::new(
                        moved,
                        CornerRadius::ZERO,
                        ACCENT_BG,
                        Stroke::new(1.0, mix(BORDER, ACCENT, 0.75)),
                        StrokeKind::Inside,
                    ),
                );
                // Eckmarken wie bei den Karten, hier in der Akzentfarbe
                widgets::corner_brackets(&painter, moved, 6.0, ACCENT);
            }
        });
        if let Some(tab) = clicked {
            self.switch_tab(tab);
        }
    }

    // ------------------------------------------------------------------ Seitenrahmen

    /// Feines Raster als Hintergrund des Inhaltsbereichs.
    fn paint_grid(ui: &Ui, rect: Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        let stroke = Stroke::new(1.0, GRID);
        let mut x = rect.left() + GRID_STEP;
        while x < rect.right() {
            painter.vline(x.round() + 0.5, rect.y_range(), stroke);
            x += GRID_STEP;
        }
        let mut y = rect.top() + GRID_STEP;
        while y < rect.bottom() {
            painter.hline(rect.x_range(), y.round() + 0.5, stroke);
            y += GRID_STEP;
        }
    }

    fn ui_page(&mut self, ui: &mut Ui) {
        let elapsed = ((self.now() - self.page_changed) / 0.24).clamp(0.0, 1.0) as f32;
        let appear = theme::ease_out(elapsed);
        if elapsed < 1.0 {
            ui.ctx().request_repaint();
        }
        egui::ScrollArea::vertical()
            .id_salt((self.tab, self.backup_step, self.restore_step))
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin { left: 26, right: 26, top: 22, bottom: 26 })
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width().min(CONTENT_WIDTH));
                        let enabled = self.running.is_none() || self.tab == Tab::Log;
                        ui.add_enabled_ui(enabled, |ui| {
                            // Die Schrittanzeige bleibt stehen; nur der Inhalt darunter bewegt sich.
                            match self.tab {
                                Tab::Backup => {
                                    let labels =
                                        if self.backup_mode == BackupMode::Profile { backup_steps() } else { scope_steps() };
                                    if let Some(step) = widgets::stepper(ui, "backup_steps", &labels, self.backup_step) {
                                        self.go_to_step(step);
                                    }
                                }
                                Tab::Restore => {
                                    if let Some(step) = widgets::stepper(ui, "restore_steps", &restore_steps(), self.restore_step) {
                                        self.go_to_step(step);
                                    }
                                }
                                _ => {}
                            }
                            // Neuer Inhalt blendet ein: Schritte gleiten seitlich, Seiten von unten herein.
                            let offset = (1.0 - appear) * 26.0;
                            let shift = if self.slide == 0.0 { vec2(0.0, offset * 0.6) } else { vec2(offset * self.slide, 0.0) };
                            let area = ui.available_rect_before_wrap().translate(shift);
                            ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
                                ui.multiply_opacity(appear);
                                match self.tab {
                                    Tab::Backup => self.ui_backup(ui),
                                    Tab::Restore => self.ui_restore(ui),
                                    Tab::Jobs => self.ui_jobs(ui),
                                    Tab::Reports => self.ui_reports(ui),
                                    Tab::Targets => self.ui_targets(ui),
                                    Tab::Log => self.ui_log(ui),
                                }
                            });
                        });
                    });
            });
    }

    /// Zwei Karten nebeneinander (bei schmalem Fenster untereinander).
    fn two_columns(
        &mut self,
        ui: &mut Ui,
        left: impl FnOnce(&mut Self, &mut Ui),
        right: impl FnOnce(&mut Self, &mut Ui),
    ) {
        if ui.available_width() >= TWO_COLUMNS_FROM {
            let width = (ui.available_width() - GAP) / 2.0;
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                ui.vertical(|ui| {
                    ui.set_width(width);
                    left(self, ui);
                });
                ui.vertical(|ui| {
                    ui.set_width(width);
                    right(self, ui);
                });
            });
        } else {
            left(self, ui);
            ui.add_space(GAP - 8.0);
            right(self, ui);
        }
    }

    // ------------------------------------------------------------------ Sichern

    fn ui_backup(&mut self, ui: &mut Ui) {
        match (self.backup_step, self.backup_mode) {
            (0, _) => {
                step_heading(
                    ui,
                    0,
                    t!("Was soll gesichert werden?", "What do you want to back up?"),
                    t!(
                        "Ein Benutzerprofil, ein ganzes Laufwerk oder das komplette System.",
                        "A user profile, a whole drive or the complete system."
                    ),
                );
                self.two_columns(ui, Self::card_backup_mode, Self::card_backup_subject);
            }
            (1, BackupMode::Profile) => {
                let name = self.profiles.get(self.backup_profile).map(|p| p.name.clone()).unwrap_or_default();
                step_heading(
                    ui,
                    1,
                    t!("Welche Ordner sollen gesichert werden?", "Which folders do you want to back up?"),
                    &tr!(
                        "Ordner von „{name}“. Rechts stehen Zusatzordner wie Browser und Mail, soweit vorhanden.",
                        "Folders of “{name}”. On the right are additional folders such as browser and mail, where present."
                    ),
                );
                if self.opt_rows.is_empty() {
                    self.card_sources(ui);
                } else {
                    self.two_columns(ui, Self::card_sources, Self::card_optional);
                }
            }
            (1, BackupMode::Drive) => {
                step_heading(
                    ui,
                    1,
                    t!("Was gehört in die Laufwerkssicherung?", "What goes into the drive backup?"),
                    t!(
                        "Alle Dateien des Laufwerks als ZIP. Ein startfähiges Windows entsteht daraus nicht.",
                        "All files on the drive as a ZIP. This does not produce a bootable Windows."
                    ),
                );
                self.card_drive_scope(ui);
            }
            (1, BackupMode::Image) => {
                step_heading(
                    ui,
                    1,
                    t!("Was gehört ins Systemabbild?", "What goes into the system image?"),
                    t!(
                        "Windows samt Programmen und Einstellungen – so, dass der PC daraus wieder startet.",
                        "Windows including programs and settings – so that the PC can boot from it again."
                    ),
                );
                self.card_image_scope(ui);
            }
            (2, BackupMode::Image) => {
                step_heading(
                    ui,
                    2,
                    t!("Wohin soll das Systemabbild?", "Where should the system image go?"),
                    t!(
                        "Auf eine externe Festplatte oder eine Netzwerkfreigabe. Nextcloud ist dafür nicht geeignet.",
                        "To an external hard drive or a network share. Nextcloud is not suitable for this."
                    ),
                );
                self.card_image_target(ui);
            }
            (2, _) => {
                step_heading(
                    ui,
                    2,
                    t!("Wohin soll die Sicherung?", "Where should the backup go?"),
                    t!(
                        "Hake eines oder mehrere deiner Sicherungsziele an: Ordner, Netzwerkfreigabe, Nextcloud.",
                        "Tick one or more of your backup targets: folder, network share, Nextcloud."
                    ),
                );
                self.card_target(ui);
            }
            (_, BackupMode::Image) => {
                step_heading(
                    ui,
                    3,
                    t!("Alles bereit?", "All set?"),
                    t!("Prüfe die Angaben und erstelle das Abbild.", "Check the details and create the image."),
                );
                self.two_columns(ui, Self::card_image_summary, |app, ui| {
                    if Self::card_image_restore_help(ui) {
                        app.open_recovery_wizard();
                    }
                });
            }
            _ => {
                step_heading(
                    ui,
                    3,
                    t!("Alles bereit?", "All set?"),
                    t!("Prüfe die Angaben und starte die Sicherung.", "Check the details and start the backup."),
                );
                self.two_columns(ui, Self::card_backup_summary, Self::card_options);
            }
        }
    }

    /// Schritt 1 links: Art der Sicherung.
    fn card_backup_mode(&mut self, ui: &mut Ui) {
        let mut chosen = None;
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Backup, t!("Art der Sicherung", "Type of backup"), "", None);
            for (mode, icon, title, description) in [
                (
                    BackupMode::Profile,
                    Icon::User,
                    t!("Benutzerprofil", "User profile"),
                    t!("Ordner eines Benutzers als ZIP", "A user's folders as a ZIP"),
                ),
                (BackupMode::Drive, Icon::Drive, t!("Ganzes Laufwerk", "Whole drive"), t!("Alle Dateien als ZIP", "All files as a ZIP")),
                (
                    BackupMode::Image,
                    Icon::Desktop,
                    t!("Systemabbild", "System image"),
                    t!("Startfähiges Abbild von Windows", "Bootable image of Windows"),
                ),
            ] {
                if widgets::option_row(ui, self.backup_mode == mode, Mark::Radio, icon, title, description).clicked() {
                    chosen = Some(mode);
                }
            }
        });
        if let Some(mode) = chosen.filter(|m| *m != self.backup_mode) {
            self.backup_mode = mode;
            self.result = None;
            self.refresh_drive_scope();
        }
    }

    /// Schritt 1 rechts: Profil, Laufwerk oder Hinweise zum Systemabbild – je nach Art.
    fn card_backup_subject(&mut self, ui: &mut Ui) {
        match self.backup_mode {
            BackupMode::Profile => {
                let mut changed = false;
                let mut admin = false;
                let locked = !self.elevated && self.profiles.iter().any(|p| !p.accessible);
                widgets::card(ui, |ui| {
                    widgets::card_header(ui, Icon::User, t!("Welches Profil?", "Which profile?"), "", Some(&self.profiles.len().to_string()));
                    changed = profile_rows(ui, &self.profiles, &mut self.backup_profile);
                    if locked {
                        admin = widgets::button(
                            ui,
                            t!("Als Administrator neu starten", "Restart as administrator"),
                            Kind::Secondary,
                            Some(Icon::Key),
                        )
                        .clicked();
                    }
                });
                if changed {
                    self.reload_std_rows();
                }
                if admin {
                    self.restart_as_admin();
                }
            }
            BackupMode::Drive => {
                let mut chosen = None;
                let mut refresh = false;
                widgets::card(ui, |ui| {
                    widgets::card_header(ui, Icon::Drive, t!("Welches Laufwerk?", "Which drive?"), "", Some(&self.drives.len().to_string()));
                    for (index, drive) in self.drives.iter().enumerate() {
                        let description = tr!(
                            "{} belegt von {} · {}",
                            "{} used of {} · {}",
                            fmt_bytes(drive.used()),
                            fmt_bytes(drive.total),
                            drive.file_system
                        );
                        let clicked = ui
                            .push_id(("source_drive", index), |ui| {
                                widgets::option_row(ui, self.drive_source == index, Mark::Radio, Icon::Drive, &drive.title(), &description)
                                    .clicked()
                            })
                            .inner;
                        if clicked {
                            chosen = Some(index);
                        }
                    }
                    refresh = widgets::button(ui, t!("Laufwerke neu einlesen", "Rescan drives"), Kind::Ghost, Some(Icon::Sync)).clicked();
                });
                if let Some(index) = chosen {
                    self.drive_source = index;
                    self.refresh_drive_scope();
                }
                if refresh {
                    self.refresh_drives();
                }
            }
            BackupMode::Image => {
                let mut admin = false;
                self.ensure_system_facts();
                widgets::card(ui, |ui| {
                    widgets::card_header(ui, Icon::Desktop, t!("Systemabbild", "System image"), "", None);
                    for line in [
                        t!(
                            "Sichert Windows, Programme, Einstellungen und alle Benutzer in einem Abbild.",
                            "Backs up Windows, programs, settings and all users in one image."
                        ),
                        t!(
                            "Erstellt wird es mit der Abbildsicherung, die Windows selbst mitbringt.",
                            "It is created with the image backup that is built into Windows."
                        ),
                        t!(
                            "Ziel ist eine externe Festplatte (NTFS) oder eine Netzwerkfreigabe.",
                            "The target is an external hard drive (NTFS) or a network share."
                        ),
                        t!(
                            "Zurückgespielt wird es über die Windows-Wiederherstellungsumgebung.",
                            "It is restored through the Windows Recovery Environment."
                        ),
                    ] {
                        ui.horizontal_top(|ui| {
                            widgets::text(ui, ">", theme::bold(13.0), ACCENT);
                            ui.add(egui::Label::new(RichText::new(line).font(theme::regular(13.0)).color(TEXT)).wrap());
                        });
                    }
                    ui.add_space(6.0);
                    let (recovery, bitlocker) = self.facts_texts();
                    widgets::kv_row(ui, t!("Wiederherstellungsumgebung", "Recovery environment"), recovery);
                    widgets::kv_row(ui, t!("BitLocker auf dem Windows-Laufwerk", "BitLocker on the Windows drive"), bitlocker);
                    if self.elevated {
                        widgets::kv_row(ui, t!("Administratorrechte", "Administrator rights"), t!("vorhanden", "available"));
                    } else {
                        warning_line(ui, t!("Dafür braucht ZEBack Administratorrechte.", "ZEBack needs administrator rights for this."));
                        admin = widgets::button(
                            ui,
                            t!("Als Administrator neu starten", "Restart as administrator"),
                            Kind::Secondary,
                            Some(Icon::Key),
                        )
                        .clicked();
                    }
                });
                if admin {
                    self.restart_as_admin();
                }
            }
        }
    }

    /// Anzeigetexte zu Wiederherstellungsumgebung und BitLocker.
    fn facts_texts(&self) -> (&'static str, &'static str) {
        let Some(facts) = self.facts else {
            return (t!("wird ermittelt …", "checking …"), t!("wird ermittelt …", "checking …"));
        };
        let recovery = match facts.recovery_environment {
            Answer::Yes => t!("eingeschaltet", "enabled"),
            Answer::No => t!("abgeschaltet", "disabled"),
            Answer::Unknown if self.elevated => t!("unbekannt", "unknown"),
            Answer::Unknown => t!("nur als Administrator sichtbar", "visible only as administrator"),
        };
        let bitlocker = match facts.bitlocker {
            Answer::Yes => t!("eingeschaltet", "enabled"),
            Answer::No => t!("aus", "off"),
            Answer::Unknown => t!("unbekannt", "unknown"),
        };
        (recovery, bitlocker)
    }

    /// Laufwerkssicherung: was ausgelassen wird und wie groß sie wird.
    fn card_drive_scope(&mut self, ui: &mut Ui) {
        let Some(drive) = self.source_drive().cloned() else {
            widgets::card(ui, |ui| warning_line(ui, t!("Es wurde kein Laufwerk gefunden.", "No drive was found.")));
            return;
        };
        let before = self.cfg.drive_data_only;
        let mut admin = false;
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Drive, &drive.title(), "", Some(&self.selected_size_text()));
            ui.spacing_mut().item_spacing.y = 2.0;
            widgets::kv_row(
                ui,
                t!("Belegt", "Used"),
                &tr!("{} von {}", "{} of {}", fmt_bytes(drive.used()), fmt_bytes(drive.total)),
            );
            widgets::kv_row(ui, t!("In der Sicherung", "In the backup"), &self.selected_size_text());
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(8.0);
            widgets::toggle_row(
                ui,
                &mut self.cfg.drive_data_only,
                t!("Nur Daten sichern", "Back up data only"),
                t!(
                    "Windows und installierte Programme auslassen. Sie lassen sich aus einer ZIP-Datei nicht startfähig zurückspielen – dafür ist das Systemabbild da.",
                    "Skip Windows and installed programs. They cannot be restored from a ZIP file in a bootable state – that is what the system image is for."
                ),
            );
            ui.add_space(8.0);
            widgets::text(
                ui,
                t!(
                    "Immer ausgelassen: Auslagerungs- und Ruhezustandsdatei, Papierkorb, Temp-Ordner, Windows-Update-Reste und vorhandene Systemabbilder.",
                    "Always skipped: page file and hibernation file, recycle bin, temp folders, Windows Update leftovers and existing system images."
                ),
                theme::regular(12.0),
                TEXT_MUTED,
            );
            if !self.elevated {
                ui.add_space(6.0);
                warning_line(
                    ui,
                    t!(
                        "Ohne Administratorrechte werden die Ordner anderer Benutzer und geschützte Systemdateien übersprungen.",
                        "Without administrator rights, other users' folders and protected system files are skipped."
                    ),
                );
                admin = widgets::button(
                    ui,
                    t!("Als Administrator neu starten", "Restart as administrator"),
                    Kind::Secondary,
                    Some(Icon::Key),
                )
                .clicked();
            }
        });
        if before != self.cfg.drive_data_only {
            self.refresh_drive_scope();
        }
        if admin {
            self.restart_as_admin();
        }
    }

    /// Systemabbild: Pflichtumfang und zusätzliche Laufwerke.
    fn card_image_scope(&mut self, ui: &mut Ui) {
        let extra: Vec<Drive> = self.drives.iter().filter(|d| !d.system && !d.removable).cloned().collect();
        let system = self.drives.iter().find(|d| d.system).cloned();
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Desktop, t!("Umfang", "Scope"), "", Some(&fmt_bytes(self.image_size())));
            ui.spacing_mut().item_spacing.y = 2.0;
            // Der Pflichtumfang ist immer dabei und lässt sich nicht abwählen.
            let mut always = true;
            ui.add_enabled_ui(false, |ui| {
                let subtitle = t!(
                    "Systemlaufwerk, Start- und Wiederherstellungspartition – immer enthalten",
                    "System drive, boot and recovery partitions – always included"
                );
                let title = system.as_ref().map(|d| format!("Windows {}", d.letter())).unwrap_or_else(|| "Windows".into());
                let size = system.as_ref().map(|d| fmt_bytes(d.used()));
                widgets::check_row(ui, &mut always, Icon::Desktop, &title, subtitle, size.as_deref(), false);
            });
            if extra.is_empty() {
                ui.add_space(6.0);
                widgets::text(
                    ui,
                    t!(
                        "Weitere fest eingebaute Laufwerke gibt es auf diesem PC nicht.",
                        "This PC has no other built-in drives."
                    ),
                    theme::regular(12.0),
                    TEXT_MUTED,
                );
            }
            for drive in &extra {
                let letter = drive.letter().to_string();
                let mut checked = self.image_include.contains(&letter);
                let subtitle = tr!("Zusätzliches Laufwerk · {}", "Additional drive · {}", drive.file_system);
                ui.push_id(("image_include", &letter), |ui| {
                    widgets::check_row(ui, &mut checked, Icon::Drive, &drive.title(), &subtitle, Some(&fmt_bytes(drive.used())), false);
                });
                if checked && !self.image_include.contains(&letter) {
                    self.image_include.push(letter);
                } else if !checked {
                    self.image_include.retain(|l| *l != letter);
                }
            }
        });
    }

    /// Systemabbild: Ziellaufwerk oder Netzwerkfreigabe.
    fn card_image_target(&mut self, ui: &mut Ui) {
        let needed = self.image_size();
        let mut refresh = false;
        let mut manage = false;
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Usb,
                t!("Ziel", "Target"),
                &tr!("Benötigt werden etwa {}", "About {} is needed", fmt_bytes(needed)),
                None,
            );
            let candidates: Vec<Drive> = self.drives.iter().filter(|d| !d.system).cloned().collect();
            if candidates.is_empty() && !self.cfg.targets.iter().any(|t| t.kind == TargetKind::Smb) {
                warning_line(
                    ui,
                    t!(
                        "Es ist kein weiteres Laufwerk angeschlossen. Bitte eine externe Festplatte anstecken oder eine Netzwerkfreigabe anlegen.",
                        "No other drive is connected. Please plug in an external hard drive or create a network share."
                    ),
                );
            }
            for drive in &candidates {
                let letter = drive.letter().to_string();
                let suitable = ["NTFS", "ReFS"].iter().any(|fs| drive.file_system.eq_ignore_ascii_case(fs));
                let included = self.image_include.contains(&letter);
                let description = if !suitable {
                    tr!(
                        "{} – nicht geeignet, das Ziel muss mit NTFS formatiert sein",
                        "{} – not suitable, the target must be formatted with NTFS",
                        drive.file_system
                    )
                } else if included {
                    t!(
                        "Gehört zum Umfang des Abbilds und kann nicht zugleich Ziel sein",
                        "Is part of the image scope and cannot be the target at the same time"
                    )
                    .to_string()
                } else if drive.free < needed {
                    tr!("{} frei – zu wenig Platz", "{} free – not enough space", fmt_bytes(drive.free))
                } else {
                    tr!(
                        "{} frei von {} · {}",
                        "{} free of {} · {}",
                        fmt_bytes(drive.free),
                        fmt_bytes(drive.total),
                        drive.file_system
                    )
                };
                let selected = self.image_smb.is_none() && self.image_drive.as_deref() == Some(letter.as_str());
                let clicked = ui
                    .add_enabled_ui(suitable && !included, |ui| {
                        ui.push_id(("image_target", &letter), |ui| {
                            widgets::option_row(ui, selected, Mark::Radio, Icon::Drive, &drive.title(), &description).clicked()
                        })
                        .inner
                    })
                    .inner;
                if clicked {
                    self.image_drive = Some(letter);
                    self.image_smb = None;
                }
            }
            // Netzwerkfreigaben aus den Sicherungszielen
            let shares: Vec<(u64, String, String)> = self
                .cfg
                .targets
                .iter()
                .filter(|t| t.kind == TargetKind::Smb)
                .map(|t| (t.id, t.name.clone(), t.path.trim().to_string()))
                .collect();
            for (id, name, path) in &shares {
                let description = if path.is_empty() {
                    t!("Noch kein Pfad eingetragen", "No path entered yet").to_string()
                } else {
                    tr!("Netzwerkfreigabe · {path}", "Network share · {path}")
                };
                let clicked = ui
                    .push_id(("image_share", id), |ui| {
                        widgets::option_row(ui, self.image_smb == Some(*id), Mark::Radio, Icon::Globe, name, &description).clicked()
                    })
                    .inner;
                if clicked {
                    self.image_smb = Some(*id);
                }
            }
            ui.horizontal(|ui| {
                refresh = widgets::button(ui, t!("Laufwerke neu einlesen", "Rescan drives"), Kind::Ghost, Some(Icon::Sync)).clicked();
                manage =
                    widgets::button(ui, t!("Netzwerkfreigabe anlegen", "Add network share"), Kind::Ghost, Some(Icon::Add)).clicked();
            });
            if !shares.is_empty() {
                widgets::text(
                    ui,
                    t!(
                        "Auf einer Netzwerkfreigabe bewahrt Windows nur das jeweils letzte Abbild auf.",
                        "On a network share, Windows only keeps the most recent image."
                    ),
                    theme::regular(12.0),
                    TEXT_MUTED,
                );
            }

            // Was auf dem gewählten Laufwerk schon liegt
            if let Some((target, images)) = &self.images
                && self.image_smb.is_none()
                && self.image_drive.as_deref() == Some(target.as_str())
            {
                ui.add_space(6.0);
                widgets::divider(ui);
                ui.add_space(6.0);
                widgets::field_label(ui, t!("Vorhandene Abbilder auf diesem Laufwerk", "Existing images on this drive"));
                if images.is_empty() {
                    let note = if self.elevated {
                        t!("Keine gefunden.", "None found.")
                    } else {
                        t!("Nur mit Administratorrechten sichtbar.", "Visible only with administrator rights.")
                    };
                    widgets::text(ui, note, theme::regular(12.0), TEXT_MUTED);
                }
                for image in images {
                    let date = image
                        .created
                        .map(|utc| utc.and_utc().with_timezone(&Local).format("%d.%m.%Y, %H:%M").to_string())
                        .unwrap_or_else(|| t!("Datum unbekannt", "Date unknown").to_string());
                    widgets::kv_row(ui, &format!("{} · {date}", image.computer), &fmt_bytes(image.size));
                }
            }
        });
        if let Some(drive) = self.image_drive.clone().filter(|_| self.image_smb.is_none()) {
            self.ensure_images(&drive);
        }
        if refresh {
            self.refresh_drives();
            self.images = None;
        }
        if manage {
            self.target_edit = self.add_target(TargetKind::Smb, "");
            self.switch_tab(Tab::Targets);
        }
    }

    fn card_image_summary(&mut self, ui: &mut Ui) {
        let mut admin = false;
        self.ensure_system_facts();
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Desktop, t!("Zusammenfassung", "Summary"), "", None);
            ui.spacing_mut().item_spacing.y = 2.0;
            widgets::kv_row(ui, t!("Art", "Type"), t!("Systemabbild", "System image"));
            let mut scope = vec![t!("Windows und Startpartitionen", "Windows and boot partitions").to_string()];
            scope.extend(self.image_include.iter().cloned());
            widgets::kv_row(ui, t!("Umfang", "Scope"), &scope.join(" + "));
            widgets::kv_row(ui, t!("Belegter Platz", "Used space"), &fmt_bytes(self.image_size()));
            widgets::kv_row(
                ui,
                t!("Ziel", "Target"),
                &self.image_target().unwrap_or_else(|| t!("noch nicht gewählt", "not chosen yet").into()),
            );
            widgets::kv_row(
                ui,
                t!("Administratorrechte", "Administrator rights"),
                if self.elevated { t!("vorhanden", "available") } else { t!("fehlen", "missing") },
            );
            ui.add_space(6.0);
            widgets::text(
                ui,
                t!(
                    "Das Abbild landet im Ordner „WindowsImageBackup“ auf dem Ziel. Die Sicherung läuft über einen Windows-Dienst und kann je nach Datenmenge lange dauern; der PC bleibt dabei benutzbar und geht nicht in den Ruhezustand.",
                    "The image ends up in the “WindowsImageBackup” folder on the target. The backup runs through a Windows service and can take a long time depending on the amount of data; the PC stays usable meanwhile and does not go to sleep."
                ),
                theme::regular(12.0),
                TEXT_MUTED,
            );
            if let Some(facts) = self.facts {
                if facts.recovery_environment == Answer::No {
                    ui.add_space(6.0);
                    warning_line(
                        ui,
                        t!(
                            "Die Wiederherstellungsumgebung ist auf diesem PC abgeschaltet. Zum Zurückspielen brauchst du dann einen Rettungs-USB-Stick oder einen Windows-Installations-Stick.",
                            "The recovery environment is disabled on this PC. To restore you will then need a recovery USB stick or a Windows installation USB stick."
                        ),
                    );
                }
                if facts.bitlocker == Answer::Yes {
                    ui.add_space(6.0);
                    warning_line(
                        ui,
                        t!(
                            "Das Windows-Laufwerk ist mit BitLocker verschlüsselt. Das Abbild liegt unverschlüsselt auf dem Ziel – schütze den Zieldatenträger selbst. Halte den BitLocker-Wiederherstellungsschlüssel bereit; nach dem Zurückspielen muss BitLocker neu eingeschaltet werden.",
                            "The Windows drive is encrypted with BitLocker. The image is stored unencrypted on the target – protect the target disk yourself. Keep the BitLocker recovery key at hand; after restoring, BitLocker has to be turned on again."
                        ),
                    );
                }
            }
            if !self.elevated {
                ui.add_space(6.0);
                warning_line(
                    ui,
                    t!(
                        "Ohne Administratorrechte lässt sich kein Systemabbild erstellen.",
                        "A system image cannot be created without administrator rights."
                    ),
                );
                admin = widgets::button(
                    ui,
                    t!("Als Administrator neu starten", "Restart as administrator"),
                    Kind::Primary,
                    Some(Icon::Key),
                )
                .clicked();
            }
        });
        if admin {
            self.restart_as_admin();
        }
    }

    /// Anleitung: So wird ein Systemabbild zurückgespielt.
    /// Rückgabe: `true`, wenn "Rettungs-USB-Stick erstellen" geklickt wurde.
    fn card_image_restore_help(ui: &mut Ui) -> bool {
        let mut create_stick = false;
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Restore,
                t!("Systemabbild zurückspielen", "Restoring a system image"),
                t!(
                    "Geht nur außerhalb des laufenden Windows, nicht aus ZEBack heraus",
                    "Only works outside the running Windows, not from within ZEBack"
                ),
                None,
            );
            for (number, step) in [
                t!("Datenträger mit dem Abbild anschließen.", "Connect the disk that holds the image."),
                t!(
                    "Einstellungen → System → Wiederherstellung → „Erweiterter Start“ → Jetzt neu starten. Startet Windows nicht mehr, vom Installations-Stick booten und „Computerreparaturoptionen“ wählen.",
                    "Settings → System → Recovery → “Advanced startup” → Restart now. If Windows no longer starts, boot from the installation USB stick and choose “Repair your computer”."
                ),
                t!(
                    "Problembehandlung → Erweiterte Optionen → Weitere Wiederherstellungsoptionen → Systemimage-Wiederherstellung.",
                    "Troubleshoot → Advanced options → See more recovery options → System Image Recovery."
                ),
                t!(
                    "Abbild auswählen und bestätigen. Das Ziellaufwerk wird dabei vollständig überschrieben.",
                    "Select the image and confirm. The target drive will be completely overwritten."
                ),
            ]
            .into_iter()
            .enumerate()
            {
                ui.horizontal_top(|ui| {
                    widgets::chip(ui, &format!("{:02}", number + 1), ACCENT);
                    ui.add(egui::Label::new(RichText::new(step).font(theme::regular(13.0)).color(TEXT)).wrap());
                });
            }
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(8.0);
            widgets::text(
                ui,
                t!(
                    "Startet der PC gar nicht mehr, hilft ein Rettungs-USB-Stick. Leg ihn am besten jetzt an – Windows bringt dafür einen Assistenten mit.",
                    "If the PC no longer starts at all, a recovery USB stick helps. It is best to create one now – Windows comes with a wizard for this."
                ),
                theme::regular(12.0),
                TEXT_MUTED,
            );
            create_stick = widgets::button(
                ui,
                t!("Rettungs-USB-Stick erstellen", "Create recovery USB stick"),
                Kind::Secondary,
                Some(Icon::Usb),
            )
            .clicked();
        });
        create_stick
    }

    /// Zusatzordner des Profils: Browser, Mail, Office und Ähnliches, nach Gruppen sortiert.
    fn card_optional(&mut self, ui: &mut Ui) {
        let badge = format!("{}/{}", self.opt_rows.iter().filter(|r| r.checked).count(), self.opt_rows.len());
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Add,
                t!("Zusätzlich", "Additional"),
                t!(
                    "Programme vorher schließen – geöffnete Dateien werden sonst übersprungen",
                    "Close the programs first – otherwise open files are skipped"
                ),
                Some(&badge),
            );
            ui.spacing_mut().item_spacing.y = 2.0;
            let mut group: Option<Group> = None;
            let sizes: Vec<Option<String>> = self.opt_rows.iter().map(|r| self.size_label(&r.folder.path)).collect();
            for (row, size) in self.opt_rows.iter_mut().zip(sizes) {
                if group != Some(row.folder.group) {
                    if group.is_some() {
                        ui.add_space(8.0);
                    }
                    group = Some(row.folder.group);
                    widgets::field_label(ui, row.folder.group.label());
                }
                let subtitle = format!("{} · {}", row.folder.hint_text(), row.folder.path.display());
                ui.push_id(row.folder.key, |ui| {
                    widgets::check_row(
                        ui,
                        &mut row.checked,
                        std_icon(Some(row.folder.key)),
                        row.folder.title(),
                        &subtitle,
                        size.as_deref(),
                        false,
                    );
                });
            }
            if self.opt_rows.iter().any(|r| r.checked && matches!(r.folder.key, "chrome" | "edge")) {
                ui.add_space(6.0);
                warning_line(
                    ui,
                    t!(
                        "Passwörter aus Chrome und Edge sind an dieses Windows-Konto gebunden und lassen sich nicht auf einen anderen PC übertragen.",
                        "Passwords from Chrome and Edge are tied to this Windows account and cannot be transferred to another PC."
                    ),
                );
            }
        });
    }

    fn card_sources(&mut self, ui: &mut Ui) {
        let total = self.std_rows.len() + self.cfg.extra_folders.len();
        let selected = self.std_rows.iter().filter(|r| r.checked).count()
            + self.cfg.extra_folders.iter().filter(|f| f.enabled).count();
        let badge = format!("{selected}/{total}");
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Folder,
                t!("Ordner", "Folders"),
                t!("Standardordner und eigene Ordner", "Standard folders and your own folders"),
                Some(&badge),
            );
            ui.spacing_mut().item_spacing.y = 2.0;
            if self.std_rows.is_empty() {
                warning_line(
                    ui,
                    t!("In diesem Profil wurden keine Standardordner gefunden.", "No standard folders were found in this profile."),
                );
            }
            let sizes: Vec<Option<String>> = self.std_rows.iter().map(|r| self.size_label(&r.folder.path)).collect();
            for (row, size) in self.std_rows.iter_mut().zip(sizes) {
                ui.push_id(row.folder.key, |ui| {
                    widgets::check_row(
                        ui,
                        &mut row.checked,
                        std_icon(Some(row.folder.key)),
                        row.folder.title(),
                        &row.folder.path.display().to_string(),
                        size.as_deref(),
                        false,
                    );
                });
            }
            let mut remove = None;
            let sizes: Vec<Option<String>> =
                self.cfg.extra_folders.iter().map(|f| self.size_label(&f.path)).collect();
            for ((index, folder), size) in self.cfg.extra_folders.iter_mut().enumerate().zip(sizes) {
                let name = folder
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| folder.path.display().to_string());
                let path = folder.path.display().to_string();
                let removed = ui
                    .push_id(("extra", &path), |ui| {
                        widgets::check_row(ui, &mut folder.enabled, Icon::Folder, &name, &path, size.as_deref(), true)
                    })
                    .inner;
                if removed {
                    remove = Some(index);
                }
            }
            if let Some(index) = remove {
                self.cfg.extra_folders.remove(index);
            }
            ui.add_space(8.0);
            if widgets::button(ui, t!("Ordner hinzufügen", "Add folder"), Kind::Secondary, Some(Icon::Add)).clicked()
                && let Some(path) = pick_folder(t!("Ordner zum Sichern auswählen", "Choose a folder to back up"))
                && !self.cfg.extra_folders.iter().any(|f| f.path == path)
                && !self.std_rows.iter().any(|r| r.folder.path == path)
            {
                self.sizes.request(&path, &[]);
                self.cfg.extra_folders.push(ExtraFolder { path, enabled: true });
            }
        });
    }

    /// Schritt "Ziel" für ZIP-Sicherungen: die eingerichteten Sicherungsziele zum Anhaken.
    fn card_target(&mut self, ui: &mut Ui) {
        let mut add_folder = false;
        let mut manage = false;
        widgets::card(ui, |ui| {
            let selected = self.cfg.targets.iter().filter(|t| t.selected).count();
            let badge = format!("{selected}/{}", self.cfg.targets.len());
            widgets::card_header(
                ui,
                Icon::Drive,
                t!("Sicherungsziele", "Backup targets"),
                t!("Ein Ziel oder mehrere gleichzeitig", "One target or several at once"),
                Some(&badge),
            );
            if self.cfg.targets.is_empty() {
                widgets::text(
                    ui,
                    t!(
                        "Noch kein Ziel eingerichtet. Wähle einen Ordner, z. B. auf einem USB-Stick – oder lege unter „Sicherungsziele“ eine Netzwerkfreigabe oder eine Nextcloud an.",
                        "No target set up yet. Choose a folder, e.g. on a USB stick – or add a network share or a Nextcloud under “Backup targets”."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
            }
            // Was einem Ziel noch fehlt (zweite Zeile der Meldung), damit es beim Anhaken auffällt.
            let problems: Vec<Option<String>> = self
                .cfg
                .targets
                .iter()
                .map(|t| self.check_target(t).err().map(|m| m.lines().nth(1).unwrap_or(m.as_str()).to_string()))
                .collect();
            for (target, problem) in self.cfg.targets.iter_mut().zip(problems) {
                let description = match problem {
                    Some(problem) if target.selected => tr!("Unvollständig – {problem}", "Incomplete – {problem}"),
                    _ => target.summary(),
                };
                let clicked = ui
                    .push_id(("target", target.id), |ui| {
                        widgets::option_row(ui, target.selected, Mark::Check, target_icon(target.kind), &target.name, &description)
                            .clicked()
                    })
                    .inner;
                if clicked {
                    target.selected = !target.selected;
                }
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                add_folder = widgets::button(ui, t!("Ordner hinzufügen", "Add folder"), Kind::Secondary, Some(Icon::Add)).clicked();
                manage = widgets::button(ui, t!("Ziele verwalten", "Manage targets"), Kind::Ghost, Some(Icon::Settings)).clicked();
            });
            self.ui_disk_info(ui);
        });
        if add_folder
            && let Some(path) = pick_folder(t!("Zielordner auswählen", "Choose a target folder"))
        {
            let path = path.display().to_string();
            let known = self
                .cfg
                .targets
                .iter_mut()
                .find(|t| t.kind == TargetKind::Local && t.path.trim().eq_ignore_ascii_case(&path));
            match known {
                Some(target) => target.selected = true,
                None => {
                    self.add_target(TargetKind::Local, &path);
                }
            }
        }
        if manage {
            self.switch_tab(Tab::Targets);
        }
    }

    /// Freier Platz am ersten angehakten Ordner-Ziel (oder im Temp-Ordner, wenn nur zu Nextcloud
    /// gesichert wird) im Vergleich zur ausgewählten Datenmenge.
    fn ui_disk_info(&mut self, ui: &mut Ui) {
        let selected: Vec<&Target> = self.cfg.targets.iter().filter(|t| t.selected).collect();
        let folder = selected.iter().find(|t| t.kind == TargetKind::Local && !t.path.trim().is_empty());
        let only_cloud = !selected.is_empty() && selected.iter().all(|t| t.kind.is_cloud());
        let (path, label, unreachable, temp) = match folder {
            Some(target) => (
                PathBuf::from(target.path.trim()),
                tr!("Frei auf „{}“", "Free on “{}”", target.name),
                tr!(
                    "„{}“ ist nicht erreichbar – ist der Datenträger angeschlossen?",
                    "“{}” is not reachable – is the disk connected?",
                    target.name
                ),
                false,
            ),
            None if only_cloud => (
                std::env::temp_dir(),
                t!("Frei im Temp-Ordner", "Free in the temp folder").to_string(),
                t!("Der Temp-Ordner ist nicht erreichbar.", "The temp folder is not reachable.").to_string(),
                true,
            ),
            None => return,
        };
        ui.add_space(6.0);
        widgets::divider(ui);
        ui.add_space(6.0);
        if temp {
            widgets::text(
                ui,
                t!(
                    "Die ZIP-Datei wird vor dem Upload vorübergehend im Temp-Ordner dieses PCs erstellt.",
                    "Before the upload, the ZIP file is created temporarily in this PC's temp folder."
                ),
                theme::regular(12.0),
                TEXT_MUTED,
            );
        }
        let label = label.as_str();
        let probe = if temp { self.disk_temp.lookup(&path) } else { self.disk_target.lookup(&path) };
        let (needed, pending) = self.selected_size();
        match probe {
            Probe::Pending => {
                widgets::text(ui, t!("Speicherplatz wird ermittelt …", "Checking free space …"), theme::regular(12.0), TEXT_MUTED);
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
            Probe::Unavailable => warning_line(ui, &unreachable),
            Probe::Ready(info) => {
                let file_system =
                    if info.file_system.is_empty() { String::new() } else { format!(" · {}", info.file_system) };
                widgets::kv_row(
                    ui,
                    label,
                    &tr!("{} von {}{file_system}", "{} of {}{file_system}", fmt_bytes(info.free), fmt_bytes(info.total)),
                );
                widgets::kv_row(ui, t!("Ausgewählte Daten", "Selected data"), &self.selected_size_text());
                if !pending && needed > info.free {
                    warning_line(
                        ui,
                        t!(
                            "Der freie Platz ist kleiner als die ausgewählten Daten. Komprimiert wird die Sicherung meist kleiner – es kann trotzdem knapp werden.",
                            "The free space is smaller than the selected data. Compressed, the backup usually gets smaller – it may still be tight."
                        ),
                    );
                }
                if !temp && info.is_fat32() && needed > disk::FAT32_MAX_FILE {
                    warning_line(
                        ui,
                        t!(
                            "Dieses Laufwerk ist mit FAT32 formatiert und speichert höchstens 4 GB pro Datei. Die Sicherung wird voraussichtlich größer – Laufwerk mit exFAT oder NTFS formatieren.",
                            "This drive is formatted with FAT32 and stores at most 4 GB per file. The backup will probably be larger – format the drive with exFAT or NTFS."
                        ),
                    );
                }
            }
        }
    }

    fn card_backup_summary(&mut self, ui: &mut Ui) {
        let mut save_job = false;
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Backup, t!("Zusammenfassung", "Summary"), "", None);
            ui.spacing_mut().item_spacing.y = 2.0;
            let identity = self.backup_identity().map(|(name, _)| name).unwrap_or_else(|_| "–".to_string());
            let names = self.selected_source_names();
            if self.backup_mode == BackupMode::Drive {
                widgets::kv_row(ui, t!("Art", "Type"), t!("Ganzes Laufwerk", "Whole drive"));
                widgets::kv_row(ui, t!("Laufwerk", "Drive"), &names.join(", "));
                widgets::kv_row(
                    ui,
                    t!("Umfang", "Scope"),
                    if self.cfg.drive_data_only {
                        t!("nur Daten", "data only")
                    } else {
                        t!("alles außer Zwischenspeichern", "everything except caches")
                    },
                );
                widgets::kv_row(ui, t!("Datenmenge", "Amount of data"), &self.selected_size_text());
            } else {
                widgets::kv_row(ui, t!("Art", "Type"), t!("Benutzerprofil", "User profile"));
                widgets::kv_row(ui, t!("Profil", "Profile"), &identity);
                widgets::kv_row(ui, t!("Ordner", "Folders"), &format!("{}", names.len()));
                widgets::kv_row(ui, t!("Datenmenge", "Amount of data"), &self.selected_size_text());
                ui.add(
                    egui::Label::new(RichText::new(names.join(", ")).font(theme::regular(12.0)).color(TEXT_MUTED)).wrap(),
                );
            }
            ui.add_space(4.0);
            let targets: Vec<&Target> = self.cfg.targets.iter().filter(|t| t.selected).collect();
            if targets.is_empty() {
                widgets::kv_row(ui, t!("Ziel", "Target"), t!("noch keines gewählt", "none chosen yet"));
            }
            for target in targets {
                widgets::kv_row(ui, t!("Ziel", "Target"), &format!("{} · {}", target.name, target.place()));
            }
            widgets::kv_row(ui, t!("Komprimierung", "Compression"), self.cfg.compression.label());
            widgets::kv_row(
                ui,
                t!("Prüfung danach", "Verify afterwards"),
                if self.cfg.verify_after_backup { t!("ja", "yes") } else { t!("nein", "no") },
            );
            widgets::kv_row(
                ui,
                t!("Verschlüsselung", "Encryption"),
                if self.cfg.encrypt { t!("AES-256 mit Passwort", "AES-256 with password") } else { t!("keine", "none") },
            );
            widgets::kv_row(ui, t!("Dateiname", "File name"), &backup::archive_file_name(&identity, false));
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(8.0);
            if widgets::button(ui, t!("Als Auftrag speichern", "Save as job"), Kind::Secondary, Some(Icon::Sync)).clicked() {
                save_job = true;
            }
            widgets::text(
                ui,
                if self.cfg.encrypt {
                    t!(
                        "Merkt sich diese Auswahl für Läufe mit einem Klick oder nach Zeitplan. Das Verschlüsselungs-Passwort wird dafür in der Windows-Anmeldeinformationsverwaltung gespeichert.",
                        "Remembers this selection for runs with one click or on a schedule. For this, the encryption password is stored in the Windows Credential Manager."
                    )
                } else {
                    t!(
                        "Merkt sich diese Auswahl. Der Auftrag läuft dann mit einem Klick oder nach Zeitplan.",
                        "Remembers this selection. The job then runs with one click or on a schedule."
                    )
                },
                theme::regular(12.0),
                TEXT_MUTED,
            );
        });
        if save_job {
            self.save_as_job();
        }
    }

    fn card_options(&mut self, ui: &mut Ui) {
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Settings, t!("Optionen", "Options"), "", None);
            widgets::field_label(ui, t!("Komprimierung", "Compression"));
            let options: Vec<(Compression, &str)> = Compression::ALL.iter().map(|c| (*c, c.label())).collect();
            widgets::segmented(ui, "compression", &mut self.cfg.compression, &options);
            widgets::text(ui, self.cfg.compression.description(), theme::regular(12.0), TEXT_MUTED);
            ui.add_space(6.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            widgets::toggle_row(
                ui,
                &mut self.cfg.encrypt,
                t!("Mit Passwort verschlüsseln", "Encrypt with a password"),
                t!(
                    "AES-256. Ohne das Passwort ist die Sicherung nicht wiederherstellbar – es wird nirgends gespeichert.",
                    "AES-256. Without the password the backup cannot be restored – it is not stored anywhere."
                ),
            );
            if self.cfg.encrypt {
                let width = ui.available_width();
                widgets::field_label(ui, t!("Passwort (mindestens 8 Zeichen)", "Password (at least 8 characters)"));
                widgets::text_field(ui, "backup_password", &mut self.backup_password, "", true, width);
                widgets::field_label(ui, t!("Passwort wiederholen", "Repeat password"));
                widgets::text_field(ui, "backup_password_repeat", &mut self.backup_password_repeat, "", true, width);
                if !self.backup_password_repeat.is_empty() && self.backup_password != self.backup_password_repeat {
                    warning_line(ui, t!("Die beiden Eingaben stimmen nicht überein.", "The two entries do not match."));
                }
                widgets::text(
                    ui,
                    t!(
                        "Verschlüsselt werden die Dateiinhalte; Datei- und Ordnernamen bleiben in der ZIP-Datei lesbar. Außerhalb von ZEBack lässt sich die Sicherung z. B. mit 7-Zip öffnen; ältere Windows-Versionen können das nicht von sich aus.",
                        "The file contents are encrypted; file and folder names remain readable in the ZIP file. Outside ZEBack the backup can be opened with 7-Zip, for example; older Windows versions cannot do this on their own."
                    ),
                    theme::regular(12.0),
                    TEXT_MUTED,
                );
            }
            ui.add_space(6.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            widgets::toggle_row(
                ui,
                &mut self.cfg.verify_after_backup,
                t!("Sicherung danach prüfen", "Verify backup afterwards"),
                t!(
                    "Die fertige ZIP-Datei wird einmal komplett gelesen und mit den Prüfsummen verglichen. Dauert etwas länger.",
                    "The finished ZIP file is read completely once and compared with the checksums. Takes a little longer."
                ),
            );
            ui.add_space(6.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            widgets::toggle_row(
                ui,
                &mut self.cfg.skip_online_only,
                t!("Nur-online-Dateien auslassen", "Skip online-only files"),
                t!(
                    "OneDrive-Dateien, die nur in der Cloud liegen, werden nicht extra heruntergeladen.",
                    "OneDrive files that exist only in the cloud are not downloaded just for the backup."
                ),
            );
        });
    }

    // ------------------------------------------------------------------ Wiederherstellen

    fn ui_restore(&mut self, ui: &mut Ui) {
        let info = self.archive.clone();
        match (self.restore_step, info) {
            (0, None) => {
                step_heading(
                    ui,
                    0,
                    t!("Welche Sicherung soll wiederhergestellt werden?", "Which backup do you want to restore?"),
                    t!(
                        "Öffne eine ZIP-Datei oder lass ZEBack auf deinen Sicherungszielen suchen.",
                        "Open a ZIP file or let ZEBack search your backup targets."
                    ),
                );
                self.ui_restore_pick(ui);
            }
            (0, Some(info)) => {
                step_heading(
                    ui,
                    0,
                    t!("Welche Sicherung soll wiederhergestellt werden?", "Which backup do you want to restore?"),
                    t!(
                        "Diese Sicherung ist geöffnet. Passt sie, geht es mit „Weiter“ zum Inhalt.",
                        "This backup is open. If it is the right one, “Next” takes you to its contents."
                    ),
                );
                self.card_archive(ui, &info);
            }
            (_, None) => {
                step_heading(
                    ui,
                    self.restore_step,
                    t!("Noch keine Sicherung geöffnet", "No backup opened yet"),
                    t!("Gehe zurück zu Schritt 1 und öffne eine Sicherung.", "Go back to step 1 and open a backup."),
                );
            }
            (1, Some(info)) => {
                step_heading(
                    ui,
                    1,
                    t!("Was soll wiederhergestellt werden?", "What do you want to restore?"),
                    t!(
                        "Hake ganze Ordner an – oder wähle darunter einzelne Dateien aus.",
                        "Tick whole folders – or pick individual files below."
                    ),
                );
                if info.manifest.is_some() {
                    self.card_contents(ui, &info);
                } else {
                    widgets::card(ui, |ui| {
                        widgets::card_header(ui, Icon::Zip, t!("Fremde ZIP-Datei", "Foreign ZIP file"), "", None);
                        widgets::text(
                            ui,
                            t!(
                                "Diese ZIP-Datei stammt nicht von ZEBack. Ohne Einzelauswahl wird sie vollständig entpackt.",
                                "This ZIP file was not created by ZEBack. Unless you pick individual files, it is extracted completely."
                            ),
                            theme::regular(13.0),
                            TEXT,
                        );
                    });
                }
                ui.add_space(GAP - 8.0);
                self.card_pick(ui);
            }
            (2, Some(info)) => {
                step_heading(
                    ui,
                    2,
                    t!("Wohin soll wiederhergestellt werden?", "Where do you want to restore to?"),
                    t!(
                        "In ein Benutzerprofil dieses PCs oder gesammelt in einen Ordner deiner Wahl.",
                        "Into a user profile on this PC or all together into a folder of your choice."
                    ),
                );
                self.card_restore_target(ui, &info);
            }
            (_, Some(info)) => {
                step_heading(
                    ui,
                    3,
                    t!("Alles bereit?", "All set?"),
                    t!(
                        "Lege fest, was mit vorhandenen Dateien passiert, und starte.",
                        "Decide what happens to existing files, then start."
                    ),
                );
                self.two_columns(ui, Self::card_conflict, move |app, ui| app.card_restore_summary(ui, &info));
            }
        }
    }

    fn ui_restore_pick(&mut self, ui: &mut Ui) {
        let width = ((ui.available_width() - GAP) / 2.0).min(400.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = GAP;
            if widgets::action_tile(
                ui,
                vec2(width, 176.0),
                Icon::OpenFile,
                t!("ZIP-Datei öffnen", "Open ZIP file"),
                t!("USB-Stick, Festplatte oder dieser PC", "USB stick, hard drive or this PC"),
            )
            .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_title(t!("Sicherung auswählen", "Choose a backup"))
                    .add_filter(t!("ZIP-Archiv", "ZIP archive"), &["zip"])
                    .pick_file()
            {
                self.open_archive(path);
            }
            if widgets::action_tile(
                ui,
                vec2(width, 176.0),
                Icon::Drive,
                t!("Auf Sicherungszielen suchen", "Search backup targets"),
                t!("Ordner, Freigaben und Nextcloud", "Folders, shares and Nextcloud"),
            )
            .clicked()
            {
                self.start_backup_search();
            }
        });
        // Systemabbilder sind keine ZIP-Dateien und werden außerhalb von Windows zurückgespielt.
        ui.add_space(GAP);
        let create_stick = ui
            .scope(|ui| {
                ui.set_max_width((width * 2.0 + GAP).min(ui.available_width()));
                Self::card_image_restore_help(ui)
            })
            .inner;
        if create_stick {
            self.open_recovery_wizard();
        }
    }

    fn card_archive(&mut self, ui: &mut Ui, info: &ArchiveInfo) {
        let mut close = false;
        let mut verify = false;
        widgets::card(ui, |ui| {
            let name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            ui.horizontal(|ui| {
                widgets::inline_icon(ui, Icon::Zip, 16.0, ACCENT);
                ui.add(egui::Label::new(RichText::new(name).font(theme::bold(15.0)).color(TEXT_BRIGHT)).truncate());
            });
            ui.add(
                egui::Label::new(
                    RichText::new(info.path.parent().map(|p| p.display().to_string()).unwrap_or_default())
                        .font(theme::regular(12.0))
                        .color(TEXT_FAINT),
                )
                .truncate(),
            );
            ui.add_space(8.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            if let Some(manifest) = &info.manifest {
                let created = DateTime::parse_from_rfc3339(&manifest.created)
                    .map(|d| d.format("%d.%m.%Y, %H:%M").to_string())
                    .unwrap_or_else(|_| manifest.created.clone());
                widgets::kv_row(ui, t!("Erstellt", "Created"), &created);
                widgets::kv_row(ui, t!("Profil", "Profile"), &manifest.user);
                widgets::kv_row(ui, t!("Computer", "Computer"), &manifest.computer);
                widgets::kv_row(ui, t!("Ordner", "Folders"), &manifest.roots.len().to_string());
            } else {
                widgets::kv_row(ui, t!("Herkunft", "Origin"), t!("nicht von ZEBack", "not from ZEBack"));
            }
            widgets::kv_row(ui, t!("ZIP-Größe", "ZIP size"), &fmt_bytes(info.size));
            widgets::kv_row(ui, t!("Entpackt", "Extracted"), &fmt_bytes(info.total_bytes));
            widgets::kv_row(ui, t!("Einträge", "Entries"), &info.entries.to_string());
            widgets::kv_row(ui, t!("Verschlüsselt", "Encrypted"), if info.encrypted { t!("ja", "yes") } else { t!("nein", "no") });
            if let Some(manifest) = info.manifest.as_ref().filter(|m| m.incremental) {
                // Teilsicherung: erklären, was beim Wiederherstellen passiert.
                ui.add_space(6.0);
                match &self.chain {
                    Some(chain) => {
                        widgets::kv_row(
                            ui,
                            t!("Art", "Type"),
                            &tr!("Teilsicherung · Kette aus {} Sicherungen", "Partial backup · chain of {} backups", chain.len()),
                        );
                        widgets::text(
                            ui,
                            t!(
                                "Die Vollsicherung und alle Teilsicherungen bis zu dieser liegen im selben Ordner. Sie werden zusammen wiederhergestellt; von jeder Datei gilt der neueste Stand.",
                                "The full backup and all partial backups up to this one are in the same folder. They are restored together; the newest version of each file is used."
                            ),
                            theme::regular(12.0),
                            TEXT_MUTED,
                        );
                    }
                    None => {
                        widgets::kv_row(ui, t!("Art", "Type"), t!("Teilsicherung", "Partial backup"));
                        warning_line(
                            ui,
                            &tr!(
                                "Diese Sicherung enthält nur, was sich geändert hat. Die Vollsicherung {} liegt nicht im selben Ordner – wiederhergestellt werden deshalb nur die geänderten Dateien.",
                                "This backup only contains what has changed. The full backup {} is not in the same folder – so only the changed files are restored.",
                                manifest.base.as_deref().unwrap_or(t!("dazu", "it is based on"))
                            ),
                        );
                    }
                }
            }
            if info.encrypted {
                ui.add_space(8.0);
                ui.spacing_mut().item_spacing.y = 8.0;
                widgets::field_label(ui, t!("Passwort der Sicherung", "Backup password"));
                let width = ui.available_width().min(420.0);
                widgets::text_field(ui, "restore_password", &mut self.restore_password, "", true, width);
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                verify = widgets::button(ui, t!("Sicherung prüfen", "Verify backup"), Kind::Secondary, Some(Icon::Check)).clicked();
                close = widgets::button(ui, t!("Andere Sicherung", "Other backup"), Kind::Secondary, Some(Icon::OpenFile)).clicked();
            });
        });
        if close {
            self.archive = None;
            self.result = None;
            self.chain = None;
            self.forget_file_selection();
        } else if verify {
            self.start_verify();
        }
    }

    fn card_contents(&mut self, ui: &mut Ui, info: &ArchiveInfo) {
        let Some(manifest) = &info.manifest else { return };
        self.root_checked.resize(manifest.roots.len(), true);
        let badge = format!("{}/{}", self.root_checked.iter().filter(|c| **c).count(), manifest.roots.len());
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Folder, t!("Ordner in der Sicherung", "Folders in the backup"), "", Some(&badge));
            ui.spacing_mut().item_spacing.y = 2.0;
            for (index, root) in manifest.roots.iter().enumerate() {
                let subtitle = tr!("{} Dateien · aus {}", "{} files · from {}", root.files, root.original_path);
                ui.push_id(index, |ui| {
                    widgets::check_row(
                        ui,
                        &mut self.root_checked[index],
                        std_icon(root.kind.as_deref()),
                        &root.name,
                        &subtitle,
                        Some(&fmt_bytes(root.bytes)),
                        false,
                    );
                });
            }
        });
    }

    /// Auswahl einzelner Dateien und Unterordner statt ganzer Ordner.
    fn card_pick(&mut self, ui: &mut Ui) {
        let (mut browse, mut clear) = (false, false);
        let count = self.picked.len();
        widgets::card(ui, |ui| {
            let badge = (count > 0).then(|| count.to_string());
            widgets::card_header(
                ui,
                Icon::Documents,
                t!("Einzelne Dateien", "Individual files"),
                t!("Nur bestimmte Dateien oder Unterordner zurückholen", "Get back only certain files or subfolders"),
                badge.as_deref(),
            );
            if count == 0 {
                widgets::text(
                    ui,
                    t!(
                        "Ohne Auswahl werden die angehakten Ordner vollständig wiederhergestellt.",
                        "Without a selection, the ticked folders are restored completely."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
                ui.add_space(6.0);
                browse = widgets::button(ui, t!("Dateien auswählen", "Select files"), Kind::Secondary, Some(Icon::Folder)).clicked();
            } else {
                let (files, bytes) = self.tree.as_ref().map(|tree| tree.total(&self.picked)).unwrap_or_default();
                widgets::kv_row(
                    ui,
                    t!("Auswahl", "Selection"),
                    &tr!("{count} Einträge · {files} Dateien · {}", "{count} entries · {files} files · {}", fmt_bytes(bytes)),
                );
                widgets::text(
                    ui,
                    t!(
                        "Es wird nur diese Auswahl wiederhergestellt; die Häkchen an den Ordnern gelten dann nicht.",
                        "Only this selection is restored; the ticks on the folders then do not apply."
                    ),
                    theme::regular(12.0),
                    TEXT_MUTED,
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    browse = widgets::button(ui, t!("Auswahl ändern", "Change selection"), Kind::Secondary, Some(Icon::Folder)).clicked();
                    clear = widgets::button(ui, t!("Auswahl aufheben", "Clear selection"), Kind::Ghost, Some(Icon::Close)).clicked();
                });
            }
        });
        if clear {
            self.picked.clear();
        } else if browse {
            self.start_browse();
        }
    }

    fn card_restore_target(&mut self, ui: &mut Ui, info: &ArchiveInfo) {
        widgets::card(ui, |ui| {
            if info.is_drive_backup() {
                widgets::card_header(
                    ui,
                    Icon::Folder,
                    t!("Zielordner", "Target folder"),
                    t!(
                        "Laufwerkssicherungen werden in einen Ordner deiner Wahl zurückgespielt",
                        "Drive backups are restored into a folder of your choice"
                    ),
                    None,
                );
            } else if info.fits_profile() {
                widgets::card_header(ui, Icon::Home, t!("Ziel", "Target"), "", None);
                if widgets::option_row(
                    ui,
                    self.restore_mode == RestoreMode::Profile,
                    Mark::Radio,
                    Icon::User,
                    t!("In ein Benutzerprofil", "Into a user profile"),
                    t!(
                        "Desktop, Dokumente usw. landen in den Ordnern des Profils",
                        "Desktop, Documents etc. end up in the profile's folders"
                    ),
                )
                .clicked()
                {
                    self.restore_mode = RestoreMode::Profile;
                }
                if widgets::option_row(
                    ui,
                    self.restore_mode == RestoreMode::Folder,
                    Mark::Radio,
                    Icon::Folder,
                    t!("Anderer Ordner", "Another folder"),
                    t!("Alles gesammelt in einen Ordner legen", "Put everything together into one folder"),
                )
                .clicked()
                {
                    self.restore_mode = RestoreMode::Folder;
                }
            } else {
                widgets::card_header(
                    ui,
                    Icon::Folder,
                    t!("Zielordner", "Target folder"),
                    t!("Die ZIP-Datei wird vollständig hierhin entpackt", "The ZIP file is extracted completely into this folder"),
                    None,
                );
            }
            if self.restore_mode == RestoreMode::Folder || !info.fits_profile() {
                ui.horizontal(|ui| {
                    let button = widgets::button_width(ui, t!("Auswählen", "Browse"), Kind::Secondary, true);
                    let width = ui.available_width() - button - ui.spacing().item_spacing.x;
                    widgets::text_field(
                        ui,
                        "restore_dir",
                        &mut self.restore_dir,
                        t!("Zielordner, z. B. D:\\Restore", "Target folder, e.g. D:\\Restore"),
                        false,
                        width,
                    );
                    if widgets::button(ui, t!("Auswählen", "Browse"), Kind::Secondary, Some(Icon::Folder)).clicked()
                        && let Some(path) = pick_folder(t!("Zielordner auswählen", "Choose a target folder"))
                    {
                        self.restore_dir = path.display().to_string();
                    }
                });
            }
        });

        if self.restore_mode == RestoreMode::Profile && info.fits_profile() {
            ui.add_space(GAP - 8.0);
            widgets::card(ui, |ui| {
                widgets::card_header(
                    ui,
                    Icon::User,
                    t!("In welches Profil?", "Into which profile?"),
                    t!(
                        "Passt auch, wenn der Benutzer auf diesem PC anders heißt als in der Sicherung",
                        "Also works if the user has a different name on this PC than in the backup"
                    ),
                    None,
                );
                profile_rows(ui, &self.profiles, &mut self.restore_profile);
            });
        }
    }

    fn card_conflict(&mut self, ui: &mut Ui) {
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Sync,
                t!("Vorhandene Dateien", "Existing files"),
                t!("Wenn eine Datei am Ziel schon existiert", "When a file already exists at the target"),
                None,
            );
            if widgets::option_row(
                ui,
                self.conflict == Conflict::Skip,
                Mark::Radio,
                Icon::Check,
                t!("Behalten", "Keep"),
                t!("Nur fehlende Dateien wiederherstellen", "Restore only missing files"),
            )
            .clicked()
            {
                self.conflict = Conflict::Skip;
            }
            if widgets::option_row(
                ui,
                self.conflict == Conflict::Overwrite,
                Mark::Radio,
                Icon::Sync,
                t!("Ersetzen", "Replace"),
                t!("Durch die Version aus der Sicherung überschreiben", "Overwrite with the version from the backup"),
            )
            .clicked()
            {
                self.conflict = Conflict::Overwrite;
            }
            if self.conflict == Conflict::Overwrite {
                warning_line(ui, t!("Neuere Dateien am Ziel gehen dabei verloren.", "Newer files at the target will be lost."));
            }
        });
    }

    fn card_restore_summary(&mut self, ui: &mut Ui, info: &ArchiveInfo) {
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Restore, t!("Zusammenfassung", "Summary"), "", None);
            ui.spacing_mut().item_spacing.y = 2.0;
            let name = info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            widgets::kv_row(ui, t!("Sicherung", "Backup"), &name);
            if !self.picked.is_empty() {
                let (files, bytes) = self.tree.as_ref().map(|tree| tree.total(&self.picked)).unwrap_or_default();
                widgets::kv_row(
                    ui,
                    t!("Nur die Auswahl", "Selection only"),
                    &tr!("{files} Dateien · {}", "{files} files · {}", fmt_bytes(bytes)),
                );
            }
            match &info.manifest {
                Some(manifest) => {
                    let selected: Vec<&ManifestRoot> =
                        manifest.roots.iter().zip(&self.root_checked).filter(|(_, c)| **c).map(|(r, _)| r).collect();
                    widgets::kv_row(
                        ui,
                        t!("Ordner", "Folders"),
                        &tr!("{} von {}", "{} of {}", selected.len(), manifest.roots.len()),
                    );
                    let target = match self.restore_mode {
                        RestoreMode::Profile => self
                            .profiles
                            .get(self.restore_profile)
                            .map(|p| tr!("Profil {}", "Profile {}", p.name))
                            .unwrap_or_default(),
                        RestoreMode::Folder => self.restore_dir.trim().to_string(),
                    };
                    widgets::kv_row(ui, t!("Ziel", "Target"), &target);
                    ui.add_space(4.0);
                    // Wohin jeder Ordner genau geht
                    for root in selected {
                        let path = self
                            .root_target(manifest, root)
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| t!("Zielordner fehlt", "Target folder missing").to_string());
                        widgets::kv_row(ui, &root.name, &path);
                    }
                }
                None => {
                    widgets::kv_row(ui, t!("Inhalt", "Contents"), t!("gesamtes Archiv", "entire archive"));
                    widgets::kv_row(ui, t!("Ziel", "Target"), self.restore_dir.trim());
                }
            }
        });
    }

    // ------------------------------------------------------------------ Sicherungsziele und Protokoll

    /// Seite "Sicherungsziele": links die Liste, rechts das ausgewählte Ziel zum Bearbeiten.
    fn ui_targets(&mut self, ui: &mut Ui) {
        self.target_edit = self.target_edit.min(self.cfg.targets.len().saturating_sub(1));
        self.two_columns(ui, Self::card_target_list, |app, ui| {
            app.card_target_editor(ui);
            if app.cfg.targets.get(app.target_edit).is_some_and(|t| t.kind == TargetKind::Nextcloud) {
                ui.add_space(GAP - 8.0);
                Self::card_app_password_help(ui);
            }
        });
    }

    fn card_target_list(&mut self, ui: &mut Ui) {
        let mut select = None;
        let mut add = None;
        widgets::card(ui, |ui| {
            let badge = self.cfg.targets.len().to_string();
            widgets::card_header(
                ui,
                Icon::Drive,
                t!("Sicherungsziele", "Backup targets"),
                t!("Orte, an die gesichert werden kann", "Places you can back up to"),
                Some(&badge),
            );
            if self.cfg.targets.is_empty() {
                widgets::text(
                    ui,
                    t!(
                        "Noch kein Ziel eingerichtet. Lege unten das erste an – etwa einen Ordner auf einem USB-Stick.",
                        "No target set up yet. Create the first one below – for example a folder on a USB stick."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
            }
            for (index, target) in self.cfg.targets.iter().enumerate() {
                let clicked = ui
                    .push_id(("edit_target", target.id), |ui| {
                        widgets::option_row(
                            ui,
                            index == self.target_edit,
                            Mark::Radio,
                            target_icon(target.kind),
                            &target.name,
                            &target.summary(),
                        )
                        .clicked()
                    })
                    .inner;
                if clicked {
                    select = Some(index);
                }
            }
            ui.add_space(6.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            widgets::field_label(ui, t!("Neues Ziel anlegen", "Add a new target"));
            ui.horizontal_wrapped(|ui| {
                for (kind, label) in [
                    (TargetKind::Local, t!("Ordner / USB", "Folder / USB")),
                    (TargetKind::Smb, t!("Netzwerkfreigabe", "Network share")),
                    (TargetKind::Nextcloud, "Nextcloud"),
                    (TargetKind::WebDav, "WebDAV"),
                    (TargetKind::S3, t!("S3-Speicher", "S3 storage")),
                    (TargetKind::Sftp, "SFTP"),
                ] {
                    if widgets::button(ui, label, Kind::Secondary, Some(target_icon(kind))).clicked() {
                        add = Some(kind);
                    }
                }
            });
        });
        if let Some(index) = select {
            self.target_edit = index;
            self.target_delete = None;
        }
        match add {
            Some(TargetKind::Local) => {
                if let Some(path) = pick_folder(t!("Zielordner auswählen", "Choose a target folder")) {
                    self.target_edit = self.add_target(TargetKind::Local, &path.display().to_string());
                }
            }
            Some(kind) => self.target_edit = self.add_target(kind, ""),
            None => {}
        }
    }

    fn card_target_editor(&mut self, ui: &mut Ui) {
        let index = self.target_edit;
        let Some(target) = self.cfg.targets.get_mut(index) else {
            widgets::card(ui, |ui| {
                widgets::card_header(
                    ui,
                    Icon::Settings,
                    t!("Ziel bearbeiten", "Edit target"),
                    t!("Noch kein Ziel vorhanden", "No target yet"),
                    None,
                );
                widgets::text(
                    ui,
                    t!(
                        "Ein Sicherungsziel ist ein Ort, an dem die ZIP-Datei landet: ein Ordner oder USB-Stick, eine Netzwerkfreigabe (SMB) oder eine Nextcloud. Du kannst mehrere anlegen und beim Sichern eines oder mehrere anhaken.",
                        "A backup target is a place where the ZIP file ends up: a folder or USB stick, a network share (SMB) or a Nextcloud. You can create several and tick one or more of them when backing up."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
            });
            return;
        };
        let id = target.id;
        let kind = target.kind;
        let password = self.target_passwords.entry(id).or_default();
        let confirm_delete = self.target_delete == Some(id);
        let (mut pick, mut save, mut test, mut delete, mut browser_login) = (false, false, false, false, false);
        let mut pick_key = false;
        widgets::card(ui, |ui| {
            widgets::card_header(ui, target_icon(kind), t!("Ziel bearbeiten", "Edit target"), kind.label(), None);
            let width = ui.available_width();
            ui.push_id(("target_editor", id), |ui| {
                widgets::field_label(ui, t!("Name", "Name"));
                widgets::text_field(ui, "name", &mut target.name, t!("z. B. USB-Festplatte", "e.g. USB hard drive"), false, width);
                ui.add_space(4.0);
                match kind {
                    TargetKind::Local => {
                        widgets::field_label(ui, t!("Ordner", "Folder"));
                        ui.horizontal(|ui| {
                            let button = widgets::button_width(ui, t!("Auswählen", "Browse"), Kind::Secondary, true);
                            let field = width - button - ui.spacing().item_spacing.x;
                            widgets::text_field(ui, "path", &mut target.path, t!("z. B. E:\\Backups", "e.g. E:\\Backups"), false, field);
                            pick = widgets::button(ui, t!("Auswählen", "Browse"), Kind::Secondary, Some(Icon::Folder)).clicked();
                        });
                    }
                    TargetKind::Smb => {
                        widgets::field_label(ui, t!("Freigabe", "Share"));
                        widgets::text_field(
                            ui,
                            "path",
                            &mut target.path,
                            t!("\\\\server\\freigabe\\ordner", "\\\\server\\share\\folder"),
                            false,
                            width,
                        );
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Benutzername (leer = Windows-Anmeldung)", "User name (empty = Windows sign-in)"));
                        widgets::text_field(ui, "user", &mut target.user, t!("z. B. NAS\\backup", "e.g. NAS\\backup"), false, width);
                        if target.needs_password() {
                            ui.add_space(4.0);
                            widgets::field_label(ui, t!("Passwort", "Password"));
                            widgets::text_field(ui, "password", password, "", true, width);
                        }
                    }
                    TargetKind::Nextcloud => {
                        widgets::field_label(ui, t!("Server-Adresse", "Server address"));
                        widgets::text_field(ui, "url", &mut target.url, "https://cloud.example.de", false, width);
                        ui.add_space(6.0);
                        browser_login =
                            widgets::button(ui, t!("Im Browser anmelden", "Sign in via browser"), Kind::Primary, Some(Icon::Globe)).clicked();
                        widgets::text(
                            ui,
                            t!(
                                "Öffnet die Anmeldeseite deiner Nextcloud. Nach dem Bestätigen trägt die App Benutzername und App-Passwort selbst ein – oder du füllst beides von Hand aus:",
                                "Opens the sign-in page of your Nextcloud. After you confirm, the app enters the user name and app password itself – or you fill in both by hand:"
                            ),
                            theme::regular(12.0),
                            TEXT_MUTED,
                        );
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Benutzername", "User name"));
                        widgets::text_field(ui, "user", &mut target.user, "", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("App-Passwort", "App password"));
                        widgets::text_field(ui, "password", password, "", true, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Ordner auf dem Server", "Folder on the server"));
                        widgets::text_field(ui, "remote_dir", &mut target.remote_dir, "Backups", false, width);
                    }
                    TargetKind::WebDav => {
                        widgets::field_label(ui, t!("WebDAV-Adresse", "WebDAV address"));
                        widgets::text_field(ui, "url", &mut target.url, "https://server.example.de/dav", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Benutzername", "User name"));
                        widgets::text_field(ui, "user", &mut target.user, "", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Passwort", "Password"));
                        widgets::text_field(ui, "password", password, "", true, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Ordner unter dieser Adresse", "Folder under this address"));
                        widgets::text_field(ui, "remote_dir", &mut target.remote_dir, "Backups", false, width);
                    }
                    TargetKind::S3 => {
                        widgets::field_label(ui, t!("Adresse des Dienstes", "Service address"));
                        widgets::text_field(ui, "url", &mut target.url, "https://s3.eu-central-1.amazonaws.com", false, width);
                        ui.add_space(4.0);
                        let half = (width - ui.spacing().item_spacing.x) / 2.0;
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                widgets::field_label(ui, t!("Bucket", "Bucket"));
                                widgets::text_field(ui, "bucket", &mut target.path, t!("meine-sicherungen", "my-backups"), false, half);
                            });
                            ui.vertical(|ui| {
                                ui.set_width(half);
                                widgets::field_label(ui, t!("Region", "Region"));
                                widgets::text_field(ui, "region", &mut target.region, "us-east-1", false, half);
                            });
                        });
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Zugangsschlüssel (Access Key)", "Access key"));
                        widgets::text_field(ui, "user", &mut target.user, "", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Geheimschlüssel (Secret Key)", "Secret key"));
                        widgets::text_field(ui, "password", password, "", true, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Ordner im Bucket", "Folder in the bucket"));
                        widgets::text_field(ui, "remote_dir", &mut target.remote_dir, "Backups", false, width);
                    }
                    TargetKind::Sftp => {
                        widgets::field_label(ui, t!("Server (mit :Port, falls nicht 22)", "Server (with :port if not 22)"));
                        widgets::text_field(ui, "url", &mut target.url, "server.example.de", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Benutzername", "User name"));
                        widgets::text_field(ui, "user", &mut target.user, "", false, width);
                        ui.add_space(4.0);
                        widgets::field_label(
                            ui,
                            t!("Schlüsseldatei (leer = Anmeldung mit Passwort)", "Key file (empty = sign in with password)"),
                        );
                        ui.horizontal(|ui| {
                            let button = widgets::button_width(ui, t!("Auswählen", "Browse"), Kind::Secondary, true);
                            let field = width - button - ui.spacing().item_spacing.x;
                            widgets::text_field(
                                ui,
                                "key_file",
                                &mut target.path,
                                t!("z. B. C:\\Users\\…\\.ssh\\id_ed25519", "e.g. C:\\Users\\…\\.ssh\\id_ed25519"),
                                false,
                                field,
                            );
                            pick_key = widgets::button(ui, t!("Auswählen", "Browse"), Kind::Secondary, Some(Icon::Key)).clicked();
                        });
                        if target.needs_password() {
                            ui.add_space(4.0);
                            widgets::field_label(ui, t!("Passwort", "Password"));
                            widgets::text_field(ui, "password", password, "", true, width);
                        }
                        ui.add_space(4.0);
                        widgets::field_label(ui, t!("Ordner auf dem Server", "Folder on the server"));
                        widgets::text_field(ui, "remote_dir", &mut target.remote_dir, "Backups", false, width);
                        ui.add_space(4.0);
                        widgets::text(
                            ui,
                            t!(
                                "Nutzt den OpenSSH-Client von Windows. Der Server wird beim ersten Verbinden gemerkt; meldet er sich später mit einem anderen Schlüssel, wird die Verbindung abgelehnt. Die Schlüsseldatei darf kein Kennwort haben.",
                                "Uses the Windows OpenSSH client. The server is remembered on first connection; if it later presents a different key, the connection is refused. The key file must not have a passphrase."
                            ),
                            theme::regular(12.0),
                            TEXT_MUTED,
                        );
                    }
                }
                if target.needs_password() {
                    ui.add_space(8.0);
                    widgets::divider(ui);
                    ui.add_space(6.0);
                    widgets::toggle_row(
                        ui,
                        &mut target.save_password,
                        t!("Passwort merken", "Remember password"),
                        t!(
                            "Geschützt in der Windows-Anmeldeinformationsverwaltung, nicht in einer Datei.",
                            "Protected in the Windows Credential Manager, not in a file."
                        ),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    // Bei Nextcloud ist die Anmeldung im Browser der Hauptweg, Speichern tritt zurück.
                    let save_kind = if kind == TargetKind::Nextcloud { Kind::Secondary } else { Kind::Primary };
                    save = widgets::button(ui, t!("Speichern", "Save"), save_kind, Some(Icon::Check)).clicked();
                    test = widgets::button(ui, t!("Prüfen", "Check"), Kind::Secondary, Some(Icon::Sync)).clicked();
                    let label = if confirm_delete { t!("Wirklich löschen?", "Really delete?") } else { t!("Löschen", "Delete") };
                    delete = widgets::button(ui, label, Kind::Danger, Some(Icon::Delete)).clicked();
                });
            });
        });
        if pick
            && let Some(path) = pick_folder(t!("Zielordner auswählen", "Choose a target folder"))
        {
            self.cfg.targets[index].path = path.display().to_string();
        }
        if pick_key {
            let mut dialog =
                rfd::FileDialog::new().set_title(t!("Private Schlüsseldatei auswählen", "Choose the private key file"));
            if let Some(home) = dirs::home_dir() {
                dialog = dialog.set_directory(home.join(".ssh"));
            }
            if let Some(path) = dialog.pick_file() {
                self.cfg.targets[index].path = path.display().to_string();
            }
        }
        if save {
            self.target_delete = None;
            self.persist();
            self.persist_target_passwords();
            self.set_result(Level::Ok, t!("Sicherungsziele gespeichert.", "Backup targets saved."));
        }
        if test {
            self.target_delete = None;
            self.start_target_test(index);
        }
        if browser_login {
            self.target_delete = None;
            self.start_browser_login(index);
        }
        if delete {
            if confirm_delete {
                self.delete_target(index);
            } else {
                // Erst beim zweiten Klick löschen.
                self.target_delete = Some(id);
            }
        }
    }

    /// Entfernt ein Ziel samt gemerktem Passwort.
    fn delete_target(&mut self, index: usize) {
        if index >= self.cfg.targets.len() {
            return;
        }
        let target = self.cfg.targets.remove(index);
        self.target_passwords.remove(&target.id);
        self.target_delete = None;
        if self.image_smb == Some(target.id) {
            self.image_smb = None;
        }
        self.target_edit = index.min(self.cfg.targets.len().saturating_sub(1));
        #[cfg(debug_assertions)]
        let keep_stored = std::env::var_os("ZEBACK_NO_SAVE").is_some();
        #[cfg(not(debug_assertions))]
        let keep_stored = false;
        if !keep_stored && let Some(key) = target.password_key() {
            config::delete_password(&key);
        }
        self.persist();
        self.set_result(Level::Ok, tr!("Ziel „{}“ gelöscht.", "Target “{}” deleted.", target.name));
    }

    // ------------------------------------------------------------------ Seite "Berichte"

    fn ui_reports(&mut self, ui: &mut Ui) {
        self.two_columns(
            ui,
            |app, ui| {
                app.card_notify(ui);
                ui.add_space(GAP - 8.0);
                app.card_updates(ui);
            },
            Self::card_mail,
        );
    }

    /// Update-Hinweis: Einstellung, Abfrage von Hand und – falls vorhanden – die neue Version.
    fn card_updates(&mut self, ui: &mut Ui) {
        let (mut check, mut open) = (false, false);
        let before = self.cfg.check_updates;
        let busy = self.update_rx.is_some();
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Sync,
                t!("Updates", "Updates"),
                &tr!("Du verwendest Version {}", "You are using version {}", env!("CARGO_PKG_VERSION")),
                None,
            );
            widgets::toggle_row(
                ui,
                &mut self.cfg.check_updates,
                t!("Beim Start nach Updates suchen", "Check for updates at startup"),
                t!(
                    "Fragt bei zeba.xyz nach der neuesten Versionsnummer. Heruntergeladen oder installiert wird dabei nichts.",
                    "Asks zeba.xyz for the latest version number. Nothing is downloaded or installed."
                ),
            );
            if let Some(release) = &self.update {
                ui.add_space(8.0);
                widgets::divider(ui);
                ui.add_space(6.0);
                widgets::kv_row(ui, t!("Neue Version", "New version"), &release.version);
                if !release.notes.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&release.notes).font(theme::regular(12.0)).color(TEXT_MUTED)).wrap());
                }
            }
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                if self.update.is_some() {
                    open = widgets::button(ui, t!("Download-Seite öffnen", "Open download page"), Kind::Primary, Some(Icon::Globe)).clicked();
                }
                // Direkt in der umbrechenden Zeile (ein verschachtelter Bereich würde nicht umbrechen);
                // während einer laufenden Abfrage zählt der Klick nicht.
                let label = if busy { t!("Wird geprüft …", "Checking …") } else { t!("Jetzt prüfen", "Check now") };
                check = widgets::button(ui, label, Kind::Secondary, Some(Icon::Sync)).clicked() && !busy;
            });
        });
        if self.cfg.check_updates != before {
            self.persist();
        }
        if check {
            self.start_update_check(true);
        }
        if open && let Some(release) = self.update.clone() {
            self.open_download_page(&release);
        }
    }

    fn card_notify(&mut self, ui: &mut Ui) {
        let (mut save, mut test) = (false, false);
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Desktop,
                t!("Windows-Benachrichtigung", "Windows notification"),
                t!("Nach Aufträgen, die ohne Fenster laufen", "After jobs that run without a window"),
                None,
            );
            widgets::segmented(
                ui,
                "notify_mode",
                &mut self.cfg.notify,
                &[
                    (ReportMode::Never, t!("Nie", "Never")),
                    (ReportMode::Errors, t!("Bei Fehlern", "On errors")),
                    (ReportMode::Always, t!("Immer", "Always")),
                ],
            );
            widgets::text(
                ui,
                match self.cfg.notify {
                    ReportMode::Never => t!(
                        "Geplante Aufträge laufen still – auch wenn sie scheitern.",
                        "Scheduled jobs run silently – even if they fail."
                    ),
                    ReportMode::Errors => t!(
                        "Scheitert ein geplanter Auftrag, erscheint ein Hinweisfenster, das stehen bleibt, bis du es schließt.",
                        "If a scheduled job fails, a message window appears and stays until you close it."
                    ),
                    ReportMode::Always => t!(
                        "Zusätzlich meldet eine kurze Benachrichtigung jeden gelungenen Lauf.",
                        "In addition, a short notification reports every successful run."
                    ),
                },
                theme::regular(12.0),
                TEXT_MUTED,
            );
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                save = widgets::button(ui, t!("Speichern", "Save"), Kind::Primary, Some(Icon::Check)).clicked();
                test = widgets::button(ui, t!("Probe anzeigen", "Show sample"), Kind::Secondary, Some(Icon::Desktop)).clicked();
            });
        });
        if save {
            self.save_reports();
        }
        if test {
            crate::notify::show(
                t!("Sicherung „Beispiel“ gelungen", "Backup “Example” succeeded"),
                t!("1 234 Dateien (5,6 GB) gesichert.", "1,234 files (5.6 GB) backed up."),
                false,
            );
            self.set_result(
                Level::Ok,
                t!(
                    "Probe angezeigt\nDie Benachrichtigung erscheint unten rechts und verschwindet nach einigen Sekunden.",
                    "Sample shown\nThe notification appears at the bottom right and disappears after a few seconds."
                ),
            );
        }
    }

    fn card_mail(&mut self, ui: &mut Ui) {
        let (mut save, mut test) = (false, false);
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Mail,
                t!("E-Mail-Bericht", "Email report"),
                t!("Nach jedem Auftrag, ob geplant oder von Hand gestartet", "After every job, whether scheduled or started manually"),
                None,
            );
            let mail = &mut self.cfg.mail;
            widgets::segmented(
                ui,
                "mail_mode",
                &mut mail.mode,
                &[
                    (ReportMode::Never, t!("Aus", "Off")),
                    (ReportMode::Errors, t!("Bei Fehlern", "On errors")),
                    (ReportMode::Always, t!("Immer", "Always")),
                ],
            );
            ui.add_space(6.0);
            let width = ui.available_width();
            widgets::field_label(ui, t!("Mailserver (SMTP)", "Mail server (SMTP)"));
            widgets::text_field(ui, "mail_server", &mut mail.server, "smtp.example.de", false, width);
            ui.add_space(4.0);
            widgets::field_label(ui, t!("Verschlüsselung", "Encryption"));
            let before = mail.starttls;
            widgets::segmented(ui, "mail_tls", &mut mail.starttls, &[(false, "TLS · Port 465"), (true, "STARTTLS · Port 587")]);
            if mail.starttls != before {
                mail.port = if mail.starttls { 587 } else { 465 };
            }
            ui.add_space(4.0);
            widgets::field_label(ui, t!("Benutzername", "User name"));
            widgets::text_field(
                ui,
                "mail_user",
                &mut mail.user,
                t!("meist die E-Mail-Adresse", "usually the email address"),
                false,
                width,
            );
            ui.add_space(4.0);
            widgets::field_label(ui, t!("Passwort", "Password"));
            widgets::text_field(ui, "mail_password", &mut self.mail_password, "", true, width);
            ui.add_space(4.0);
            widgets::field_label(ui, t!("Absender", "Sender"));
            widgets::text_field(ui, "mail_from", &mut mail.from, t!("berichte@example.de", "reports@example.com"), false, width);
            ui.add_space(4.0);
            widgets::field_label(ui, t!("Empfänger (mehrere mit Komma)", "Recipients (separate several with commas)"));
            widgets::text_field(ui, "mail_to", &mut mail.to, t!("du@example.de", "you@example.com"), false, width);
            ui.add_space(4.0);
            widgets::text(
                ui,
                t!(
                    "Gesendet wird nur verschlüsselt. Das Passwort liegt in der Windows-Anmeldeinformationsverwaltung, nicht in einer Datei.",
                    "Mail is only sent encrypted. The password is kept in the Windows Credential Manager, not in a file."
                ),
                theme::regular(12.0),
                TEXT_MUTED,
            );
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                save = widgets::button(ui, t!("Speichern", "Save"), Kind::Primary, Some(Icon::Check)).clicked();
                test = widgets::button(ui, t!("Testnachricht senden", "Send test message"), Kind::Secondary, Some(Icon::Mail)).clicked();
            });
        });
        if save {
            self.save_reports();
        }
        if test {
            self.start_test_mail();
        }
    }

    // ------------------------------------------------------------------ Seite "Aufträge"

    fn ui_jobs(&mut self, ui: &mut Ui) {
        self.job_edit = self.job_edit.min(self.cfg.jobs.len().saturating_sub(1));
        self.two_columns(ui, Self::card_job_list, Self::card_job_editor);
    }

    fn card_job_list(&mut self, ui: &mut Ui) {
        let mut select = None;
        let mut create = false;
        widgets::card(ui, |ui| {
            let badge = self.cfg.jobs.len().to_string();
            widgets::card_header(ui, Icon::Sync, t!("Aufträge", "Jobs"), t!("Gespeicherte Sicherungen", "Saved backups"), Some(&badge));
            if self.cfg.jobs.is_empty() {
                widgets::text(
                    ui,
                    t!(
                        "Noch kein Auftrag gespeichert. Stelle unter „Sichern“ zusammen, was wohin gesichert werden soll, und wähle im letzten Schritt „Als Auftrag speichern“.",
                        "No job saved yet. Under “Back up”, put together what should be backed up where, and choose “Save as job” in the last step."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
            }
            for (index, job) in self.cfg.jobs.iter().enumerate() {
                let keep = match job.keep_last {
                    0 => t!("behält alle", "keeps all").to_string(),
                    n => tr!("behält {n}", "keeps {n}"),
                };
                let description =
                    tr!("{} · {} Ziel(e) · {keep}", "{} · {} target(s) · {keep}", job.schedule_text(), job.target_ids.len());
                let clicked = ui
                    .push_id(("job", job.id), |ui| {
                        widgets::option_row(ui, index == self.job_edit, Mark::Radio, Icon::Sync, &job.name, &description).clicked()
                    })
                    .inner;
                if clicked {
                    select = Some(index);
                }
            }
            ui.add_space(6.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            create = widgets::button(ui, t!("Neuer Auftrag", "New job"), Kind::Secondary, Some(Icon::Add)).clicked();
        });
        if let Some(index) = select {
            self.job_edit = index;
            self.job_delete = None;
        }
        if create {
            self.switch_tab(Tab::Backup);
            self.backup_step = 0;
            self.set_result(
                Level::Ok,
                t!(
                    "Neuer Auftrag\nStelle die Sicherung zusammen und wähle im letzten Schritt „Als Auftrag speichern“.",
                    "New job\nPut the backup together and choose “Save as job” in the last step."
                ),
            );
        }
    }

    fn card_job_editor(&mut self, ui: &mut Ui) {
        let index = self.job_edit;
        let Some(job) = self.cfg.jobs.get_mut(index) else {
            widgets::card(ui, |ui| {
                widgets::card_header(ui, Icon::Settings, t!("Auftrag", "Job"), t!("Noch keiner vorhanden", "None yet"), None);
                widgets::text(
                    ui,
                    t!(
                        "Ein Auftrag merkt sich, was wohin gesichert wird. Er läuft mit einem Klick, nach Zeitplan über die Windows-Aufgabenplanung oder aus einem Skript – und räumt auf Wunsch alte Sicherungen weg.",
                        "A job remembers what is backed up where. It runs with one click, on a schedule via Windows Task Scheduler or from a script – and clears out old backups if you want."
                    ),
                    theme::regular(13.0),
                    TEXT,
                );
            });
            return;
        };
        let id = job.id;
        let scope = match job.sources.first() {
            Some(source) if source.kind.as_deref() == Some("drive") => {
                tr!("{} (ganzes Laufwerk)", "{} (whole drive)", source.path.display())
            }
            _ => tr!("{} · {} Ordner", "{} · {} folders", job.profile_name, job.sources.len()),
        };
        let target_names: Vec<String> = job
            .target_ids
            .iter()
            .map(|id| match self.cfg.targets.iter().find(|t| t.id == *id) {
                Some(target) => target.name.clone(),
                None => t!("gelöschtes Ziel", "deleted target").to_string(),
            })
            .collect();
        let last_run = self.history.iter().rev().find(|r| r.job_id == id).cloned();
        let confirm_delete = self.job_delete == Some(id);
        let (mut run, mut save, mut delete) = (false, false, false);
        widgets::card(ui, |ui| {
            widgets::card_header(ui, Icon::Sync, t!("Auftrag", "Job"), t!("Zeitplan und Aufbewahrung", "Schedule and retention"), None);
            let width = ui.available_width();
            ui.push_id(("job_editor", id), |ui| {
                widgets::field_label(ui, t!("Name", "Name"));
                widgets::text_field(ui, "name", &mut job.name, t!("z. B. Büro-PC täglich", "e.g. Office PC daily"), false, width);
                ui.add_space(6.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    widgets::kv_row(ui, t!("Sichert", "Backs up"), &scope);
                    widgets::kv_row(ui, t!("Ziele", "Targets"), &target_names.join(" + "));
                    widgets::kv_row(
                        ui,
                        t!("Verschlüsselung", "Encryption"),
                        if job.encrypt { t!("AES-256, Passwort gespeichert", "AES-256, password stored") } else { t!("keine", "none") },
                    );
                });

                ui.add_space(8.0);
                widgets::divider(ui);
                ui.add_space(6.0);
                widgets::field_label(ui, t!("Zeitplan", "Schedule"));
                widgets::segmented(
                    ui,
                    "schedule",
                    &mut job.schedule,
                    &[
                        (ScheduleKind::Off, t!("Aus", "Off")),
                        (ScheduleKind::Daily, t!("Täglich", "Daily")),
                        (ScheduleKind::Weekly, t!("Wöchentlich", "Weekly")),
                    ],
                );
                if job.schedule == ScheduleKind::Weekly {
                    ui.add_space(4.0);
                    let days: Vec<(u8, &str)> =
                        config::weekdays().iter().enumerate().map(|(day, name)| (day as u8, *name)).collect();
                    widgets::segmented(ui, "weekday", &mut job.weekday, &days);
                }
                if job.schedule != ScheduleKind::Off {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        widgets::text(ui, t!("um", "at"), theme::regular(13.0), TEXT_MUTED);
                        number_stepper(ui, "hour", &mut job.hour, 24, 1);
                        widgets::text(ui, ":", theme::bold(14.0), TEXT_BRIGHT);
                        number_stepper(ui, "minute", &mut job.minute, 60, 5);
                        // Im Englischen steht nach der Uhrzeit kein Wort.
                        widgets::text(ui, t!("Uhr", ""), theme::regular(13.0), TEXT_MUTED);
                    });
                }

                ui.add_space(8.0);
                widgets::toggle_row(
                    ui,
                    &mut job.incremental,
                    t!("Nur Geändertes sichern", "Back up changes only"),
                    t!(
                        "Zwischen den Vollsicherungen entstehen kleine Teilsicherungen mit den neuen und geänderten Dateien.",
                        "Between the full backups, small partial backups with the new and changed files are created."
                    ),
                );
                if job.incremental {
                    ui.add_space(4.0);
                    widgets::field_label(ui, t!("Vollsicherung bei jedem …", "Full backup on every …"));
                    widgets::segmented(
                        ui,
                        "full_every",
                        &mut job.full_every,
                        &[(7, t!("7. Lauf", "7th run")), (14, t!("14. Lauf", "14th run")), (30, t!("30. Lauf", "30th run"))],
                    );
                }

                ui.add_space(8.0);
                widgets::field_label(
                    ui,
                    if job.incremental {
                        t!(
                            "Vollsicherungen (samt Teilsicherungen) je Ziel aufbewahren",
                            "Full backups (with their partial backups) to keep per target"
                        )
                    } else {
                        t!("Sicherungen je Ziel aufbewahren", "Backups to keep per target")
                    },
                );
                widgets::segmented(
                    ui,
                    "keep",
                    &mut job.keep_last,
                    &[(0, t!("Alle", "All")), (3, "3"), (7, "7"), (14, "14"), (30, "30")],
                );
                widgets::text(
                    ui,
                    if job.keep_last == 0 {
                        t!("Es wird nie etwas gelöscht.", "Nothing is ever deleted.").to_string()
                    } else if job.incremental {
                        tr!(
                            "Nach einem gelungenen Lauf bleiben die {} neuesten Vollsicherungen dieses Profils mit ihren Teilsicherungen je Ziel; ältere werden gelöscht.",
                            "After a successful run, the {} newest full backups of this profile with their partial backups are kept per target; older ones are deleted.",
                            job.keep_last
                        )
                    } else {
                        tr!(
                            "Nach einem gelungenen Lauf bleiben die {} neuesten Sicherungen dieses Profils je Ziel; ältere werden gelöscht.",
                            "After a successful run, the {} newest backups of this profile are kept per target; older ones are deleted.",
                            job.keep_last
                        )
                    },
                    theme::regular(12.0),
                    TEXT_MUTED,
                );

                ui.add_space(8.0);
                widgets::divider(ui);
                ui.add_space(6.0);
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    match &last_run {
                        Some(record) => {
                            let state = if record.ok { t!("gelungen", "succeeded") } else { t!("fehlgeschlagen", "failed") };
                            widgets::kv_row(ui, t!("Letzter Lauf", "Last run"), &format!("{} · {state}", record.finished));
                            let color = if record.ok { TEXT_MUTED } else { RED };
                            ui.add(egui::Label::new(RichText::new(&record.text).font(theme::regular(12.0)).color(color)).wrap());
                        }
                        None => widgets::kv_row(ui, t!("Letzter Lauf", "Last run"), t!("noch keiner", "none yet")),
                    }
                    widgets::kv_row(ui, t!("Für Skripte", "For scripts"), &format!("ZEBack.exe --run-job {id}"));
                });

                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    run = widgets::button(ui, t!("Jetzt ausführen", "Run now"), Kind::Primary, Some(Icon::Backup)).clicked();
                    save = widgets::button(ui, t!("Speichern", "Save"), Kind::Secondary, Some(Icon::Check)).clicked();
                    let label = if confirm_delete { t!("Wirklich löschen?", "Really delete?") } else { t!("Löschen", "Delete") };
                    delete = widgets::button(ui, label, Kind::Danger, Some(Icon::Delete)).clicked();
                });
            });
        });
        if run {
            self.job_delete = None;
            self.start_job(index);
        }
        if save {
            self.job_delete = None;
            self.save_job(index);
        }
        if delete {
            if confirm_delete {
                self.delete_job(index);
            } else {
                self.job_delete = Some(id);
            }
        }
    }

    fn card_app_password_help(ui: &mut Ui) {
        widgets::card(ui, |ui| {
            widgets::card_header(
                ui,
                Icon::Key,
                t!("App-Passwort von Hand", "App password by hand"),
                t!("Nur nötig, wenn die Anmeldung im Browser nicht geht", "Only needed if signing in via the browser does not work"),
                None,
            );
            widgets::text(
                ui,
                t!(
                    "„Im Browser anmelden“ erledigt das alles selbst und funktioniert auch mit Zwei-Faktor-Anmeldung. Von Hand geht es so:",
                    "“Sign in via browser” does all of this for you and also works with two-factor authentication. By hand it goes like this:"
                ),
                theme::regular(12.5),
                TEXT_MUTED,
            );
            ui.add_space(4.0);
            for (number, step) in [
                t!(
                    "In Nextcloud oben rechts auf das Profilbild klicken, dann „Einstellungen“.",
                    "In Nextcloud, click your profile picture at the top right, then “Settings”."
                ),
                t!(
                    "Links „Sicherheit“ wählen, zu „Geräte & Sitzungen“ scrollen.",
                    "Choose “Security” on the left and scroll to “Devices & sessions”."
                ),
                t!(
                    "Als Namen „ZEBack“ eintragen, „Neues App-Passwort erstellen“ klicken.",
                    "Enter “ZEBack” as the name and click “Create new app password”."
                ),
                t!(
                    "Das angezeigte Passwort links bei „App-Passwort“ einfügen.",
                    "Paste the password shown into “App password” on the left."
                ),
            ]
            .into_iter()
            .enumerate()
            {
                ui.horizontal_top(|ui| {
                    widgets::chip(ui, &format!("{:02}", number + 1), ACCENT);
                    ui.add(egui::Label::new(RichText::new(step).font(theme::regular(13.0)).color(TEXT)).wrap());
                });
            }
        });
    }

    fn ui_log(&mut self, ui: &mut Ui) {
        let mut clear = false;
        let badge = self.log.len().to_string();
        widgets::card(ui, |ui| {
            ui.horizontal(|ui| {
                widgets::inline_icon(ui, Icon::Log, 16.0, TEXT_MUTED);
                widgets::text(ui, t!("Verlauf", "History"), theme::bold(16.0), TEXT_BRIGHT);
                widgets::chip(ui, &badge, TEXT);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if !self.log.is_empty() {
                        clear = widgets::button(ui, t!("Leeren", "Clear"), Kind::Ghost, Some(Icon::Delete)).clicked();
                    }
                });
            });
            ui.add_space(8.0);
            widgets::divider(ui);
            ui.add_space(6.0);
            if self.log.is_empty() {
                ui.add_space(20.0);
                widgets::text(ui, t!("> noch nichts passiert", "> nothing has happened yet"), theme::regular(13.0), TEXT_MUTED);
                widgets::text(
                    ui,
                    t!(
                        "  Hier erscheint, was bei Sicherungen und Wiederherstellungen geschieht.",
                        "  What happens during backups and restores appears here."
                    ),
                    theme::regular(12.0),
                    TEXT_FAINT,
                );
                ui.add_space(20.0);
            } else {
                ui.spacing_mut().item_spacing.y = 3.0;
                for line in &self.log {
                    ui.horizontal_top(|ui| {
                        let (prefix, color) = if line.warn { ("!", YELLOW) } else { (">", TEXT_FAINT) };
                        widgets::text(ui, prefix, theme::bold(13.0), color);
                        let color = if line.warn { YELLOW } else { TEXT };
                        ui.add(egui::Label::new(RichText::new(&line.text).font(theme::regular(13.0)).color(color)).wrap());
                    });
                }
            }
        });
        if clear {
            self.log.clear();
            self.unseen_warnings = 0;
        }
    }

    // ------------------------------------------------------------------ Aktionsleiste

    fn action_bar_visible(&self) -> bool {
        self.running.is_some() || self.result.is_some() || matches!(self.tab, Tab::Backup | Tab::Restore)
    }

    fn ui_action_bar(&mut self, ui: &mut Ui) {
        let response = egui::Frame::new()
            .inner_margin(Margin::symmetric(26, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width().min(CONTENT_WIDTH));
                if self.running.is_some() {
                    self.ui_running(ui);
                    return;
                }
                let has_banner = self.result.is_some();
                self.ui_banner(ui);
                if has_banner && matches!(self.tab, Tab::Backup | Tab::Restore) {
                    ui.add_space(4.0);
                }
                match self.tab {
                    Tab::Backup => {
                        let hint = match (self.backup_step, self.backup_mode) {
                            (0, BackupMode::Profile) => {
                                self.profiles
                                    .get(self.backup_profile)
                                    .map(|p| tr!("Profil: {}", "Profile: {}", p.name))
                                    .unwrap_or_default()
                            }
                            (0, BackupMode::Drive) => {
                                self.source_drive().map(|d| tr!("Laufwerk: {}", "Drive: {}", d.title())).unwrap_or_default()
                            }
                            (0, BackupMode::Image) => t!("Systemabbild", "System image").to_string(),
                            (1, BackupMode::Profile) => {
                                tr!(
                                    "{} Ordner ausgewählt · {}",
                                    "{} folders selected · {}",
                                    self.selected_source_count(),
                                    self.selected_size_text()
                                )
                            }
                            (1, _) => tr!("Datenmenge: {}", "Amount of data: {}", self.selected_size_text()),
                            (2, BackupMode::Image) => self
                                .image_target()
                                .map(|t| tr!("Ziel: {t}", "Target: {t}"))
                                .unwrap_or_else(|| t!("noch kein Ziel", "no target yet").to_string()),
                            (2, _) => {
                                let names: Vec<&str> =
                                    self.cfg.targets.iter().filter(|t| t.selected).map(|t| t.name.as_str()).collect();
                                match names.len() {
                                    0 => t!("noch kein Ziel", "no target yet").to_string(),
                                    1 | 2 => tr!("Ziel: {}", "Target: {}", names.join(" + ")),
                                    count => tr!("{count} Ziele ausgewählt", "{count} targets selected"),
                                }
                            }
                            _ => t!("Bereit zum Start", "Ready to start").to_string(),
                        };
                        let start_label = if self.backup_mode == BackupMode::Image {
                            t!("Abbild erstellen", "Create image")
                        } else {
                            t!("Backup starten", "Start backup")
                        };
                        match self.ui_wizard_bar(ui, self.backup_step, &hint, start_label, Icon::Backup, true) {
                            Some(true) => self.backup_next(),
                            Some(false) => self.go_to_step(self.backup_step.saturating_sub(1)),
                            None => {}
                        }
                    }
                    Tab::Restore => {
                        let hint = match (&self.archive, self.restore_step) {
                            (None, _) => t!("noch keine Sicherung geöffnet", "no backup opened yet").to_string(),
                            (Some(info), 0) => info.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                            (Some(info), 1) if info.manifest.is_some() => {
                                tr!("{} Ordner ausgewählt", "{} folders selected", self.root_checked.iter().filter(|c| **c).count())
                            }
                            (Some(_), 1) => t!("gesamtes Archiv", "entire archive").to_string(),
                            (Some(_), 2) => match self.restore_mode {
                                RestoreMode::Profile => self
                                    .profiles
                                    .get(self.restore_profile)
                                    .map(|p| tr!("Ziel: Profil {}", "Target: profile {}", p.name))
                                    .unwrap_or_default(),
                                RestoreMode::Folder => t!("Ziel: anderer Ordner", "Target: another folder").to_string(),
                            },
                            _ => t!("Bereit zum Start", "Ready to start").to_string(),
                        };
                        let can_continue = self.archive.is_some();
                        match self.ui_wizard_bar(
                            ui,
                            self.restore_step,
                            &hint,
                            t!("Wiederherstellen", "Restore"),
                            Icon::Restore,
                            can_continue,
                        ) {
                            Some(true) => self.restore_next(),
                            Some(false) => self.go_to_step(self.restore_step.saturating_sub(1)),
                            None => {}
                        }
                    }
                    _ => {}
                }
            })
            .response;
        let line = ui.max_rect().x_range();
        ui.painter().hline(line, response.rect.top(), Stroke::new(1.0, BORDER));
    }

    /// Leiste des geführten Ablaufs: "Zurück" links, Hinweis in der Mitte, "Weiter"/Start rechts.
    /// Rückgabe: `Some(true)` = weiter, `Some(false)` = zurück.
    fn ui_wizard_bar(
        &mut self,
        ui: &mut Ui,
        step: usize,
        hint: &str,
        start_label: &str,
        start_icon: Icon,
        can_continue: bool,
    ) -> Option<bool> {
        let mut action = None;
        ui.horizontal(|ui| {
            if step > 0 && widgets::button(ui, t!("← Zurück", "← Back"), Kind::Secondary, None).clicked() {
                action = Some(false);
            }
            let width = (ui.available_width() - 230.0).max(60.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, 34.0), Sense::hover());
            let galley = widgets::elided(ui, hint, theme::regular(13.0), TEXT_MUTED, width - 8.0);
            ui.painter().galley(pos2(rect.left() + 4.0, rect.center().y - galley.size().y / 2.0), galley, TEXT_MUTED);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(can_continue, |ui| {
                    let clicked = if step >= LAST_STEP {
                        widgets::button(ui, start_label, Kind::Primary, Some(start_icon)).clicked()
                    } else {
                        widgets::button(ui, t!("Weiter →", "Next →"), Kind::Primary, None).clicked()
                    };
                    if clicked {
                        action = Some(true);
                    }
                });
            });
        });
        action
    }

    fn ui_running(&mut self, ui: &mut Ui) {
        let Some(running) = &self.running else { return };
        let cancelling = running.cancel.load(Ordering::Relaxed);
        // Nur eine Prozentangabe (ohne Datenmenge): dann gibt es auch kein Tempo.
        let percent_only = matches!(self.progress, Some((_, 100)) if self.progress_percent);
        let detail = match self.progress {
            Some((done, total)) if percent_only => format!("{} %", done * 100 / total),
            Some((done, total)) if total > 0 => {
                let percent = (done as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
                format!("{percent:.0} %  ·  {} / {}", fmt_bytes(done), fmt_bytes(total))
            }
            // Gesamtmenge unbekannt: wenigstens zeigen, wie viel schon geschrieben ist.
            Some((done, 0)) if done > 0 => tr!("{} bisher", "{} so far", fmt_bytes(done)),
            _ => String::new(),
        };
        // Zweite Zeile: wie lange es schon läuft, wie schnell es geht, wie lange es noch dauert.
        let elapsed = Duration::from_secs_f64((self.now() - self.started_at).max(0.0));
        let mut facts = vec![tr!("Läuft seit {}", "Running for {}", fmt_duration(elapsed))];
        if !percent_only && let Some(speed) = self.rate.per_second() {
            facts.push(tr!("{}/s", "{}/s", fmt_bytes(speed as u64)));
            if let Some(left) = self.progress.and_then(|(_, total)| self.rate.seconds_left(total)) {
                facts.push(tr!("noch etwa {}", "about {} left", fmt_duration(Duration::from_secs_f64(left))));
            } else if speed < 1.0 && self.progress.is_some_and(|(done, total)| done < total) {
                facts.push(t!("steht gerade", "currently stalled").to_string());
            }
        }
        let facts = facts.join("  ·  ");
        ui.horizontal(|ui| {
            widgets::spinner(ui, 18.0, ACCENT);
            let width = (ui.available_width() - 200.0).max(80.0);
            let (rect, _) = ui.allocate_exact_size(vec2(width, 34.0), Sense::hover());
            let status = widgets::elided(ui, &self.status, theme::bold(14.0), TEXT_BRIGHT, width * 0.6);
            let status_width = status.size().x;
            ui.painter().galley(pos2(rect.left(), rect.center().y - status.size().y / 2.0), status, TEXT_BRIGHT);
            let detail = widgets::elided(ui, &detail, theme::regular(13.0), TEXT_MUTED, width - status_width - 20.0);
            ui.painter().galley(
                pos2(rect.left() + status_width + 16.0, rect.center().y - detail.size().y / 2.0),
                detail,
                TEXT_MUTED,
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_enabled_ui(!cancelling, |ui| {
                    let label = if cancelling { t!("Wird abgebrochen", "Cancelling") } else { t!("Abbrechen", "Cancel") };
                    if widgets::button(ui, label, Kind::Danger, Some(Icon::Close)).clicked() {
                        running.cancel.store(true, Ordering::Relaxed);
                    }
                });
            });
        });
        let fraction = match self.progress {
            Some((done, total)) if total > 0 => Some((done as f64 / total as f64).clamp(0.0, 1.0) as f32),
            _ => None,
        };
        widgets::progress_bar(ui, fraction);
        ui.add_space(4.0);
        widgets::text(ui, facts, theme::regular(12.0), TEXT_MUTED);
        ui.add_space(2.0);
    }

    /// Ergebnis-Meldung (Erfolg, Hinweis oder Fehler); gleitet beim Erscheinen herein.
    fn ui_banner(&mut self, ui: &mut Ui) {
        let Some((level, message)) = self.result.clone() else { return };
        let elapsed = ((self.now() - self.result_at) / 0.28).clamp(0.0, 1.0) as f32;
        let appear = theme::ease_out(elapsed);
        if elapsed < 1.0 {
            ui.ctx().request_repaint();
        }
        let (color, icon) = match level {
            Level::Ok => (GREEN, Icon::Check),
            Level::Warn => (YELLOW, Icon::Warning),
            Level::Error => (RED, Icon::Error),
        };
        let mut close = false;
        ui.scope(|ui| {
            ui.multiply_opacity(appear);
            ui.add_space((1.0 - appear) * 10.0);
            let frame = egui::Frame::new()
                .fill(SURFACE)
                .stroke(Stroke::new(1.0, mix(BORDER, color, 0.45)))
                .corner_radius(CornerRadius::same(3))
                .inner_margin(Margin { left: 16, right: 10, top: 10, bottom: 10 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        widgets::inline_icon(ui, icon, 15.0, color);
                        let width = (ui.available_width() - 40.0).max(80.0);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.spacing_mut().item_spacing.y = 2.0;
                            for (index, line) in message.lines().enumerate() {
                                if index == 0 {
                                    widgets::text(ui, line, theme::bold(14.0), TEXT_BRIGHT);
                                } else {
                                    ui.add(egui::Label::new(RichText::new(line).font(theme::regular(13.0)).color(TEXT)).wrap());
                                }
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                            close = widgets::icon_button(ui, Icon::Close, t!("Meldung schließen", "Close message")).clicked();
                        });
                    });
                });
            // Farbiger Streifen am linken Rand
            let rect = frame.response.rect;
            ui.painter().rect_filled(
                Rect::from_min_max(rect.left_top(), pos2(rect.left() + 3.0, rect.bottom())),
                CornerRadius { nw: 3, sw: 3, ne: 0, se: 0 },
                color,
            );
        });
        if close {
            self.result = None;
        }
    }

    // ------------------------------------------------------------------ Dialog: einzelne Dateien auswählen

    fn ui_browse_dialog(&mut self) {
        let (Some(dir), Some(tree)) = (self.browse_dir.clone(), self.tree.clone()) else { return };
        let nodes = tree.children(&dir);
        // Ist der geöffnete Ordner selbst (oder einer darüber) ausgewählt, gilt das für alles darin.
        let inherited = !dir.is_empty() && restore::is_selected(&self.picked, &dir);
        let (files, bytes) = tree.total(&self.picked);
        let (mut close, mut clear, mut up) = (false, false, false);
        let mut open: Option<String> = None;
        let mut toggle: Option<String> = None;
        let modal = egui::Modal::new(egui::Id::unique("browse_archive"))
            .backdrop_color(Color32::from_black_alpha(190))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::ZERO)
                    .inner_margin(Margin::same(20)),
            )
            .show(&self.ctx, |ui| {
                ui.set_width(660.0);
                ui.horizontal(|ui| {
                    widgets::inline_icon(ui, Icon::Folder, 16.0, TEXT_MUTED);
                    widgets::text(ui, t!("Dateien auswählen", "Select files"), theme::bold(16.0), TEXT_BRIGHT);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = widgets::icon_button(ui, Icon::Close, t!("Schließen", "Close")).clicked();
                    });
                });
                widgets::text(
                    ui,
                    t!(
                        "Häkchen wählt aus, ein Klick auf einen Ordner öffnet ihn.",
                        "A tick selects, a click on a folder opens it."
                    ),
                    theme::regular(12.5),
                    TEXT_MUTED,
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add_enabled_ui(!dir.is_empty(), |ui| {
                        up = widgets::button(ui, t!("Nach oben", "Up"), Kind::Secondary, Some(Icon::Home)).clicked();
                    });
                    let location = if dir.is_empty() {
                        t!("Sicherung", "Backup").to_string()
                    } else {
                        tr!("Sicherung / {}", "Backup / {}", dir.replace('/', " / "))
                    };
                    ui.add(egui::Label::new(RichText::new(location).font(theme::regular(13.0)).color(TEXT)).truncate());
                });
                ui.add_space(6.0);
                egui::Frame::new().fill(BG).stroke(Stroke::new(1.0, BORDER)).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    egui::ScrollArea::vertical()
                        .id_salt(&dir)
                        .max_height(374.0)
                        .auto_shrink([false, false])
                        .show_rows(ui, 34.0, nodes.len(), |ui, range| {
                            for node in &nodes[range] {
                                let checked = inherited || self.picked.contains(&node.path);
                                let trailing = if node.dir {
                                    tr!("{} Dateien · {}", "{} files · {}", node.files, fmt_bytes(node.size))
                                } else {
                                    fmt_bytes(node.size)
                                };
                                let (toggled, opened) = ui
                                    .push_id(&node.path, |ui| {
                                        widgets::browse_row(ui, checked, inherited, node.dir, &node.name, &trailing)
                                    })
                                    .inner;
                                if toggled {
                                    toggle = Some(node.path.clone());
                                }
                                if opened {
                                    open = Some(node.path.clone());
                                }
                            }
                        });
                });
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    close |= widgets::button(ui, t!("Übernehmen", "Apply"), Kind::Primary, Some(Icon::Check)).clicked();
                    ui.add_enabled_ui(!self.picked.is_empty(), |ui| {
                        clear = widgets::button(ui, t!("Auswahl leeren", "Clear selection"), Kind::Ghost, Some(Icon::Close)).clicked();
                    });
                    let summary = match self.picked.len() {
                        0 => t!("noch nichts ausgewählt", "nothing selected yet").to_string(),
                        count => tr!(
                            "{count} Einträge · {files} Dateien · {}",
                            "{count} entries · {files} files · {}",
                            fmt_bytes(bytes)
                        ),
                    };
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::text(ui, summary, theme::regular(12.5), TEXT_MUTED);
                    });
                });
            });
        widgets::corner_brackets(&self.ctx.layer_painter(modal.response.layer_id), modal.response.rect, 12.0, BRACKET);
        if let Some(path) = toggle
            && !self.picked.remove(&path)
        {
            // Was unter dem Ordner schon einzeln ausgewählt war, ist jetzt mit abgedeckt.
            let below = format!("{path}/");
            self.picked.retain(|p| !p.starts_with(&below));
            self.picked.insert(path);
        }
        if clear {
            self.picked.clear();
        }
        if let Some(path) = open {
            self.browse_dir = Some(path);
        } else if up {
            self.browse_dir = Some(dir.rsplit_once('/').map(|(parent, _)| parent.to_string()).unwrap_or_default());
        } else if close || modal.should_close() {
            self.browse_dir = None;
        }
    }

    // ------------------------------------------------------------------ Dialog: gefundene Sicherungen

    fn ui_found_dialog(&mut self) {
        let Some(files) = self.found_backups.clone() else { return };
        let mut chosen = None;
        let mut close = false;
        let modal = egui::Modal::new(egui::Id::unique("remote_backups"))
            .backdrop_color(Color32::from_black_alpha(190))
            .frame(
                egui::Frame::new()
                    .fill(SURFACE)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(CornerRadius::same(2))
                    .inner_margin(Margin::same(20)),
            )
            .show(&self.ctx, |ui| {
                ui.set_width(620.0);
                ui.horizontal(|ui| {
                    widgets::inline_icon(ui, Icon::Drive, 16.0, TEXT_MUTED);
                    widgets::text(ui, t!("Sicherungen auf deinen Zielen", "Backups on your targets"), theme::bold(16.0), TEXT_BRIGHT);
                    widgets::chip(ui, &files.len().to_string(), TEXT);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        close = widgets::icon_button(ui, Icon::Close, t!("Schließen", "Close")).clicked();
                    });
                });
                widgets::text(
                    ui,
                    t!(
                        "Neueste zuerst. Sicherungen auf einer Nextcloud werden vor dem Öffnen heruntergeladen.",
                        "Newest first. Backups on a Nextcloud are downloaded before they are opened."
                    ),
                    theme::regular(12.5),
                    TEXT_MUTED,
                );
                ui.add_space(8.0);
                egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                    for (index, file) in files.iter().enumerate() {
                        egui::Frame::new()
                            .fill(BG)
                            .stroke(Stroke::new(1.0, BORDER))
                            .corner_radius(CornerRadius::same(3))
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    widgets::inline_icon(ui, Icon::Zip, 16.0, ACCENT);
                                    let width = (ui.available_width() - 170.0).max(120.0);
                                    ui.vertical(|ui| {
                                        ui.set_width(width);
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.add(egui::Label::new(RichText::new(&file.name).font(theme::bold(14.0)).color(TEXT_BRIGHT)).truncate());
                                        let date = file.modified.map(|d| d.format("%d.%m.%Y, %H:%M").to_string()).unwrap_or_default();
                                        widgets::text(
                                            ui,
                                            format!("{} · {date} · {}", fmt_bytes(file.size), file.target_name),
                                            theme::regular(12.0),
                                            TEXT_MUTED,
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.push_id(index, |ui| {
                                            let (label, icon) = match file.place {
                                                Place::File(_) => (t!("Öffnen", "Open"), Icon::OpenFile),
                                                Place::Cloud(_) => (t!("Laden", "Download"), Icon::Downloads),
                                            };
                                            if widgets::button(ui, label, Kind::Primary, Some(icon)).clicked() {
                                                chosen = Some(file.clone());
                                            }
                                        });
                                    });
                                });
                            });
                    }
                });
            });
        widgets::corner_brackets(
            &self.ctx.layer_painter(modal.response.layer_id),
            modal.response.rect,
            12.0,
            BRACKET,
        );
        match chosen {
            Some(FoundBackup { place: Place::File(path), .. }) => {
                self.found_backups = None;
                self.open_archive(path);
            }
            Some(FoundBackup { place: Place::Cloud(file), target_id, .. }) => self.start_download(target_id, file),
            None if close || modal.should_close() => self.found_backups = None,
            None => {}
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.poll();

        egui::Panel::top("topbar")
            .exact_size(TOPBAR_HEIGHT)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| self.ui_topbar(ui));

        egui::Panel::left("sidebar")
            .exact_size(SIDEBAR_WIDTH)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(BG))
            .show(ui, |ui| self.ui_sidebar(ui));

        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG)).show(ui, |ui| {
            Self::paint_grid(ui, ui.max_rect());

            if self.action_bar_visible() {
                egui::Panel::bottom("actions")
                    .show_separator_line(false)
                    .frame(egui::Frame::new().fill(BG))
                    .show(ui, |ui| self.ui_action_bar(ui));
            }

            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| self.ui_page(ui));
        });

        self.ui_found_dialog();
        self.ui_browse_dialog();
    }

    fn on_exit(&mut self) {
        self.persist();
    }
}
