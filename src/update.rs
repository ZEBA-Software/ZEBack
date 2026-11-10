//! Updates über GitHub: fragt nach der neuesten Veröffentlichung, lädt auf Wunsch den
//! passenden Installer herunter, prüft ihn und startet die Installation.
//!
//! Erwartet werden Veröffentlichungen (Releases) im Repository mit einer Marke wie `v1.2.3`
//! und den Installern aus `installer\build-msi.ps1` als Anhang:
//! `ZEBack-1.2.3-x64.msi` und `ZEBack-1.2.3-arm64.msi`.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

use crate::job::{Counter, Reporter};
use crate::util::TempFile;

/// Das Repository, aus dem Updates kommen (die Adressen darunter müssen dazu passen).
#[cfg(test)]
const REPOSITORY: &str = "ZEBA-Software/ZEBack";
const API_URL: &str = "https://api.github.com/repos/ZEBA-Software/ZEBack/releases/latest";
/// Seite mit der neuesten Veröffentlichung (falls kein passender Installer dabei ist).
pub const RELEASES_URL: &str = "https://github.com/ZEBA-Software/ZEBack/releases/latest";
/// Nur Adressen unter diesem Anfang werden angenommen – alles andere wäre nicht unser Repository.
const TRUSTED_PREFIX: &str = "https://github.com/ZEBA-Software/ZEBack/";

/// Architektur, für die dieses Programm gebaut ist – so heißt sie im Namen des Installers.
const ARCH: &str = if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" };

/// Der Installer einer Veröffentlichung für diesen PC.
#[derive(Clone, Debug, PartialEq)]
pub struct Installer {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// SHA-256 der Datei, wie GitHub sie angibt (Hex, klein); `None`, wenn die Angabe fehlt
    pub sha256: Option<String>,
}

/// Eine neuere Version, die bereitsteht.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// Seite der Veröffentlichung
    pub url: String,
    /// Kurze Beschreibung der Neuerungen (kann leer sein)
    pub notes: String,
    /// Fehlt, wenn die Veröffentlichung keinen Installer für diese Architektur enthält
    pub installer: Option<Installer>,
}

/// Zerlegt eine Versionsnummer wie `1.12.0` in Zahlen; alles ab einem Zusatz wie `-beta` zählt nicht.
fn parts(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+', ' '])
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// Ist `candidate` neuer als `current`? `0.10.0` ist neuer als `0.9.1`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (mut a, mut b) = (parts(candidate), parts(current));
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a > b
}

fn trusted(url: &str) -> bool {
    url.starts_with(TRUSTED_PREFIX) && !url.chars().any(|c| c.is_control() || c == ' ' || c == '"')
}

/// Liest die Antwort von GitHub zur neuesten Veröffentlichung; `None`, wenn sie nicht neuer
/// ist als `current`. `arch` wählt den Installer aus.
fn parse(text: &str, current: &str, arch: &str) -> Result<Option<Release>> {
    let json: serde_json::Value = serde_json::from_str(text)
        .context(t!("Die Antwort von GitHub ist nicht lesbar", "The answer from GitHub is not readable"))?;
    let version = json["tag_name"]
        .as_str()
        .map(|tag| tag.trim().trim_start_matches(['v', 'V']).to_string())
        // Nur Ziffern und Punkte: Die Angabe wird angezeigt und landet in einem Dateinamen.
        .filter(|v| !v.is_empty() && v.len() <= 20 && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .context(t!("Die Veröffentlichung auf GitHub hat keine Versionsnummer", "The release on GitHub has no version number"))?;
    if !is_newer(&version, current) {
        return Ok(None);
    }
    let url = json["html_url"].as_str().filter(|url| trusted(url)).unwrap_or(RELEASES_URL).to_string();
    let notes: String = json["body"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(400)
        .collect::<String>()
        .trim()
        .to_string();
    // Der Installer muss genau so heißen, wie das Bauskript ihn nennt, und aus unserem Repository kommen.
    let wanted = format!("ZEBack-{version}-{arch}.msi");
    let installer = json["assets"].as_array().into_iter().flatten().find_map(|asset| {
        let name = asset["name"].as_str()?;
        let url = asset["browser_download_url"].as_str()?;
        let expected_start = format!("{TRUSTED_PREFIX}releases/download/");
        if name != wanted || !trusted(url) || !url.starts_with(&expected_start) {
            return None;
        }
        let sha256 = asset["digest"]
            .as_str()
            .and_then(|digest| digest.strip_prefix("sha256:"))
            .filter(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
            .map(str::to_ascii_lowercase);
        Some(Installer { name: name.to_string(), url: url.to_string(), size: asset["size"].as_u64()?, sha256 })
    });
    Ok(Some(Release { version, url, notes, installer }))
}

fn agent() -> Agent {
    Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build())
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .user_agent(concat!("ZEBack/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Fragt unter `url` nach und vergleicht mit `current`.
fn check_at(url: &str, current: &str, arch: &str) -> Result<Option<Release>> {
    let mut response = agent()
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!(tr!("GitHub ist nicht erreichbar ({e})", "GitHub is not reachable ({e})")))?;
    match response.status().as_u16() {
        200 => {}
        // Noch keine Veröffentlichung im Repository: also auch kein Update.
        404 => return Ok(None),
        403 | 429 => bail!(t!(
            "GitHub lässt gerade keine weiteren Abfragen zu. Bitte später noch einmal versuchen.",
            "GitHub is not accepting more requests right now. Please try again later."
        )),
        status => bail!(tr!("Die Update-Abfrage ist fehlgeschlagen (HTTP {status})", "The update check failed (HTTP {status})")),
    }
    let text = response
        .body_mut()
        .with_config()
        .limit(2 * 1024 * 1024)
        .read_to_string()
        .context(t!("Die Antwort von GitHub ist nicht lesbar", "The answer from GitHub is not readable"))?;
    parse(&text, current, arch)
}

/// Fragt bei GitHub nach einer neueren Version als der laufenden.
pub fn check() -> Result<Option<Release>> {
    check_at(API_URL, env!("CARGO_PKG_VERSION"), ARCH)
}

/// Lädt den Installer in den Ordner `dir` und prüft Größe und Prüfsumme. Gibt den Pfad der
/// fertigen Datei zurück.
fn download_to(installer: &Installer, dir: &Path, rep: &Reporter) -> Result<PathBuf> {
    fs::create_dir_all(dir).context(t!("Ordner für das Update konnte nicht angelegt werden", "Could not create the folder for the update"))?;
    let target = dir.join(&installer.name);
    let mut part_name = target.as_os_str().to_owned();
    part_name.push(".part");
    let mut part = TempFile::new(part_name);

    rep.status(tr!("Lade {} herunter …", "Downloading {} …", installer.name));
    let response = agent()
        .get(&installer.url)
        .call()
        .map_err(|e| anyhow!(tr!("Der Download von GitHub ist fehlgeschlagen ({e})", "The download from GitHub failed ({e})")))?;
    let status = response.status().as_u16();
    if status != 200 {
        bail!(tr!("Der Download von GitHub ist fehlgeschlagen (HTTP {status})", "The download from GitHub failed (HTTP {status})"));
    }
    let mut out = BufWriter::with_capacity(1 << 20, File::create(part.path())?);
    let mut hasher = Sha256::new();
    let mut counter = Counter::new(rep, installer.size);
    let mut reader = response.into_body().into_reader();
    let mut buf = vec![0u8; 1 << 20];
    let mut received = 0u64;
    loop {
        rep.check()?;
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow!(e).context(t!("Download abgebrochen – Verbindung verloren", "Download interrupted – connection lost"))),
        };
        received += n as u64;
        // Mehr als angekündigt ist nie richtig; so läuft auch keine Platte voll.
        if received > installer.size {
            bail!(t!("Die heruntergeladene Datei ist größer als angegeben.", "The downloaded file is larger than announced."));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n])?;
        counter.add(n as u64);
    }
    out.flush()?;
    drop(out);
    counter.flush();

    if received != installer.size {
        bail!(tr!(
            "Die heruntergeladene Datei ist unvollständig ({received} von {} Bytes).",
            "The downloaded file is incomplete ({received} of {} bytes).",
            installer.size
        ));
    }
    let actual: String = hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    match &installer.sha256 {
        Some(expected) if *expected != actual => bail!(t!(
            "Die Prüfsumme des Installers stimmt nicht mit der Angabe von GitHub überein. Er wird nicht gestartet.",
            "The checksum of the installer does not match the one given by GitHub. It will not be started."
        )),
        Some(_) => rep.log(t!("Prüfsumme des Installers stimmt.", "The installer's checksum matches.").to_string()),
        None => rep.warn(t!(
            "GitHub nennt für den Installer keine Prüfsumme; geprüft wurde nur die Größe.",
            "GitHub gives no checksum for the installer; only its size was checked."
        )),
    }
    fs::rename(part.path(), &target)?;
    part.keep();
    Ok(target)
}

/// Lädt den Installer der Veröffentlichung in den Temp-Ordner und prüft ihn.
pub fn download(release: &Release, rep: &Reporter) -> Result<PathBuf> {
    let installer = release.installer.as_ref().context(t!(
        "Diese Veröffentlichung enthält keinen Installer für diesen PC.",
        "This release contains no installer for this PC."
    ))?;
    download_to(installer, &std::env::temp_dir().join("ZEBack-Update"), rep)
}

/// Befehlszeile für `cmd.exe`: Installer starten, auf sein Ende warten und danach das frisch
/// installierte Programm öffnen.
fn install_command(msi: &Path, program: &Path) -> String {
    // "/passive": Fortschritt zeigen, aber nichts fragen (Windows fragt selbst nach Adminrechten).
    // "start /wait": sicher auf das Ende des Installers warten; nur nach Erfolg das Programm öffnen.
    format!(
        "/D /C \"start \"\" /wait msiexec.exe /i \"{}\" /passive /norestart && start \"\" \"{}\"\"",
        msi.display(),
        program.display()
    )
}

/// Startet die Installation. Das laufende Programm muss sich danach sofort beenden, damit der
/// Installer seine Datei ersetzen kann.
#[cfg(windows)]
pub fn install(msi: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Anführungszeichen in Pfaden würden die Befehlszeile aufbrechen (unter Windows nicht erlaubt, trotzdem prüfen).
    if !msi.is_file() || msi.display().to_string().contains(['"', '%']) {
        bail!(t!("Der Installer wurde nicht gefunden.", "The installer was not found."));
    }
    let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32");
    let programs = std::env::var_os("ProgramW6432").or_else(|| std::env::var_os("ProgramFiles")).unwrap_or_else(|| "C:\\Program Files".into());
    let program = PathBuf::from(programs).join("ZEBack").join("ZEBack.exe");
    std::process::Command::new(system.join("cmd.exe"))
        .raw_arg(install_command(msi, &program))
        .current_dir(&system)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context(t!("Der Installer ließ sich nicht starten.", "The installer could not be started."))?;
    Ok(())
}

#[cfg(not(windows))]
pub fn install(_msi: &Path) -> Result<()> {
    bail!("Updates werden nur unter Windows installiert")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    const ANSWER: &str = r#"{
        "tag_name": "v2.0.0",
        "html_url": "https://github.com/ZEBA-Software/ZEBack/releases/tag/v2.0.0",
        "body": "Neu:\r\n- alles",
        "assets": [
            {"name": "ZEBack-2.0.0-x64.msi", "size": 6000000, "digest": "sha256:AB12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12",
             "browser_download_url": "https://github.com/ZEBA-Software/ZEBack/releases/download/v2.0.0/ZEBack-2.0.0-x64.msi"},
            {"name": "ZEBack-2.0.0-arm64.msi", "size": 5500000,
             "browser_download_url": "https://github.com/ZEBA-Software/ZEBack/releases/download/v2.0.0/ZEBack-2.0.0-arm64.msi"},
            {"name": "ZEBack-2.0.0-x64.exe", "size": 1, "browser_download_url": "https://github.com/ZEBA-Software/ZEBack/releases/download/v2.0.0/ZEBack-2.0.0-x64.exe"}
        ]
    }"#;

    #[test]
    fn compares_versions_by_number() {
        assert!(is_newer("0.10.0", "0.9.1"));
        assert!(is_newer("1.0", "0.99.99"));
        assert!(is_newer("v1.2.1", "1.2"));
        assert!(!is_newer("1.2.0", "1.2"));
        assert!(!is_newer("0.12.0", "0.12.0"));
        assert!(!is_newer("0.11.9", "0.12.0"));
        assert!(!is_newer("unsinn", "0.1.0"));
    }

    #[test]
    fn reads_a_github_release_and_picks_the_installer_for_this_pc() {
        let release = parse(ANSWER, "1.9.0", "x64").unwrap().unwrap();
        assert_eq!(release.version, "2.0.0");
        assert_eq!(release.url, "https://github.com/ZEBA-Software/ZEBack/releases/tag/v2.0.0");
        assert_eq!(release.notes, "Neu:  - alles");
        let installer = release.installer.unwrap();
        assert_eq!((installer.name.as_str(), installer.size), ("ZEBack-2.0.0-x64.msi", 6_000_000));
        assert_eq!(installer.sha256.as_deref(), Some("ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12"));
        // Andere Architektur: anderer Installer, hier ohne Prüfsumme.
        let arm = parse(ANSWER, "1.9.0", "arm64").unwrap().unwrap().installer.unwrap();
        assert_eq!((arm.name.as_str(), arm.sha256), ("ZEBack-2.0.0-arm64.msi", None));
        // Gleich alt oder älter: kein Hinweis.
        assert_eq!(parse(ANSWER, "2.0.0", "x64").unwrap(), None);
    }

    #[test]
    fn refuses_installers_from_elsewhere_and_odd_versions() {
        // Der Installer liegt nicht in unserem Repository: Hinweis ja, Installation nein.
        for url in [
            "https://github.com/jemand-anders/ZEBack/releases/download/v9.0/ZEBack-9.0-x64.msi",
            "https://example.com/ZEBack-9.0-x64.msi",
            "http://github.com/ZEBA-Software/ZEBack/releases/download/v9.0/ZEBack-9.0-x64.msi",
            "https://github.com/ZEBA-Software/ZEBack/archive/ZEBack-9.0-x64.msi",
        ] {
            let text = format!(r#"{{"tag_name":"v9.0","assets":[{{"name":"ZEBack-9.0-x64.msi","size":5,"browser_download_url":"{url}"}}]}}"#);
            let release = parse(&text, "1.0", "x64").unwrap().unwrap();
            assert_eq!((release.installer, release.url.as_str()), (None, RELEASES_URL), "{url}");
        }
        // Falscher Dateiname (z. B. eine EXE oder eine andere Version) wird nicht genommen.
        let text = r#"{"tag_name":"v9.0","assets":[{"name":"setup.exe","size":5,"browser_download_url":"https://github.com/ZEBA-Software/ZEBack/releases/download/v9.0/setup.exe"}]}"#;
        assert_eq!(parse(text, "1.0", "x64").unwrap().unwrap().installer, None);
        // Keine oder eine merkwürdige Versionsnummer ist ein Fehler, kein Hinweis.
        assert!(parse(r#"{"message":"Not Found"}"#, "1.0", "x64").is_err());
        assert!(parse(r#"{"tag_name":"2.0 & calc"}"#, "1.0", "x64").is_err());
        assert!(parse("<html>", "1.0", "x64").is_err());
    }

    /// Kleiner Webserver: liefert unter `/latest` die Beschreibung einer Veröffentlichung und
    /// unter `/datei` den Inhalt des "Installers".
    fn start_server(content: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let (status, body): (u16, Vec<u8>) = if request.contains("/latest") {
                    (200, ANSWER.as_bytes().to_vec())
                } else if request.contains("/datei") {
                    (200, content.to_vec())
                } else {
                    (404, b"{}".to_vec())
                };
                let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    #[test]
    fn asks_a_server_and_treats_no_release_as_no_update() {
        let base = start_server(b"");
        let found = check_at(&format!("{base}/latest"), "1.0.0", "x64").unwrap().unwrap();
        assert_eq!(found.version, "2.0.0");
        assert_eq!(check_at(&format!("{base}/latest"), "2.0.0", "x64").unwrap(), None);
        // 404 heißt bei GitHub: Es gibt noch keine Veröffentlichung.
        assert_eq!(check_at(&format!("{base}/gibt-es-nicht"), "1.0.0", "x64").unwrap(), None);
    }

    #[test]
    fn downloads_and_checks_the_installer() {
        const CONTENT: &[u8] = b"Das ist kein echter Installer, aber er hat eine Pruefsumme.";
        let base = start_server(CONTENT);
        let dir = std::env::temp_dir().join(format!("zeback-test-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let checksum: String = Sha256::digest(CONTENT).iter().map(|b| format!("{b:02x}")).collect();
        let installer = Installer {
            name: "ZEBack-2.0.0-x64.msi".into(),
            url: format!("{base}/datei"),
            size: CONTENT.len() as u64,
            sha256: Some(checksum),
        };

        let path = download_to(&installer, &dir, &rep).unwrap();
        assert_eq!(fs::read(&path).unwrap(), CONTENT);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_file(&path).unwrap();

        // Falsche Prüfsumme oder falsche Größe: Die Datei wird verworfen, nichts bleibt liegen.
        let tampered = Installer { sha256: Some("0".repeat(64)), ..installer.clone() };
        assert!(download_to(&tampered, &dir, &rep).err().unwrap().to_string().contains("Prüfsumme"));
        let longer = Installer { size: installer.size + 5, ..installer.clone() };
        assert!(download_to(&longer, &dir, &rep).err().unwrap().to_string().contains("unvollständig"));
        let shorter = Installer { size: 10, ..installer };
        assert!(download_to(&shorter, &dir, &rep).err().unwrap().to_string().contains("größer"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn builds_the_installer_command_line() {
        let command = install_command(Path::new("C:\\Temp\\ZEBack Update\\ZEBack-2.0.0-x64.msi"), Path::new("C:\\Program Files\\ZEBack\\ZEBack.exe"));
        assert_eq!(
            command,
            "/D /C \"start \"\" /wait msiexec.exe /i \"C:\\Temp\\ZEBack Update\\ZEBack-2.0.0-x64.msi\" /passive /norestart && start \"\" \"C:\\Program Files\\ZEBack\\ZEBack.exe\"\""
        );
        assert_eq!(REPOSITORY, "ZEBA-Software/ZEBack");
        assert!(API_URL.contains(REPOSITORY) && RELEASES_URL.contains(REPOSITORY) && TRUSTED_PREFIX.contains(REPOSITORY));
    }

    /// Fragt wirklich bei GitHub nach und zeigt die Antwort.
    /// Von Hand starten: `cargo test real_update_server -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn asks_the_real_update_server() {
        println!("Antwort von {API_URL}: {:?}", check().map_err(|e| format!("{e:#}")));
    }
}
