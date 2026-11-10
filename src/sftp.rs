//! SFTP-Ziele über den OpenSSH-Client, der zu Windows gehört (`System32\OpenSSH\sftp.exe`).
//! Jede Aktion läuft als eigene, kurze Sitzung mit einer Befehlsdatei. Ein Passwort fragt
//! OpenSSH über ein Hilfsprogramm ab – das ist ZEBack selbst (siehe [`answer_askpass`]).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, NaiveDate, TimeZone};

use crate::job::{Cancelled, Counter, Reporter};
use crate::nextcloud::RemoteFile;
use crate::remote::Remote;
use crate::util::TempFile;

/// Umgebungsvariable, über die ZEBack dem von OpenSSH gestarteten Hilfsprogramm das Passwort reicht.
const SECRET_VAR: &str = "ZEBACK_ASKPASS_SECRET";

#[derive(Clone)]
pub struct SftpSettings {
    /// `server.example.de` oder `server.example.de:2222`
    pub host: String,
    pub user: String,
    pub password: String,
    /// Private Schlüsseldatei (ohne Kennwort); leer = Anmeldung mit Passwort
    pub key_file: String,
    pub remote_dir: String,
}

impl std::fmt::Debug for SftpSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Das Passwort soll in keiner Fehlermeldung und keinem Protokoll auftauchen.
        write!(f, "SftpSettings {{ host: {:?}, user: {:?}, password: *** }}", self.host, self.user)
    }
}

pub struct SftpClient {
    program: PathBuf,
    host: String,
    port: u16,
    user: String,
    password: String,
    key_file: String,
    /// Ordner auf dem Server ohne Schrägstrich am Ende; leer = Anmeldeordner
    dir: String,
}

/// Wird ZEBack von OpenSSH als Hilfsprogramm für die Passwortabfrage gestartet, steht das
/// Passwort in der Umgebung. Dann gibt diese Funktion die Antwort zurück, und das Programm
/// soll sie ausgeben und sich beenden. Beantwortet werden nur Fragen nach Passwort oder
/// Kennwort – nie die Rückfrage, ob einem unbekannten Server vertraut werden soll.
pub fn answer_askpass(args: &[String]) -> Option<Result<String, ()>> {
    let secret = std::env::var(SECRET_VAR).ok()?;
    let prompt = args.first().map(|p| p.to_ascii_lowercase()).unwrap_or_default();
    Some(if prompt.contains("assword") || prompt.contains("passphrase") { Ok(secret) } else { Err(()) })
}

fn sftp_program() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("OpenSSH").join("sftp.exe")
}

/// Zerlegt `server:port` in Name und Port (Standard 22).
fn parse_host(input: &str) -> Result<(String, u16)> {
    let text = input.trim().trim_start_matches("sftp://").trim_end_matches('/');
    let (host, port) = match text.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| anyhow!(t!("Der Port hinter dem Doppelpunkt ist keine Zahl.", "The port after the colon is not a number.")))?,
        ),
        None => (text, 22),
    };
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    if host.is_empty() || !host.chars().all(allowed) || host.starts_with('-') {
        bail!(t!(
            "Bitte den Namen des SFTP-Servers eintragen, z. B. server.example.de oder server.example.de:2222.",
            "Please enter the name of the SFTP server, e.g. server.example.com or server.example.com:2222."
        ));
    }
    Ok((host.to_string(), port))
}

/// Setzt einen Pfad für die Befehlsdatei in Anführungszeichen.
fn quote(path: &str) -> Result<String> {
    if path.contains(['\n', '\r']) {
        bail!(t!("Pfade mit Zeilenumbruch werden nicht unterstützt.", "Paths containing a line break are not supported."));
    }
    Ok(format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\"")))
}

/// Lokale Pfade bekommen Schrägstriche: der umgekehrte Schrägstrich ist in der Befehlsdatei
/// ein Sonderzeichen.
fn quote_local(path: &Path) -> Result<String> {
    quote(&path.display().to_string().replace('\\', "/"))
}

/// Woran eine sftp-Sitzung gescheitert ist – abgelesen an der (englischen) Fehlerausgabe von
/// OpenSSH, unabhängig von der Sprache der Oberfläche.
#[derive(Clone, Copy, PartialEq)]
enum Failure {
    HostKeyChanged,
    LoginRejected,
    UnknownHost,
    Unreachable,
    /// Datei oder Ordner gibt es auf dem Server nicht.
    NotFound,
    AccessDenied,
    Other,
}

fn classify(stderr: &str) -> Failure {
    let text = stderr.to_ascii_lowercase();
    if text.contains("identification has changed") || text.contains("host key verification failed") {
        Failure::HostKeyChanged
    } else if text.contains("permission denied") && text.contains("publickey") || text.contains("permission denied (") {
        Failure::LoginRejected
    } else if text.contains("could not resolve hostname") {
        Failure::UnknownHost
    } else if text.contains("connection refused") || text.contains("connection timed out") || text.contains("network is unreachable") {
        Failure::Unreachable
    } else if text.contains("no such file") {
        Failure::NotFound
    } else if text.contains("permission denied") {
        Failure::AccessDenied
    } else {
        Failure::Other
    }
}

/// Verständliche Meldung aus der Fehlerausgabe von sftp.
fn explain(stderr: &str, host: &str) -> String {
    match classify(stderr) {
        Failure::HostKeyChanged => tr!(
            "Der Server {host} meldet sich mit einem anderen Schlüssel als beim letzten Mal. Wenn das beabsichtigt ist (Server neu aufgesetzt), den alten Eintrag in der Datei known_hosts im Ordner .ssh entfernen.",
            "The server {host} presents a different key than last time. If this is intended (server was set up again), remove the old entry from the known_hosts file in the .ssh folder."
        ),
        Failure::LoginRejected => t!(
            "Anmeldung abgelehnt – Benutzername, Passwort bzw. Schlüsseldatei prüfen",
            "Sign-in rejected – check user name, password or key file"
        )
        .to_string(),
        Failure::UnknownHost => tr!("Der Server {host} wurde nicht gefunden – Namen prüfen", "The server {host} was not found – check the name"),
        Failure::Unreachable => tr!(
            "Der Server {host} ist nicht erreichbar – Adresse, Port und Netzwerk prüfen",
            "The server {host} is not reachable – check address, port and network"
        ),
        Failure::NotFound => t!("Datei oder Ordner auf dem Server nicht gefunden", "File or folder not found on the server").to_string(),
        Failure::AccessDenied => t!("Der Server verweigert den Zugriff auf den Ordner", "The server denies access to the folder").to_string(),
        Failure::Other => {
            // Die letzten Zeilen sagen meist, woran es lag.
            let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let tail = lines[lines.len().saturating_sub(2)..].join(" – ");
            if tail.is_empty() { t!("sftp wurde ohne Meldung beendet", "sftp ended without a message").to_string() } else { tail }
        }
    }
}

/// Fehler einer sftp-Sitzung: die verständliche Meldung und – für Entscheidungen im Programm –
/// die Ursache, damit niemand den Wortlaut der Meldung auswerten muss.
#[derive(Debug)]
struct SftpError {
    message: String,
    not_found: bool,
}

impl std::fmt::Display for SftpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SftpError {}

/// Liest eine Zeile aus `ls -ln`: Dateien liefern Name, Größe und Datum, alles andere `None`.
fn parse_listing_line(line: &str, today: NaiveDate) -> Option<RemoteFile> {
    const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    if !line.starts_with('-') {
        return None;
    }
    let mut rest = line;
    let mut fields = Vec::new();
    // Acht Felder vor dem Namen; der Name selbst darf Leerzeichen enthalten.
    for _ in 0..8 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let name = rest.trim_start().rsplit('/').next()?.trim_end().to_string();
    if name.is_empty() {
        return None;
    }
    let size = fields[4].parse().ok()?;
    let month = MONTHS.iter().position(|m| fields[5].to_ascii_lowercase().starts_with(m))? as u32 + 1;
    let day: u32 = fields[6].parse().ok()?;
    let modified = match fields[7].split_once(':') {
        // Jüngere Dateien: Uhrzeit statt Jahr. Liegt das Datum damit in der Zukunft, war es letztes Jahr.
        Some((hour, minute)) => {
            let mut date = NaiveDate::from_ymd_opt(today.year(), month, day)?;
            if date > today + chrono::Days::new(1) {
                date = NaiveDate::from_ymd_opt(today.year() - 1, month, day)?;
            }
            date.and_hms_opt(hour.parse().ok()?, minute.parse().ok()?, 0)
        }
        None => NaiveDate::from_ymd_opt(fields[7].parse().ok()?, month, day)?.and_hms_opt(0, 0, 0),
    };
    let modified: Option<DateTime<Local>> = modified.and_then(|naive| Local.from_local_datetime(&naive).earliest());
    Some(RemoteFile { name, size, modified })
}

impl SftpClient {
    /// Prüft Angaben und Anmeldung und legt den Sicherungsordner an, falls er fehlt.
    pub fn connect(settings: &SftpSettings) -> Result<Self> {
        let (host, port) = parse_host(&settings.host)?;
        let user = settings.user.trim().to_string();
        if user.is_empty() || user.starts_with('-') || user.contains(['@', ' ', '"']) {
            bail!(t!(
                "Bitte einen gültigen Benutzernamen für den SFTP-Server eintragen.",
                "Please enter a valid user name for the SFTP server."
            ));
        }
        let key_file = settings.key_file.trim().to_string();
        if key_file.is_empty() && settings.password.is_empty() {
            bail!(t!("Bitte ein Passwort eintragen oder eine Schlüsseldatei auswählen.", "Please enter a password or choose a key file."));
        }
        if !key_file.is_empty() && !Path::new(&key_file).is_file() {
            bail!(tr!("Die Schlüsseldatei wurde nicht gefunden: {key_file}", "The key file was not found: {key_file}"));
        }
        let program = sftp_program();
        if !program.is_file() {
            bail!(t!(
                "Der OpenSSH-Client von Windows fehlt. Er lässt sich in den Windows-Einstellungen unter „Optionale Features“ nachinstallieren.",
                "The Windows OpenSSH client is missing. It can be installed in Windows Settings under “Optional features”."
            ));
        }
        let absolute = settings.remote_dir.trim().starts_with('/');
        let folders: Vec<&str> =
            settings.remote_dir.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty() && *s != "." && *s != "..").collect();
        let dir = format!("{}{}", if absolute { "/" } else { "" }, folders.join("/"));
        let client = Self { program, host, port, user, password: settings.password.clone(), key_file, dir };

        // Ordner Stufe für Stufe anlegen ("-" davor: Fehler ignorieren, falls es ihn schon gibt).
        let mut batch = String::new();
        let mut path = String::from(if absolute { "/" } else { "" });
        for (index, folder) in folders.iter().enumerate() {
            if index > 0 {
                path.push('/');
            }
            path.push_str(folder);
            batch.push_str(&format!("-mkdir {}\n", quote(&path)?));
        }
        if !client.dir.is_empty() {
            batch.push_str(&format!("cd {}\n", quote(&client.dir)?));
        }
        batch.push_str("pwd\n");
        client.run(&batch, None, &mut || {}).context(t!("Anmeldung am SFTP-Server", "Signing in to the SFTP server"))?;
        Ok(client)
    }

    fn remote_path(&self, name: &str) -> String {
        if self.dir.is_empty() { name.to_string() } else { format!("{}/{name}", self.dir.trim_end_matches('/')) }
    }

    /// Argumente für sftp.exe (ohne die Befehlsdatei).
    fn arguments(&self) -> Vec<String> {
        let mut args = vec![
            // Meldungen zur Verbindung mitschreiben – daraus entstehen die verständlichen
            // Fehlermeldungen (mit weniger meldet OpenSSH nur "Connection closed").
            "-o".to_string(),
            "LogLevel=INFO".to_string(),
            "-P".to_string(),
            self.port.to_string(),
            // Unbekannte Server beim ersten Mal merken, einen geänderten Schlüssel ablehnen.
            "-o".to_string(),
            "StrictHostKeyChecking=accept-new".to_string(),
            "-o".to_string(),
            "ConnectTimeout=20".to_string(),
            "-o".to_string(),
            "ServerAliveInterval=30".to_string(),
            "-o".to_string(),
            "NumberOfPasswordPrompts=1".to_string(),
        ];
        if self.key_file.is_empty() {
            args.extend(["-o".to_string(), "PreferredAuthentications=password,keyboard-interactive".to_string()]);
            args.extend(["-o".to_string(), "PubkeyAuthentication=no".to_string()]);
        } else {
            args.extend(["-o".to_string(), "PreferredAuthentications=publickey".to_string()]);
            args.extend(["-o".to_string(), "IdentitiesOnly=yes".to_string()]);
            args.extend(["-i".to_string(), self.key_file.clone()]);
        }
        args
    }

    /// Führt die Befehle in einer Sitzung aus und gibt die Ausgabe zurück. `tick` wird etwa
    /// fünfmal pro Sekunde aufgerufen (für Fortschritt), `rep` erlaubt den Abbruch.
    fn run(&self, batch: &str, rep: Option<&Reporter>, tick: &mut dyn FnMut()) -> Result<String> {
        let batch_file = TempFile::new(std::env::temp_dir().join(format!(
            "zeback-sftp-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default()
        )));
        fs::write(batch_file.path(), batch)
            .context(t!("Befehlsdatei für sftp konnte nicht geschrieben werden", "Could not write the command file for sftp"))?;

        let mut command = Command::new(&self.program);
        command
            .args(self.arguments())
            .arg("-b")
            .arg(batch_file.path())
            .arg(format!("{}@{}", self.user, self.host))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // OpenSSH fragt das Passwort über dieses Programm ab statt an einer Konsole.
            .env("SSH_ASKPASS", std::env::current_exe().context(t!("Programmpfad nicht ermittelbar", "Could not determine the program path"))?)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DISPLAY", "zeback")
            .env(SECRET_VAR, &self.password);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command.spawn().context(t!("sftp.exe ließ sich nicht starten", "Could not start sftp.exe"))?;
        let read_all = |mut pipe: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = pipe.read_to_end(&mut bytes);
                String::from_utf8_lossy(&bytes).into_owned()
            })
        };
        let stdout = read_all(Box::new(child.stdout.take().expect("Ausgabe umgeleitet")));
        let stderr = read_all(Box::new(child.stderr.take().expect("Fehlerausgabe umgeleitet")));
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if rep.is_some_and(|rep| rep.cancelled()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Cancelled.into());
            }
            tick();
            std::thread::sleep(Duration::from_millis(200));
        };
        let output = stdout.join().unwrap_or_default();
        let errors = stderr.join().unwrap_or_default();
        if !status.success() {
            // Die Ursache geht getrennt vom Wortlaut mit: `list` fragt sie ab.
            let not_found = classify(&errors) == Failure::NotFound;
            return Err(SftpError { message: explain(&errors, &self.host), not_found }.into());
        }
        Ok(output)
    }
}

impl Remote for SftpClient {
    fn upload(&self, local: &Path, name: &str, rep: &Reporter) -> Result<String> {
        let size = fs::metadata(local)?.len();
        let remote = self.remote_path(name);
        let part = format!("{remote}.part");
        // Erst unter vorläufigem Namen hochladen: eine abgebrochene Übertragung sieht dann
        // nicht wie eine fertige Sicherung aus.
        let batch = format!(
            "-rm {part}\nput {local} {part}\nrename {part} {remote}\n",
            part = quote(&part)?,
            local = quote_local(local)?,
            remote = quote(&remote)?,
        );
        // sftp meldet in einer Befehlsdatei keinen Zwischenstand; angezeigt wird nur "läuft".
        rep.progress(0, 0);
        rep.log(t!(
            "SFTP überträgt ohne Fortschrittsanzeige – bei großen Sicherungen bitte Geduld.",
            "SFTP transfers without a progress display – please be patient with large backups."
        ));
        self.run(&batch, Some(rep), &mut || {}).context(t!("Upload per SFTP", "Upload via SFTP"))?;
        rep.progress(size, size);
        Ok(format!("{}:{remote}", self.host))
    }

    fn list(&self) -> Result<Vec<RemoteFile>> {
        let target = if self.dir.is_empty() { ".".to_string() } else { self.dir.clone() };
        let output = match self.run(&format!("ls -ln {}\n", quote(&target)?), None, &mut || {}) {
            Ok(output) => output,
            // Den Ordner gibt es noch nicht: also auch keine Sicherungen.
            Err(e) if e.downcast_ref::<SftpError>().is_some_and(|e| e.not_found) => return Ok(Vec::new()),
            Err(e) => return Err(e.context(t!("Sicherungen auf dem SFTP-Server auflisten", "Listing backups on the SFTP server"))),
        };
        let today = Local::now().date_naive();
        let mut files: Vec<RemoteFile> = output
            .lines()
            .filter_map(|line| parse_listing_line(line.trim_end(), today))
            .filter(|file| file.name.to_ascii_lowercase().ends_with(".zip"))
            .collect();
        files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
        Ok(files)
    }

    fn delete(&self, name: &str) -> Result<()> {
        let batch = format!("rm {}\n", quote(&self.remote_path(name))?);
        self.run(&batch, None, &mut || {}).map(|_| ()).context(t!("Alte Sicherung auf dem SFTP-Server löschen", "Deleting an old backup on the SFTP server"))
    }

    fn download(&self, file: &RemoteFile, dest: &Path, rep: &Reporter) -> Result<()> {
        let mut part_name = dest.as_os_str().to_owned();
        part_name.push(".part");
        let mut part = TempFile::new(part_name);
        let batch = format!("get {} {}\n", quote(&self.remote_path(&file.name))?, quote_local(part.path())?);
        // Der Fortschritt ergibt sich aus der wachsenden Datei auf diesem PC.
        let mut counter = Counter::new(rep, file.size);
        let part_path = part.path().to_path_buf();
        self.run(&batch, Some(rep), &mut || {
            if let Ok(meta) = fs::metadata(&part_path) {
                counter.set(meta.len().min(file.size));
            }
        })
        .context(t!("Download per SFTP", "Download via SFTP"))?;
        fs::rename(part.path(), dest)?;
        part.keep();
        counter.set(file.size);
        Ok(())
    }

    fn describe(&self) -> String {
        let folder =
            if self.dir.is_empty() { t!("Anmeldeordner", "home folder").to_string() } else { tr!("Ordner {}", "folder {}", self.dir) };
        tr!("Angemeldet als „{}“ an {} (Port {}), {folder}.", "Signed in as “{}” to {} (port {}), {folder}.", self.user, self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_host_and_port() {
        assert_eq!(parse_host("server.example.de").unwrap(), ("server.example.de".to_string(), 22));
        assert_eq!(parse_host(" sftp://nas.local:2222/ ").unwrap(), ("nas.local".to_string(), 2222));
        // Nichts, was sftp als Option oder die Shell als Befehl verstehen könnte.
        for bad in ["", "-oProxyCommand=calc", "server example", "server;calc", "server:abc"] {
            assert!(parse_host(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn quotes_paths_for_the_batch_file() {
        assert_eq!(quote("Backups/Büro PC/a.zip").unwrap(), "\"Backups/Büro PC/a.zip\"");
        assert_eq!(quote("a\"b\\c").unwrap(), "\"a\\\"b\\\\c\"");
        assert!(quote("a\nrm *").is_err());
        assert_eq!(quote_local(Path::new("C:\\Temp\\Backup 1.zip")).unwrap(), "\"C:/Temp/Backup 1.zip\"");
    }

    #[test]
    fn reads_directory_listings() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 10).unwrap();
        let file = parse_listing_line("-rw-r--r--    1 1000     1000      5368709120 Nov  9 08:15 Backups/Backup_PC Büro_2026-11-09_081500.zip", today).unwrap();
        assert_eq!((file.name.as_str(), file.size), ("Backup_PC Büro_2026-11-09_081500.zip", 5_368_709_120));
        assert_eq!(file.modified.unwrap().format("%d.%m.%Y %H:%M").to_string(), "09.11.2026 08:15");
        // Ältere Dateien tragen das Jahr statt der Uhrzeit; ein Datum "in der Zukunft" war letztes Jahr.
        let old = parse_listing_line("-rw-------    1 0        0             692 Mar  3  2024 alt.zip", today).unwrap();
        assert_eq!(old.modified.unwrap().format("%d.%m.%Y").to_string(), "03.03.2024");
        let last_year = parse_listing_line("-rw-------    1 0        0             692 Dec 24 18:00 weihnachten.zip", today).unwrap();
        assert_eq!(last_year.modified.unwrap().format("%d.%m.%Y").to_string(), "24.12.2025");
        // Ordner, Befehlsecho und Unlesbares werden übergangen.
        for line in ["drwxr-xr-x    2 1000     1000         4096 Nov  9 08:15 Backups", "sftp> ls -ln \"Backups\"", "", "-rw-r--r-- kaputt"] {
            assert!(parse_listing_line(line, today).is_none(), "{line}");
        }
    }

    #[test]
    fn explains_common_failures() {
        assert!(explain("user@host: Permission denied (publickey,password).", "host").contains("Anmeldung abgelehnt"));
        assert!(explain("ssh: connect to host h port 22: Connection refused\r\nConnection closed", "h").contains("nicht erreichbar"));
        assert!(explain("ssh: Could not resolve hostname x: No such host is known.", "x").contains("nicht gefunden"));
        assert!(explain("@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @", "h").contains("anderen Schlüssel"));
        assert!(explain("Couldn't stat remote file: No such file or directory", "h").contains("nicht gefunden"));
        assert!(explain("etwas anderes\nganz am Ende", "h").contains("ganz am Ende"));
    }

    #[test]
    fn askpass_answers_only_password_questions() {
        // SAFETY: Nur dieser Test liest und schreibt diese Variable.
        unsafe { std::env::set_var(SECRET_VAR, "geheim") };
        let ask = |prompt: &str| answer_askpass(&[prompt.to_string()]);
        assert_eq!(ask("anna@server's password: "), Some(Ok("geheim".to_string())));
        assert_eq!(ask("Enter passphrase for key 'id_ed25519': "), Some(Ok("geheim".to_string())));
        assert_eq!(ask("Are you sure you want to continue connecting (yes/no/[fingerprint])?"), Some(Err(())));
        unsafe { std::env::remove_var(SECRET_VAR) };
        assert_eq!(ask("anna@server's password: "), None);
    }

    #[test]
    fn rejects_incomplete_settings_and_unreachable_servers() {
        let settings = |host: &str, password: &str| SftpSettings {
            host: host.into(),
            user: "anna".into(),
            password: password.into(),
            key_file: String::new(),
            remote_dir: "Backups".into(),
        };
        assert!(SftpClient::connect(&settings("server.example.de", "")).err().unwrap().to_string().contains("Passwort"));
        assert!(!format!("{:?}", settings("h", "streng-geheim")).contains("streng-geheim"));
        // An diesem Port lauscht nichts: sftp.exe läuft wirklich und die Meldung wird übersetzt.
        if sftp_program().is_file() {
            let error = SftpClient::connect(&settings("127.0.0.1:1", "x")).err().unwrap();
            assert!(format!("{error:#}").contains("nicht erreichbar"), "{error:#}");
        }
    }
}
