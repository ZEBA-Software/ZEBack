//! Benutzerprofile dieses PCs (Ordner unter C:\Users) und ihre Standardordner.

use std::fs;
use std::path::{Path, PathBuf};

/// Standardordner: Schlüssel, Anzeigename (deutsch und englisch) und mögliche Ordnernamen auf
/// der Festplatte.
const KINDS: [(&str, &str, &str, &[&str]); 6] = [
    ("desktop", "Desktop", "Desktop", &["Desktop"]),
    ("documents", "Dokumente", "Documents", &["Documents", "Dokumente"]),
    ("downloads", "Downloads", "Downloads", &["Downloads"]),
    ("pictures", "Bilder", "Pictures", &["Pictures", "Bilder"]),
    ("music", "Musik", "Music", &["Music", "Musik"]),
    ("videos", "Videos", "Videos", &["Videos"]),
];

/// Ordner unter C:\Users, die keine Benutzerprofile sind.
const NOT_A_PROFILE: [&str; 5] = ["public", "default", "default user", "all users", "defaultapppool"];

#[derive(Clone, Debug)]
pub struct StdFolder {
    pub key: &'static str,
    /// Deutscher Name; zugleich der Ordnername in den Sicherungen und daher unabhängig von der Sprache.
    pub label: &'static str,
    /// Englischer Name, nur für die Anzeige
    pub label_en: &'static str,
    pub path: PathBuf,
}

impl StdFolder {
    /// Name in der eingestellten Sprache der Oberfläche.
    pub fn title(&self) -> &'static str {
        t!(self.label, self.label_en)
    }
}

/// Gruppe, unter der ein Zusatzordner in der Oberfläche erscheint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Profile,
    Browser,
    Mail,
    Other,
}

impl Group {
    pub fn label(self) -> &'static str {
        match self {
            Group::Profile => t!("Profil", "Profile"),
            Group::Browser => "Browser",
            Group::Mail => t!("Mail und Office", "Mail and Office"),
            Group::Other => t!("Sonstiges", "Other"),
        }
    }
}

/// Wo ein Zusatzordner liegt.
#[derive(Clone, Copy)]
enum Base {
    /// direkt im Profilordner
    Profile,
    /// AppData\Roaming
    Roaming,
    /// AppData\Local
    Local,
    /// C:\Users\Public (für alle Benutzer)
    Public,
}

struct OptionalDef {
    key: &'static str,
    label: &'static str,
    label_en: &'static str,
    hint: &'static str,
    hint_en: &'static str,
    group: Group,
    base: Base,
    /// Mögliche Pfade relativ zur Basis; der erste vorhandene zählt.
    paths: &'static [&'static str],
    /// Unterordner (Namen), die nicht mitgesichert werden – vor allem Zwischenspeicher.
    exclude: &'static [&'static str],
}

/// Zwischenspeicher von Chrome und Edge: groß, ständig in Benutzung und nach dem
/// Wiederherstellen wertlos.
const CHROMIUM_CACHES: &[&str] = &[
    "cache",
    "code cache",
    "gpucache",
    "shadercache",
    "grshadercache",
    "graphitedawncache",
    "dawncache",
    "dawngraphitecache",
    "dawnwebgpucache",
    "cachestorage",
    "scriptcache",
    "component_crx_cache",
    "extensions_crx_cache",
    "crashpad",
    "browsermetrics",
    "optimization_guide_model_store",
];

/// Zusätzliche Ordner, die sich oft zu sichern lohnen. Angeboten wird nur, was vorhanden ist.
const OPTIONAL: &[OptionalDef] = &[
    OptionalDef { key: "favorites", label: "Favoriten", label_en: "Favorites", hint: "Gespeicherte Links", hint_en: "Saved links", group: Group::Profile, base: Base::Profile, paths: &["Favorites"], exclude: &[] },
    OptionalDef { key: "contacts", label: "Kontakte", label_en: "Contacts", hint: "Windows-Kontakte", hint_en: "Windows contacts", group: Group::Profile, base: Base::Profile, paths: &["Contacts"], exclude: &[] },
    OptionalDef { key: "links", label: "Links", label_en: "Links", hint: "Schnellzugriff im Explorer", hint_en: "Quick access in File Explorer", group: Group::Profile, base: Base::Profile, paths: &["Links"], exclude: &[] },
    OptionalDef { key: "saved_games", label: "Gespeicherte Spiele", label_en: "Saved Games", hint: "Spielstände", hint_en: "Game saves", group: Group::Profile, base: Base::Profile, paths: &["Saved Games"], exclude: &[] },
    OptionalDef { key: "firefox", label: "Firefox", label_en: "Firefox", hint: "Lesezeichen, Passwörter, Einstellungen", hint_en: "Bookmarks, passwords, settings", group: Group::Browser, base: Base::Roaming, paths: &["Mozilla\\Firefox"], exclude: &[] },
    OptionalDef { key: "chrome", label: "Chrome", label_en: "Chrome", hint: "Lesezeichen, Verlauf, Erweiterungen", hint_en: "Bookmarks, history, extensions", group: Group::Browser, base: Base::Local, paths: &["Google\\Chrome\\User Data"], exclude: CHROMIUM_CACHES },
    OptionalDef { key: "edge", label: "Edge", label_en: "Edge", hint: "Lesezeichen, Verlauf, Erweiterungen", hint_en: "Bookmarks, history, extensions", group: Group::Browser, base: Base::Local, paths: &["Microsoft\\Edge\\User Data"], exclude: CHROMIUM_CACHES },
    OptionalDef { key: "thunderbird", label: "Thunderbird", label_en: "Thunderbird", hint: "Mails, Konten, Adressbuch", hint_en: "Mail, accounts, address book", group: Group::Mail, base: Base::Roaming, paths: &["Thunderbird"], exclude: &[] },
    OptionalDef { key: "outlook_signatures", label: "Outlook-Signaturen", label_en: "Outlook signatures", hint: "Signaturen für E-Mails", hint_en: "Email signatures", group: Group::Mail, base: Base::Roaming, paths: &["Microsoft\\Signatures", "Microsoft\\Signaturen"], exclude: &[] },
    OptionalDef { key: "office_templates", label: "Office-Vorlagen", label_en: "Office templates", hint: "Eigene Vorlagen (z. B. Normal.dotm)", hint_en: "Custom templates (e.g. Normal.dotm)", group: Group::Mail, base: Base::Roaming, paths: &["Microsoft\\Templates", "Microsoft\\Vorlagen"], exclude: &[] },
    OptionalDef { key: "office_dictionary", label: "Office-Wörterbuch", label_en: "Office dictionary", hint: "Eigene Wörter der Rechtschreibprüfung", hint_en: "Custom spell-check words", group: Group::Mail, base: Base::Roaming, paths: &["Microsoft\\UProof"], exclude: &[] },
    OptionalDef { key: "sticky_notes", label: "Kurznotizen", label_en: "Sticky Notes", hint: "Sticky Notes", hint_en: "Notes from the Sticky Notes app", group: Group::Other, base: Base::Local, paths: &["Packages\\Microsoft.MicrosoftStickyNotes_8wekyb3d8bbwe\\LocalState"], exclude: &[] },
    OptionalDef { key: "ssh", label: "SSH-Schlüssel", label_en: "SSH keys", hint: "Ordner .ssh", hint_en: ".ssh folder", group: Group::Other, base: Base::Profile, paths: &[".ssh"], exclude: &[] },
    OptionalDef { key: "vscode", label: "VS Code", label_en: "VS Code", hint: "Einstellungen und Tastenkürzel", hint_en: "Settings and keyboard shortcuts", group: Group::Other, base: Base::Roaming, paths: &["Code\\User"], exclude: &["workspacestorage", "history"] },
    OptionalDef { key: "public_documents", label: "Öffentliche Dokumente", label_en: "Public Documents", hint: "Für alle Benutzer dieses PCs", hint_en: "For all users of this PC", group: Group::Other, base: Base::Public, paths: &["Documents"], exclude: &[] },
    OptionalDef { key: "public_desktop", label: "Öffentlicher Desktop", label_en: "Public Desktop", hint: "Für alle Benutzer dieses PCs", hint_en: "For all users of this PC", group: Group::Other, base: Base::Public, paths: &["Desktop"], exclude: &[] },
];

#[derive(Clone, Debug)]
pub struct OptionalFolder {
    pub key: &'static str,
    /// Deutscher Name; zugleich der Ordnername in den Sicherungen und daher unabhängig von der Sprache.
    pub label: &'static str,
    /// Englischer Name, nur für die Anzeige
    pub label_en: &'static str,
    pub hint: &'static str,
    pub hint_en: &'static str,
    pub group: Group,
    pub path: PathBuf,
    pub exclude: &'static [&'static str],
}

impl OptionalFolder {
    /// Name in der eingestellten Sprache der Oberfläche.
    pub fn title(&self) -> &'static str {
        t!(self.label, self.label_en)
    }

    /// Kurze Erklärung in der eingestellten Sprache der Oberfläche.
    pub fn hint_text(&self) -> &'static str {
        t!(self.hint, self.hint_en)
    }
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub name: String,
    pub dir: PathBuf,
    /// Profil des gerade angemeldeten Benutzers
    pub current: bool,
    /// `false`, wenn der Ordner ohne Administratorrechte nicht lesbar ist
    pub accessible: bool,
    /// Vorhandene Standardordner (Desktop, Dokumente, …)
    pub folders: Vec<StdFolder>,
    /// Vorhandene Zusatzordner (Browser, Mail, Office, …)
    pub optional: Vec<OptionalFolder>,
}

impl Profile {
    /// Zielordner für eine Wiederherstellung: der vorhandene Standardordner oder,
    /// falls es ihn noch nicht gibt, sein üblicher Platz im Profil.
    pub fn target_for(&self, key: &str) -> Option<PathBuf> {
        if let Some(folder) = self.folders.iter().find(|f| f.key == key) {
            return Some(folder.path.clone());
        }
        KINDS.iter().find(|(k, _, _, _)| *k == key).map(|(_, _, _, names)| self.dir.join(names[0]))
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().trim_end_matches('\\').eq_ignore_ascii_case(b.to_string_lossy().trim_end_matches('\\'))
}

#[cfg(windows)]
fn is_hidden(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const HIDDEN: u32 = 0x2;
    meta.file_attributes() & HIDDEN != 0
}

#[cfg(not(windows))]
fn is_hidden(_meta: &fs::Metadata) -> bool {
    false
}

/// Standardordner des angemeldeten Benutzers. Die Pfade kommen von Windows (Known Folders)
/// und stimmen daher auch bei Umleitung, z. B. nach OneDrive.
fn current_user_folders() -> Vec<StdFolder> {
    let paths = [
        dirs::desktop_dir(),
        dirs::document_dir(),
        dirs::download_dir(),
        dirs::picture_dir(),
        dirs::audio_dir(),
        dirs::video_dir(),
    ];
    KINDS
        .iter()
        .zip(paths)
        .filter_map(|((key, label, label_en, _), path)| Some(StdFolder { key, label, label_en, path: path? }))
        .collect()
}

/// Standardordner eines anderen Profils: zuerst direkt im Profil suchen, dann in OneDrive-Ordnern.
fn other_user_folders(dir: &Path) -> Vec<StdFolder> {
    let onedrive_dirs: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("OneDrive"))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    KINDS
        .iter()
        .filter_map(|(key, label, label_en, names)| {
            let direct = names.iter().map(|name| dir.join(name));
            let redirected = onedrive_dirs.iter().flat_map(|od| names.iter().map(move |name| od.join(name)));
            let path = direct.chain(redirected).find(|p| p.is_dir())?;
            Some(StdFolder { key, label, label_en, path })
        })
        .collect()
}

/// Vorhandene Zusatzordner eines Profils. Beim angemeldeten Benutzer kommen die AppData-Pfade
/// von Windows (sie können umgeleitet sein), sonst gilt der übliche Platz im Profil.
fn optional_folders(dir: &Path, users_dir: &Path, current: bool) -> Vec<OptionalFolder> {
    let roaming = current.then(dirs::config_dir).flatten().unwrap_or_else(|| dir.join("AppData").join("Roaming"));
    let local = current.then(dirs::data_local_dir).flatten().unwrap_or_else(|| dir.join("AppData").join("Local"));
    let public = users_dir.join("Public");
    OPTIONAL
        .iter()
        .filter_map(|def| {
            let base = match def.base {
                Base::Profile => dir,
                Base::Roaming => roaming.as_path(),
                Base::Local => local.as_path(),
                Base::Public => public.as_path(),
            };
            let path = def.paths.iter().map(|p| base.join(p)).find(|p| p.is_dir())?;
            Some(OptionalFolder {
                key: def.key,
                label: def.label,
                label_en: def.label_en,
                hint: def.hint,
                hint_en: def.hint_en,
                group: def.group,
                path,
                exclude: def.exclude,
            })
        })
        .collect()
}

/// Alle Benutzerprofile dieses PCs; das Profil des angemeldeten Benutzers steht vorn.
pub fn list() -> Vec<Profile> {
    let home = dirs::home_dir();
    let users_dir = home.as_deref().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_else(|| {
        PathBuf::from(format!("{}\\Users", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())))
    });

    let mut profiles = Vec::new();
    if let Ok(entries) = fs::read_dir(&users_dir) {
        for entry in entries.flatten() {
            // `metadata()` folgt Verknüpfungen nicht: "All Users" und "Default User" fallen so heraus.
            let Ok(meta) = entry.metadata() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !meta.is_dir() || is_hidden(&meta) || NOT_A_PROFILE.contains(&name.to_lowercase().as_str()) {
                continue;
            }
            let dir = entry.path();
            let current = home.as_deref().is_some_and(|h| same_path(h, &dir));
            let accessible = current || fs::read_dir(&dir).is_ok();
            let folders = if current {
                current_user_folders()
            } else if accessible {
                other_user_folders(&dir)
            } else {
                Vec::new()
            };
            let optional = if accessible { optional_folders(&dir, &users_dir, current) } else { Vec::new() };
            profiles.push(Profile { name, dir, current, accessible, folders, optional });
        }
    }
    if !profiles.iter().any(|p| p.current)
        && let Some(home) = home
    {
        let name = home.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Benutzer".into());
        let optional = optional_folders(&home, &users_dir, true);
        profiles.push(Profile {
            name,
            dir: home,
            current: true,
            accessible: true,
            folders: current_user_folders(),
            optional,
        });
    }
    profiles.sort_by(|a, b| b.current.cmp(&a.current).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    profiles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_current_profile_first() {
        let profiles = list();
        assert!(!profiles.is_empty());
        assert!(profiles[0].current && profiles[0].accessible);
        assert_eq!(profiles.iter().filter(|p| p.current).count(), 1);
        assert!(!profiles.iter().any(|p| NOT_A_PROFILE.contains(&p.name.to_lowercase().as_str())));
    }

    #[test]
    fn finds_folders_of_other_profiles_including_onedrive() {
        let base = std::env::temp_dir().join(format!("packup-test-profiles-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("Desktop")).unwrap();
        fs::create_dir_all(base.join("OneDrive - Firma").join("Dokumente")).unwrap();

        let folders = other_user_folders(&base);
        let keys: Vec<&str> = folders.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["desktop", "documents"]);
        assert_eq!(folders[1].path, base.join("OneDrive - Firma").join("Dokumente"));

        // Fehlende Ordner bekommen bei der Wiederherstellung ihren üblichen Platz.
        let profile = Profile {
            name: "test".into(),
            dir: base.clone(),
            current: false,
            accessible: true,
            folders,
            optional: Vec::new(),
        };
        assert_eq!(profile.target_for("pictures"), Some(base.join("Pictures")));
        assert_eq!(profile.target_for("desktop"), Some(base.join("Desktop")));
        assert_eq!(profile.target_for("unbekannt"), None);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn offers_only_existing_optional_folders() {
        let users = std::env::temp_dir().join(format!("packup-test-optional-{}", std::process::id()));
        let _ = fs::remove_dir_all(&users);
        let dir = users.join("anna");
        fs::create_dir_all(dir.join("Favorites")).unwrap();
        fs::create_dir_all(dir.join("AppData").join("Local").join("Google").join("Chrome").join("User Data")).unwrap();
        // Deutsches Office legt Signaturen unter "Signaturen" ab.
        fs::create_dir_all(dir.join("AppData").join("Roaming").join("Microsoft").join("Signaturen")).unwrap();
        fs::create_dir_all(users.join("Public").join("Documents")).unwrap();

        let found = optional_folders(&dir, &users, false);
        let keys: Vec<&str> = found.iter().map(|f| f.key).collect();
        assert_eq!(keys, ["favorites", "chrome", "outlook_signatures", "public_documents"]);
        let chrome = &found[1];
        assert_eq!(chrome.group, Group::Browser);
        assert!(chrome.exclude.contains(&"cache"));
        assert!(found[2].path.ends_with("Signaturen"));
        let _ = fs::remove_dir_all(users);
    }
}
