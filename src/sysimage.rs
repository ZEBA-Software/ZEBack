//! Systemabbild: komplettes Windows samt Programmen, erstellt mit der Abbildsicherung,
//! die Windows selbst mitbringt (`wbadmin`). Zurückgespielt wird ein solches Abbild über die
//! Windows-Wiederherstellungsumgebung, nicht aus dem laufenden System heraus.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::job::{Cancelled, Reporter};

#[derive(Clone, Debug)]
pub struct ImageJob {
    /// Ziel: Laufwerk (`E:`) oder Netzwerkfreigabe (`\\server\freigabe`)
    pub target: String,
    /// Zusätzliche Laufwerke neben den für den Start nötigen, z. B. `D:`
    pub include: Vec<String>,
    /// Anmeldung (Benutzer, Passwort) für eine Netzwerkfreigabe als Ziel
    pub login: Option<(String, String)>,
    /// Geschätzte Datenmenge (belegter Platz der gesicherten Laufwerke); 0 = unbekannt.
    /// Dient nur der Fortschrittsanzeige.
    pub expected_bytes: u64,
}

#[derive(Debug)]
pub struct ImageSummary {
    pub target: String,
    pub duration: Duration,
}

#[cfg(windows)]
mod ffi {
    #[link(name = "shell32")]
    unsafe extern "system" {
        pub fn IsUserAnAdmin() -> i32;
        pub fn ShellExecuteW(
            window: isize,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> isize;
    }

    /// Zähler für Lese- und Schreibvorgänge eines Prozesses (IO_COUNTERS).
    #[repr(C)]
    #[derive(Default)]
    pub struct IoCounters {
        pub read_operations: u64,
        pub write_operations: u64,
        pub other_operations: u64,
        pub read_bytes: u64,
        pub write_bytes: u64,
        pub other_bytes: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
        pub fn GetProcessIoCounters(process: isize, counters: *mut IoCounters) -> i32;
        pub fn CloseHandle(handle: isize) -> i32;
        pub fn SetThreadExecutionState(flags: u32) -> u32;
        pub fn MultiByteToWideChar(
            code_page: u32,
            flags: u32,
            bytes: *const u8,
            byte_count: i32,
            wide: *mut u16,
            wide_count: i32,
        ) -> i32;
    }
}

/// Läuft Packup mit Administratorrechten?
#[cfg(windows)]
pub fn is_elevated() -> bool {
    // SAFETY: Aufruf ohne Parameter.
    unsafe { ffi::IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// Startet ein Programm über die Windows-Shell. `false`, wenn der Start scheiterte oder die
/// Nachfrage der Benutzerkontensteuerung abgelehnt wurde.
#[cfg(windows)]
fn shell_execute(operation: &str, file: &std::ffi::OsStr, arguments: &str) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let wide = |s: &std::ffi::OsStr| -> Vec<u16> { s.encode_wide().chain(Some(0)).collect() };
    let (operation, file, parameters) = (wide(operation.as_ref()), wide(file), wide(arguments.as_ref()));
    const SW_SHOWNORMAL: i32 = 1;
    // SAFETY: Alle Zeichenketten sind nullterminiert und leben bis zum Ende des Aufrufs.
    let result = unsafe {
        ffi::ShellExecuteW(0, operation.as_ptr(), file.as_ptr(), parameters.as_ptr(), std::ptr::null(), SW_SHOWNORMAL)
    };
    // Werte bis 32 bedeuten Fehler.
    result > 32
}

#[cfg(not(windows))]
fn shell_execute(_operation: &str, _file: &std::ffi::OsStr, _arguments: &str) -> bool {
    false
}

/// Startet Packup erneut mit Administratorrechten (Windows fragt per UAC nach).
/// `arguments` werden an die neue Instanz übergeben.
pub fn relaunch_elevated(arguments: &str) -> Result<()> {
    let exe = std::env::current_exe()
        .context(t!("Programmpfad nicht ermittelbar", "The program path could not be determined"))?;
    if !shell_execute("runas", exe.as_os_str(), arguments) {
        bail!(t!(
            "Der Start mit Administratorrechten wurde abgebrochen oder ist fehlgeschlagen.",
            "Starting with administrator rights was cancelled or failed."
        ));
    }
    Ok(())
}

/// Öffnet eine Webadresse im Standardbrowser. Alles andere als http(s) wird abgelehnt, damit
/// eine Adresse aus einer Server-Antwort kein Programm starten kann.
pub fn open_in_browser(url: &str) -> Result<()> {
    let is_web = url.starts_with("https://") || url.starts_with("http://");
    if !is_web || url.chars().any(|c| c.is_control() || c == '"' || c == ' ') {
        bail!(tr!("Keine gültige Webadresse: {url}", "Not a valid web address: {url}"));
    }
    if !shell_execute("open", std::ffi::OsStr::new(url), "") {
        bail!(t!(
            "Windows konnte den Standardbrowser nicht starten.",
            "Windows could not start the default browser."
        ));
    }
    Ok(())
}

fn system32(file: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(file)
}

/// Öffnet den Windows-Assistenten "Wiederherstellungslaufwerk erstellen". Mit dem so erzeugten
/// USB-Stick lässt sich der PC starten, um ein Systemabbild zurückzuspielen.
pub fn open_recovery_drive_wizard() -> Result<()> {
    let wizard = system32("RecoveryDrive.exe");
    if !wizard.exists() {
        bail!(t!(
            "Der Windows-Assistent für Wiederherstellungslaufwerke fehlt auf diesem PC.",
            "The Windows recovery drive wizard is missing on this PC."
        ));
    }
    if !shell_execute("open", wizard.as_os_str(), "") {
        bail!(t!(
            "Der Assistent wurde nicht gestartet (Nachfrage abgelehnt?).",
            "The wizard was not started (prompt declined?)."
        ));
    }
    Ok(())
}

/// Hält den PC wach, solange der Wert lebt: kein automatischer Ruhezustand während einer
/// Sicherung. Der Bildschirm darf trotzdem ausgehen.
pub struct KeepAwake;

impl KeepAwake {
    const ES_CONTINUOUS: u32 = 0x8000_0000;
    const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    pub fn new() -> Self {
        // SAFETY: Aufruf mit festen Flags; gilt für den aufrufenden Thread.
        #[cfg(windows)]
        unsafe {
            ffi::SetThreadExecutionState(Self::ES_CONTINUOUS | Self::ES_SYSTEM_REQUIRED);
        }
        Self
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: wie oben; hebt die Anforderung wieder auf.
        #[cfg(windows)]
        unsafe {
            ffi::SetThreadExecutionState(Self::ES_CONTINUOUS);
        }
    }
}

/// Antwort auf eine Ja/Nein-Frage an Windows, die auch offen bleiben kann.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Unknown,
}

/// Was vor einem Systemabbild über den PC bekannt sein sollte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemFacts {
    /// Ist die Windows-Wiederherstellungsumgebung eingeschaltet? (nur mit Administratorrechten ermittelbar)
    pub recovery_environment: Answer,
    /// Ist das Windows-Laufwerk mit BitLocker verschlüsselt?
    pub bitlocker: Answer,
}

/// Startet ein Konsolenprogramm ohne Fenster und liefert seine Ausgabe als Text.
fn capture(program: PathBuf, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command.output().ok()?;
    Some(decode_console(&output.stdout))
}

/// Liest den Status aus der Ausgabe von `reagentc /info`.
pub fn parse_recovery_status(output: &str) -> Answer {
    for line in output.lines() {
        let lower = line.to_lowercase();
        let Some((key, value)) = lower.split_once(':') else { continue };
        if !key.contains("status") {
            continue;
        }
        if value.contains("disabled") || value.contains("deaktiviert") {
            return Answer::No;
        }
        if value.contains("enabled") || value.contains("aktiviert") {
            return Answer::Yes;
        }
    }
    Answer::Unknown
}

/// Liest den BitLocker-Zustand, wie ihn die Windows-Shell meldet
/// (1 = an, 2 = aus, 3 = wird verschlüsselt, 4 = wird entschlüsselt, 5 = angehalten, 6 = gesperrt).
pub fn parse_bitlocker_value(output: &str) -> Answer {
    match output.trim().parse::<u32>() {
        Ok(1 | 3 | 5 | 6) => Answer::Yes,
        Ok(2 | 4) => Answer::No,
        _ => Answer::Unknown,
    }
}

/// Ermittelt die Angaben. Dauert einen Moment und gehört in einen Hintergrund-Thread.
pub fn system_facts() -> SystemFacts {
    let recovery_environment = if is_elevated() {
        capture(system32("reagentc.exe"), &["/info"]).map_or(Answer::Unknown, |o| parse_recovery_status(&o))
    } else {
        Answer::Unknown
    };
    // Der Laufwerksbuchstabe stammt aus der Umgebung und wird auf einen Buchstaben begrenzt.
    let letter = std::env::var("SystemDrive")
        .ok()
        .and_then(|d| d.chars().next())
        .filter(char::is_ascii_alphabetic)
        .unwrap_or('C');
    let script = format!(
        "(New-Object -ComObject Shell.Application).NameSpace('{letter}:').Self.ExtendedProperty('System.Volume.BitLockerProtection')"
    );
    let powershell = system32("WindowsPowerShell\\v1.0\\powershell.exe");
    let bitlocker = capture(powershell, &["-NoProfile", "-NonInteractive", "-Command", &script])
        .map_or(Answer::Unknown, |o| parse_bitlocker_value(&o));
    SystemFacts { recovery_environment, bitlocker }
}

/// Ein vorhandenes Systemabbild auf einem Ziel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExistingImage {
    pub computer: String,
    /// Zeitpunkt der Sicherung (Weltzeit), falls aus dem Ordnernamen lesbar
    pub created: Option<chrono::NaiveDateTime>,
    pub size: u64,
}

/// Sucht Systemabbilder im Ordner `WindowsImageBackup` des Ziels. Der Ordner ist nur mit
/// Administratorrechten lesbar; ohne sie bleibt die Liste leer.
pub fn list_images(target: &str) -> Vec<ExistingImage> {
    let root = PathBuf::from(format!("{}\\", target.trim_end_matches('\\'))).join("WindowsImageBackup");
    let mut images = Vec::new();
    let Ok(computers) = std::fs::read_dir(&root) else { return images };
    for computer in computers.flatten().filter(|e| e.path().is_dir()) {
        let Ok(backups) = std::fs::read_dir(computer.path()) else { continue };
        for backup in backups.flatten() {
            let name = backup.file_name().to_string_lossy().into_owned();
            // Ordner heißen z. B. "Backup 2026-11-10 120000".
            if !backup.path().is_dir() || !name.starts_with("Backup ") {
                continue;
            }
            let size = std::fs::read_dir(backup.path())
                .map(|files| files.flatten().filter_map(|f| f.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
                .unwrap_or(0);
            images.push(ExistingImage {
                computer: computer.file_name().to_string_lossy().into_owned(),
                created: chrono::NaiveDateTime::parse_from_str(&name, "Backup %Y-%m-%d %H%M%S").ok(),
                size,
            });
        }
    }
    images.sort_by(|a, b| b.created.cmp(&a.created));
    images
}

/// Wandelt die Ausgabe eines Konsolenprogramms (OEM-Zeichensatz) in Text um.
#[cfg(windows)]
pub(crate) fn decode_console(bytes: &[u8]) -> String {
    const CP_OEMCP: u32 = 1;
    if bytes.is_empty() {
        return String::new();
    }
    let mut wide = vec![0u16; bytes.len()];
    // SAFETY: Quell- und Zielpuffer sind mit ihrer tatsächlichen Länge angegeben;
    // ein OEM-Byte ergibt höchstens ein UTF-16-Zeichen.
    let written = unsafe {
        ffi::MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), wide.len() as i32)
    };
    String::from_utf16_lossy(&wide[..written.max(0) as usize])
}

#[cfg(not(windows))]
pub(crate) fn decode_console(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Macht aus der Eingabe ein Ziel, wie `wbadmin` es erwartet: `E:` oder `\\server\freigabe`.
pub fn normalize_target(input: &str) -> Result<String> {
    let target = input.trim().trim_matches('"').trim_end_matches('\\');
    if let Some(rest) = target.strip_prefix("\\\\") {
        let mut parts = rest.split('\\').filter(|p| !p.is_empty());
        if parts.next().is_none() || parts.next().is_none() {
            bail!(t!(
                "Die Netzwerkfreigabe muss die Form \\\\server\\freigabe haben.",
                "The network share must have the form \\\\server\\share."
            ));
        }
        return Ok(target.to_string());
    }
    let mut chars = target.chars();
    match (chars.next(), chars.next(), chars.next()) {
        (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic() => Ok(format!("{}:", letter.to_ascii_uppercase())),
        _ => bail!(t!(
            "Als Ziel bitte ein Laufwerk (z. B. E:) oder eine Netzwerkfreigabe (\\\\server\\freigabe) angeben.",
            "Please enter a drive (e.g. E:) or a network share (\\\\server\\share) as the target."
        )),
    }
}

/// Aufrufparameter für `wbadmin start backup`.
pub fn build_args(job: &ImageJob) -> Vec<String> {
    let mut args = vec![
        "start".to_string(),
        "backup".to_string(),
        format!("-backupTarget:{}", job.target),
        // Alles, was Windows zum Starten braucht: Systemlaufwerk, Start- und Wiederherstellungspartition.
        "-allCritical".to_string(),
    ];
    if !job.include.is_empty() {
        args.push(format!("-include:{}", job.include.join(",")));
    }
    if let Some((user, password)) = &job.login {
        args.push(format!("-user:{user}"));
        args.push(format!("-password:{password}"));
    }
    args.push("-quiet".to_string());
    args
}

/// Der Aufruf fürs Protokoll – ohne das Passwort.
fn describe_args(args: &[String]) -> String {
    args.iter()
        .map(|a| if a.starts_with("-password:") { "-password:***" } else { a.as_str() })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Liest den Fortschritt aus einer Ausgabezeile, z. B. `… (42%) wurden kopiert.`
pub fn parse_percent(line: &str) -> Option<u64> {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'%' {
            continue;
        }
        let digits_start = bytes[..index].iter().rposition(|b| !b.is_ascii_digit()).map_or(0, |p| p + 1);
        if digits_start < index
            && digits_start > 0
            && bytes[digits_start - 1] == b'('
            && bytes.get(index + 1) == Some(&b')')
        {
            return line[digits_start..index].parse().ok().filter(|p| *p <= 100);
        }
    }
    None
}

fn wbadmin_path() -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join("wbadmin.exe")
}

fn command(args: &[String]) -> Command {
    let mut command = Command::new(wbadmin_path());
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // Kein eigenes Konsolenfenster aufblitzen lassen.
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Liest einen Ausgabekanal zeilenweise (Zeilenende ist `\n` oder `\r`) und reicht die Zeilen weiter.
fn forward_lines(mut source: impl Read + Send + 'static, lines: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];
        let flush = |pending: &mut Vec<u8>| {
            let line = decode_console(pending).trim().to_string();
            pending.clear();
            line.is_empty() || lines.send(line).is_ok()
        };
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    for byte in &buffer[..count] {
                        if *byte == b'\n' || *byte == b'\r' {
                            if !flush(&mut pending) {
                                return;
                            }
                        } else {
                            pending.push(*byte);
                        }
                    }
                }
            }
        }
        flush(&mut pending);
    });
}

/// Kennung des laufenden Prozesses mit diesem Namen (z. B. `wbengine.exe`), über `tasklist`.
fn process_id(image_name: &str) -> Option<u32> {
    let filter = format!("IMAGENAME eq {image_name}");
    let output = capture(system32("tasklist.exe"), &["/FI", &filter, "/FO", "CSV", "/NH"])?;
    // Zeile der Form "wbengine.exe","16748","Services","0","12.345 K"
    output.lines().find_map(|line| {
        let mut fields = line.split(',').map(|field| field.trim().trim_matches('"'));
        fields.next().filter(|name| name.eq_ignore_ascii_case(image_name))?;
        fields.next()?.parse().ok()
    })
}

/// Wie viele Bytes der Prozess seit seinem Start geschrieben hat.
#[cfg(windows)]
fn written_bytes(process_id: u32) -> Option<u64> {
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    let mut counters = ffi::IoCounters::default();
    // SAFETY: Das Handle wird nur zwischen Öffnen und Schließen benutzt; `counters` hat die
    // von Windows erwartete Größe.
    let ok = unsafe {
        let process = ffi::OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
        if process == 0 {
            return None;
        }
        let ok = ffi::GetProcessIoCounters(process, &mut counters) != 0;
        ffi::CloseHandle(process);
        ok
    };
    ok.then_some(counters.write_bytes)
}

#[cfg(not(windows))]
fn written_bytes(_process_id: u32) -> Option<u64> {
    None
}

/// Kopf- und Leerzeilen von wbadmin, die als Stand nichts aussagen.
fn is_noise(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    line.trim().is_empty() || lower.starts_with("wbadmin ") || lower.contains("copyright")
}

/// Erstellt das Systemabbild. Braucht Administratorrechte.
pub fn run(job: &ImageJob, rep: &Reporter) -> Result<ImageSummary> {
    let started = Instant::now();
    if !is_elevated() {
        bail!(t!(
            "Für ein Systemabbild muss Packup als Administrator laufen.",
            "Packup must run as administrator to create a system image."
        ));
    }
    let args = build_args(job);
    rep.log(format!("wbadmin {}", describe_args(&args)));
    rep.status(t!("Systemabbild wird vorbereitet …", "Preparing system image …"));
    rep.progress(0, 0);

    let mut child = command(&args).spawn().context(t!(
        "Die Windows-Abbildsicherung (wbadmin) konnte nicht gestartet werden",
        "The Windows image backup (wbadmin) could not be started"
    ))?;
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        forward_lines(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_lines(stderr, tx.clone());
    }
    drop(tx);

    // Lässt sich messen, wie viel der Sicherungsdienst schreibt, zeigt der Fortschritt Bytes
    // (daraus ergeben sich Tempo und Restzeit); sonst nur die Prozentangabe von wbadmin.
    let measuring = std::cell::Cell::new(false);
    let mut last_lines: VecDeque<String> = VecDeque::new();
    let mut handle = |line: String| {
        match parse_percent(&line) {
            Some(percent) => {
                if !measuring.get() {
                    rep.progress(percent, 100);
                }
                rep.status(line.clone());
            }
            None => {
                // Auch Zwischenschritte ohne Prozentangabe zeigen (z. B. "Schattenkopie wird erstellt").
                if !is_noise(&line) {
                    rep.status(line.clone());
                }
                rep.log(line.clone());
            }
        }
        if last_lines.len() == 6 {
            last_lines.pop_front();
        }
        last_lines.push_back(line);
    };

    let mut stopping = false;
    // Der Dienst "wbengine" schreibt das Abbild; sein Schreibzähler ist der wahre Fortschritt.
    let mut engine: Option<u32> = None;
    let mut baseline: Option<u64> = None;
    let mut last_measured = Instant::now() - Duration::from_secs(10);
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            handle(line);
        }
        let pause = if engine.is_some() { Duration::from_secs(1) } else { Duration::from_secs(3) };
        if !stopping && last_measured.elapsed() >= pause {
            last_measured = Instant::now();
            if engine.is_none() {
                engine = process_id("wbengine.exe");
            }
            match engine.and_then(written_bytes) {
                Some(written) => {
                    let done = written.saturating_sub(*baseline.get_or_insert(written));
                    measuring.set(true);
                    // Die erwartete Menge ist geschätzt: nicht über 100 % hinaus anzeigen.
                    match job.expected_bytes {
                        0 => rep.progress(done, 0),
                        expected => rep.progress(done.min(expected), expected),
                    }
                }
                // Dienst (noch) nicht da oder nicht lesbar: später erneut suchen.
                None => engine = None,
            }
        }
        if let Some(status) = child
            .try_wait()
            .context(t!("Status der Abbildsicherung nicht lesbar", "The status of the image backup could not be read"))?
        {
            // Restliche Ausgabe abholen, bis beide Kanäle geschlossen sind.
            while let Ok(line) = rx.recv_timeout(Duration::from_secs(2)) {
                handle(line);
            }
            break status;
        }
        if rep.cancelled() && !stopping {
            stopping = true;
            rep.status(t!("Abbildsicherung wird abgebrochen …", "Cancelling image backup …"));
            // Die Sicherung läuft in einem Windows-Dienst; sie muss dort gestoppt werden.
            let stop = ["stop".to_string(), "job".to_string(), "-quiet".to_string()];
            let _ = command(&stop).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        std::thread::sleep(Duration::from_millis(150));
    };

    if stopping {
        return Err(Cancelled.into());
    }
    if !status.success() {
        let details: Vec<String> = last_lines.into_iter().collect();
        bail!(tr!(
            "Die Windows-Abbildsicherung ist fehlgeschlagen (Code {}).\n{}",
            "The Windows image backup failed (code {}).\n{}",
            status.code().unwrap_or(-1),
            details.join("\n")
        ));
    }
    rep.progress(100, 100);
    Ok(ImageSummary { target: job.target.clone(), duration: started.elapsed() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_progress_in_german_and_english() {
        assert_eq!(parse_percent("Sicherung von Volume \"(C:)\" wird erstellt, (42%) wurden kopiert."), Some(42));
        assert_eq!(parse_percent("Creating a backup of volume (C:), copied (7%)."), Some(7));
        assert_eq!(parse_percent("Die Sicherung von Volume \"(C:)\" wurde erfolgreich abgeschlossen. (100%)"), Some(100));
        assert_eq!(parse_percent("Volume (C:) wird gesichert"), None);
        assert_eq!(parse_percent("50% fertig"), None);
        assert_eq!(parse_percent("(250%)"), None);
    }

    #[test]
    fn reads_recovery_environment_status() {
        let german = "Konfigurationsinformationen zur Windows-Wiederherstellungsumgebung (WinRE):\n\n    WinRE-Status:              Enabled\n    WinRE-Ort:                 \\\\?\\GLOBALROOT\\device\\harddisk0\\partition4\\Recovery\\WindowsRE\n";
        assert_eq!(parse_recovery_status(german), Answer::Yes);
        assert_eq!(parse_recovery_status("    Windows RE status:         Disabled\n"), Answer::No);
        assert_eq!(parse_recovery_status("    WinRE-Status:              Deaktiviert\n"), Answer::No);
        assert_eq!(parse_recovery_status("REAGENTC.EXE: Vorgang fehlgeschlagen: 5\n"), Answer::Unknown);
    }

    #[test]
    fn reads_bitlocker_value() {
        assert_eq!(parse_bitlocker_value("1\r\n"), Answer::Yes);
        assert_eq!(parse_bitlocker_value("2"), Answer::No);
        assert_eq!(parse_bitlocker_value("5"), Answer::Yes);
        assert_eq!(parse_bitlocker_value(""), Answer::Unknown);
        assert_eq!(parse_bitlocker_value("Fehler"), Answer::Unknown);
    }

    #[test]
    fn finds_existing_images_on_a_target() {
        let base = std::env::temp_dir().join(format!("packup-test-images-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let newer = base.join("WindowsImageBackup").join("PC01").join("Backup 2026-11-10 120000");
        let older = base.join("WindowsImageBackup").join("PC01").join("Backup 2026-10-01 080000");
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::create_dir_all(&older).unwrap();
        std::fs::create_dir_all(base.join("WindowsImageBackup").join("PC01").join("Catalog")).unwrap();
        std::fs::write(newer.join("system.vhdx"), vec![0u8; 5000]).unwrap();
        std::fs::write(newer.join("BackupSpecs.xml"), vec![0u8; 100]).unwrap();
        std::fs::write(older.join("system.vhdx"), vec![0u8; 3000]).unwrap();

        let images = list_images(&base.to_string_lossy());
        assert_eq!(images.len(), 2);
        assert_eq!((images[0].computer.as_str(), images[0].size), ("PC01", 5100));
        assert_eq!(images[0].created.unwrap().to_string(), "2026-11-10 12:00:00");
        assert_eq!(images[1].size, 3000);
        assert!(list_images("Q:").is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(windows)]
    #[test]
    fn gathers_system_facts() {
        // Ohne Administratorrechte bleibt die Wiederherstellungsumgebung offen; BitLocker ist abfragbar.
        let facts = system_facts();
        if !is_elevated() {
            assert_eq!(facts.recovery_environment, Answer::Unknown);
        }
        println!("{facts:?}");
    }

    #[test]
    fn normalizes_targets() {
        assert_eq!(normalize_target("e:\\").unwrap(), "E:");
        assert_eq!(normalize_target(" F: ").unwrap(), "F:");
        assert_eq!(normalize_target("\\\\nas\\backup\\").unwrap(), "\\\\nas\\backup");
        assert_eq!(normalize_target("\\\\nas\\backup\\pc01").unwrap(), "\\\\nas\\backup\\pc01");
        assert!(normalize_target("\\\\nas").is_err());
        assert!(normalize_target("E:\\Ordner").is_err());
        assert!(normalize_target("").is_err());
    }

    #[test]
    fn builds_wbadmin_arguments() {
        let job = ImageJob { target: "E:".into(), include: vec![], login: None, expected_bytes: 0 };
        assert_eq!(build_args(&job), ["start", "backup", "-backupTarget:E:", "-allCritical", "-quiet"]);
        let job = ImageJob {
            target: "\\\\nas\\backup".into(),
            include: vec!["D:".into(), "F:".into()],
            login: Some(("admin".into(), "geheim".into())),
            expected_bytes: 0,
        };
        let args = build_args(&job);
        assert_eq!(
            args,
            [
                "start",
                "backup",
                "-backupTarget:\\\\nas\\backup",
                "-allCritical",
                "-include:D:,F:",
                "-user:admin",
                "-password:geheim",
                "-quiet"
            ]
        );
        // Im Protokoll taucht das Passwort nicht auf.
        let logged = describe_args(&args);
        assert!(logged.contains("-password:***") && !logged.contains("geheim"), "{logged}");
    }

    #[cfg(windows)]
    #[test]
    fn decodes_console_umlauts() {
        // "für Größe" im OEM-Zeichensatz 850 bzw. 437 (dort liegen ü, ö und ß an denselben Stellen).
        let text = decode_console(&[b'f', 0x81, b'r', b' ', b'G', b'r', 0x94, 0xE1, b'e']);
        assert_eq!(text, "für Größe");
    }

    /// Startet das echte `wbadmin` mit einer harmlosen Abfrage und prüft, dass seine Ausgabe
    /// ankommt und lesbar umgewandelt wird (ohne Administratorrechte ist es eine Fehlermeldung).
    #[cfg(windows)]
    #[test]
    fn captures_output_of_the_real_wbadmin() {
        if !wbadmin_path().exists() {
            return;
        }
        let args = ["get".to_string(), "versions".to_string()];
        let mut child = command(&args).spawn().unwrap();
        let (tx, rx) = mpsc::channel();
        forward_lines(child.stdout.take().unwrap(), tx.clone());
        forward_lines(child.stderr.take().unwrap(), tx);
        let status = child.wait().unwrap();
        let lines: Vec<String> = rx.iter().collect();
        println!("Exit-Code {:?}\n{}", status.code(), lines.join("\n"));
        assert!(lines.iter().any(|l| l.to_lowercase().contains("wbadmin")), "{lines:?}");
        // Keine Ersatzzeichen: die Umwandlung aus dem Konsolen-Zeichensatz hat geklappt.
        assert!(!lines.iter().any(|l| l.contains('\u{FFFD}')), "{lines:?}");
    }

    #[test]
    fn refuses_to_run_without_admin_rights() {
        if is_elevated() {
            return; // Als Administrator würde der Test eine echte Sicherung anstoßen.
        }
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let err = run(&ImageJob { target: "E:".into(), include: vec![], login: None, expected_bytes: 0 }, &rep).err().unwrap();
        assert!(err.to_string().contains("Administrator"), "{err}");
    }

    #[test]
    fn finds_a_process_and_reads_how_much_it_wrote() {
        // Der Testlauf selbst dient als Beispielprozess.
        let own_name = std::env::current_exe().unwrap().file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(process_id(&own_name), Some(std::process::id()));
        assert_eq!(process_id("gibt-es-nicht-4711.exe"), None);

        let before = written_bytes(std::process::id()).expect("eigener Schreibzähler lesbar");
        let file = std::env::temp_dir().join(format!("packup-test-io-{}.bin", std::process::id()));
        std::fs::write(&file, vec![7u8; 300_000]).unwrap();
        let after = written_bytes(std::process::id()).unwrap();
        let _ = std::fs::remove_file(file);
        assert!(after >= before + 300_000, "{before} → {after}");

        assert!(is_noise("wbadmin 1.0 - Sicherungs-Befehlszeilentool") && is_noise("(C) Copyright Microsoft Corporation."));
        assert!(!is_noise("Erstellen einer Schattenkopie der für die Sicherung angegebenen Volumes..."));
    }
}
