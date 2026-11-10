//! Nextcloud-Anbindung über WebDAV: Hochladen (bei großen Dateien in Teilen),
//! Auflisten und Herunterladen von Sicherungen.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chrono::{DateTime, Local};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use ureq::http::{Request, Response};
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use ureq::{Agent, AsSendBody, Body, SendBody};

use crate::job::{Cancelled, Counter, Reporter};
use crate::util::TempFile;

/// Größe eines Upload-Teils. Bleibt unter den üblichen Grenzen von Reverse-Proxys (z. B. 100 MB).
const DEFAULT_CHUNK_SIZE: u64 = 50 * 1024 * 1024;
/// Nextcloud erlaubt höchstens 10 000 Teile pro Upload.
const MAX_CHUNKS: u64 = 9_000;
const ATTEMPTS: u32 = 3;

const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

#[derive(Clone, Debug)]
pub struct NcSettings {
    pub url: String,
    pub user: String,
    pub password: String,
    pub remote_dir: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteFile {
    pub name: String,
    pub size: u64,
    pub modified: Option<DateTime<Local>>,
}

pub struct NcClient {
    agent: Agent,
    base: String,
    auth: String,
    user_id: String,
    chunk_size: u64,
    /// Gewöhnlicher WebDAV-Server statt Nextcloud: Die Adresse zeigt direkt auf den Ordner,
    /// und große Dateien gehen in einem Stück hoch.
    plain: bool,
}

/// Adresse eines gewöhnlichen WebDAV-Ordners: verschlüsselt, ohne Schrägstrich am Ende.
pub fn normalize_webdav_url(input: &str) -> Result<String> {
    let mut url = input.trim().to_string();
    if url.is_empty() {
        bail!(t!("Bitte die WebDAV-Adresse eintragen.", "Please enter the WebDAV address."));
    }
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    while url.ends_with('/') {
        url.pop();
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" {
            bail!(t!(
                "Unverschlüsselte Verbindungen (http://) werden nicht unterstützt. Bitte https:// verwenden.",
                "Unencrypted connections (http://) are not supported. Please use https://."
            ));
        }
    } else if !url.starts_with("https://") {
        bail!(t!("Die WebDAV-Adresse muss mit https:// beginnen.", "The WebDAV address must start with https://."));
    }
    if url.splitn(4, '/').nth(2).unwrap_or_default().is_empty() {
        bail!(t!("Die WebDAV-Adresse ist unvollständig.", "The WebDAV address is incomplete."));
    }
    Ok(url)
}

/// Macht aus der Eingabe des Benutzers die Basisadresse des Servers.
/// Akzeptiert auch eingefügte WebDAV- oder Browser-Adressen.
pub fn normalize_base_url(input: &str) -> Result<String> {
    let mut url = input.trim().to_string();
    if url.is_empty() {
        bail!(t!("Bitte die Adresse des Nextcloud-Servers eintragen.", "Please enter the address of the Nextcloud server."));
    }
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    for marker in ["/remote.php", "/index.php", "/ocs/", "/apps/", "/login"] {
        if let Some(pos) = url.find(marker) {
            url.truncate(pos);
        }
    }
    while url.ends_with('/') {
        url.pop();
    }
    if let Some(rest) = url.strip_prefix("http://") {
        // Unverschlüsselt würde das Passwort im Klartext übertragen – nur lokal zulassen.
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        if host != "localhost" && host != "127.0.0.1" {
            bail!(t!(
                "Unverschlüsselte Verbindungen (http://) werden nicht unterstützt. Bitte https:// verwenden.",
                "Unencrypted connections (http://) are not supported. Please use https://."
            ));
        }
    } else if !url.starts_with("https://") {
        bail!(t!("Die Server-Adresse muss mit https:// beginnen.", "The server address must start with https://."));
    }
    if url.splitn(4, '/').nth(2).unwrap_or_default().is_empty() {
        bail!(t!("Die Server-Adresse ist unvollständig.", "The server address is incomplete."));
    }
    Ok(url)
}

fn encode(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT).to_string()
}

fn dir_segments(remote_dir: &str) -> Vec<&str> {
    remote_dir
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "." && *s != "..")
        .collect()
}

fn status_error(status: u16, action: &str) -> anyhow::Error {
    let reason = match status {
        401 => t!(
            "Anmeldung fehlgeschlagen – Benutzername und App-Passwort prüfen",
            "sign-in failed – check user name and app password"
        )
        .to_string(),
        403 => t!("Zugriff verweigert (403)", "access denied (403)").to_string(),
        404 => t!("nicht gefunden (404) – Adresse bzw. Ordner prüfen", "not found (404) – check address or folder").to_string(),
        413 => t!(
            "die Datei ist für den Server oder einen vorgeschalteten Proxy zu groß (413)",
            "the file is too large for the server or a proxy in front of it (413)"
        )
        .to_string(),
        423 => t!("die Datei ist auf dem Server gesperrt (423)", "the file is locked on the server (423)").to_string(),
        507 => t!(
            "nicht genug Speicherplatz auf dem Server bzw. Kontingent erschöpft (507)",
            "not enough storage space on the server, or quota exhausted (507)"
        )
        .to_string(),
        503 => t!(
            "der Server ist im Wartungsmodus oder überlastet (503)",
            "the server is in maintenance mode or overloaded (503)"
        )
        .to_string(),
        other => tr!("der Server antwortete mit HTTP {other}", "the server replied with HTTP {other}"),
    };
    anyhow!("{action}: {reason}")
}

pub(crate) fn is_retryable(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 504)
}

/// Liest aus einer Datei, zählt den Fortschritt und bricht auf Wunsch ab.
pub(crate) struct ProgressReader<'a, R> {
    pub inner: R,
    pub rep: &'a Reporter,
    pub counter: &'a mut Counter,
}

impl<R: Read> Read for ProgressReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.rep.cancelled() {
            return Err(io::Error::other(Cancelled));
        }
        let n = self.inner.read(buf)?;
        self.counter.add(n as u64);
        Ok(n)
    }
}

pub(crate) fn new_agent() -> Agent {
    Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                // Windows-eigenes TLS inkl. Zertifikatsspeicher (funktioniert auch mit Firmen-CAs).
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .http_status_as_error(false)
        // WebDAV-Methoden wie MKCOL, PROPFIND und MOVE zulassen.
        .allow_non_standard_methods(true)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(900)))
        .user_agent(concat!("ZEBack/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

// ---------------------------------------------------------------------------- Anmeldung im Browser

/// Ergebnis der Anmeldung im Browser: Nextcloud hat ein eigenes App-Passwort für dieses Gerät erstellt.
#[derive(Clone, PartialEq)]
pub struct BrowserLogin {
    pub server: String,
    pub user: String,
    pub app_password: String,
}

impl std::fmt::Debug for BrowserLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Das Passwort soll in keiner Fehlermeldung und keinem Protokoll auftauchen.
        write!(f, "BrowserLogin {{ server: {:?}, user: {:?}, app_password: *** }}", self.server, self.user)
    }
}

/// So lange gilt eine angefangene Anmeldung auf dem Server.
const LOGIN_VALID_FOR: Duration = Duration::from_secs(20 * 60);
const LOGIN_POLL_EVERY: Duration = Duration::from_secs(2);

/// Meldet sich über den Browser an ("Login Flow v2" von Nextcloud): Die App öffnet die
/// Anmeldeseite des Servers und wartet, bis der Zugriff dort bestätigt wurde. Das Kennwort des
/// Benutzers sieht die App dabei nie – sie erhält ein eigenes App-Passwort.
/// `open` öffnet die Anmeldeseite im Browser.
pub fn login_in_browser(url: &str, open: impl FnOnce(&str) -> Result<()>, rep: &Reporter) -> Result<BrowserLogin> {
    login_flow(url, open, rep, LOGIN_POLL_EVERY)
}

fn login_flow(
    url: &str,
    open: impl FnOnce(&str) -> Result<()>,
    rep: &Reporter,
    poll_every: Duration,
) -> Result<BrowserLogin> {
    let base = normalize_base_url(url)?;
    let agent = new_agent();
    // Adressen aus der Antwort des Servers müssen wieder verschlüsselt sein (oder zum lokalen
    // Testserver gehören) – sonst ginge das App-Passwort im Klartext über die Leitung.
    let acceptable = |address: &str| address.starts_with("https://") || (base.starts_with("http://") && address.starts_with(&base));
    let post = |address: &str, body: &str| -> Result<Response<Body>> {
        let request = Request::builder()
            .method("POST")
            .uri(address)
            // Unter diesem Namen erscheint das Gerät in Nextcloud unter "Geräte & Sitzungen".
            .header("User-Agent", device_name())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .context(t!("Ungültige Server-Adresse", "Invalid server address"))?;
        agent.run(request).map_err(|e| anyhow!(tr!("Der Server {base} ist nicht erreichbar ({e})", "The server {base} is not reachable ({e})")))
    };

    rep.status(t!("Anmeldung wird vorbereitet …", "Preparing sign-in …"));
    let mut resp = post(&format!("{base}/index.php/login/v2"), "")?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(status_error(status, t!("Anmeldung im Browser vorbereiten", "Preparing sign-in in the browser")));
    }
    let no_nextcloud = t!(
        "Unter dieser Adresse antwortet kein Nextcloud-Server (oder er ist älter als Version 16)",
        "No Nextcloud server answers at this address (or it is older than version 16)"
    );
    let unreadable = t!("Antwort des Servers nicht lesbar", "Could not read the server's response");
    let text = resp.body_mut().read_to_string().context(unreadable)?;
    let json: serde_json::Value = serde_json::from_str(&text).context(no_nextcloud)?;
    let field = |value: &serde_json::Value| value.as_str().map(str::to_owned).context(no_nextcloud);
    let login_url = field(&json["login"])?;
    let endpoint = field(&json["poll"]["endpoint"])?;
    let token = field(&json["poll"]["token"])?;
    if !acceptable(&login_url) || !acceptable(&endpoint) {
        bail!(t!(
            "Der Server nennt für die Anmeldung eine unverschlüsselte Adresse. Bitte „overwriteprotocol“ in der Nextcloud-Konfiguration prüfen.",
            "The server names an unencrypted address for the sign-in. Please check “overwriteprotocol” in the Nextcloud configuration."
        ));
    }

    open(&login_url).context(t!("Der Browser konnte nicht geöffnet werden", "Could not open the browser"))?;
    rep.status(t!("Warte auf die Bestätigung im Browser …", "Waiting for confirmation in the browser …"));
    rep.log(tr!("Anmeldeseite im Browser geöffnet: {base}", "Sign-in page opened in the browser: {base}"));

    let body = format!("token={}", encode(&token));
    let started = std::time::Instant::now();
    loop {
        // In kleinen Schritten warten, damit "Abbrechen" sofort wirkt.
        let wait_until = std::time::Instant::now() + poll_every;
        while std::time::Instant::now() < wait_until {
            rep.check()?;
            std::thread::sleep(poll_every.min(Duration::from_millis(150)));
        }
        if started.elapsed() > LOGIN_VALID_FOR {
            bail!(t!(
                "Die Anmeldung wurde im Browser nicht innerhalb von 20 Minuten bestätigt. Bitte noch einmal versuchen.",
                "The sign-in was not confirmed in the browser within 20 minutes. Please try again."
            ));
        }
        let mut resp = match post(&endpoint, &body) {
            Ok(resp) => resp,
            // Kurze Aussetzer der Verbindung überstehen; die Zeitgrenze oben beendet das Warten.
            Err(_) => continue,
        };
        match resp.status().as_u16() {
            200 => {
                let text = resp.body_mut().read_to_string().context(unreadable)?;
                let json: serde_json::Value = serde_json::from_str(&text).context(no_nextcloud)?;
                let server =
                    json["server"].as_str().and_then(|s| normalize_base_url(s).ok()).unwrap_or_else(|| base.clone());
                return Ok(BrowserLogin {
                    server,
                    user: field(&json["loginName"])?,
                    app_password: field(&json["appPassword"])?,
                });
            }
            // 404 heißt: im Browser noch nicht bestätigt.
            404 => {}
            status if is_retryable(status) || status == 503 => {}
            status => return Err(status_error(status, t!("Anmeldung im Browser", "Sign-in in the browser"))),
        }
    }
}

/// Name, unter dem dieses Gerät in Nextcloud erscheint (nur ASCII, da HTTP-Kopfzeile).
fn device_name() -> String {
    let computer: String = std::env::var("COMPUTERNAME")
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    if computer.is_empty() { "ZEBack".to_string() } else { format!("ZEBack ({computer})") }
}

impl NcClient {
    /// Baut die Verbindung auf und prüft dabei die Anmeldung.
    pub fn connect(settings: &NcSettings) -> Result<Self> {
        let base = normalize_base_url(&settings.url)?;
        if settings.user.trim().is_empty() || settings.password.is_empty() {
            bail!(t!(
                "Bitte Benutzername und App-Passwort für Nextcloud eintragen.",
                "Please enter user name and app password for Nextcloud."
            ));
        }
        let agent = new_agent();
        let credentials = format!("{}:{}", settings.user.trim(), settings.password);
        let auth = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(credentials));
        let mut client = Self {
            agent,
            base,
            auth,
            user_id: String::new(),
            chunk_size: DEFAULT_CHUNK_SIZE,
            plain: false,
        };
        client.user_id = client.fetch_user_id()?;
        Ok(client)
    }

    /// Verbindet sich mit einem gewöhnlichen WebDAV-Server (kein Nextcloud) und prüft die
    /// Anmeldung. `settings.url` ist die Adresse des WebDAV-Ordners.
    pub fn connect_webdav(settings: &NcSettings) -> Result<Self> {
        let base = normalize_webdav_url(&settings.url)?;
        if settings.user.trim().is_empty() || settings.password.is_empty() {
            bail!(t!("Bitte Benutzername und Passwort für WebDAV eintragen.", "Please enter user name and password for WebDAV."));
        }
        let credentials = format!("{}:{}", settings.user.trim(), settings.password);
        let client = Self {
            agent: new_agent(),
            base,
            auth: format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(credentials)),
            user_id: settings.user.trim().to_string(),
            chunk_size: u64::MAX,
            plain: true,
        };
        let resp = client.send("PROPFIND", &format!("{}/", client.base), &[("Depth", "0")], ())?;
        match resp.status().as_u16() {
            207 | 200 => Ok(client),
            401 => bail!(t!(
                "Anmeldung an WebDAV fehlgeschlagen – Benutzername und Passwort prüfen",
                "WebDAV sign-in failed – check user name and password"
            )),
            404 => bail!(t!("Unter dieser Adresse gibt es keinen WebDAV-Ordner (404)", "There is no WebDAV folder at this address (404)")),
            405 | 501 => bail!(t!("Unter dieser Adresse antwortet kein WebDAV-Server", "No WebDAV server answers at this address")),
            status => Err(status_error(status, t!("Anmeldung an WebDAV", "WebDAV sign-in"))),
        }
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, &str)],
        body: impl AsSendBody,
    ) -> Result<Response<Body>> {
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("Authorization", &self.auth)
            .header("OCS-APIRequest", "true");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(body).context(t!("Ungültige Server-Adresse", "Invalid server address"))?;
        self.agent
            .run(request)
            .map_err(|e| anyhow!(tr!("Der Server {} ist nicht erreichbar ({e})", "The server {} is not reachable ({e})", self.base)))
    }

    /// Die Benutzer-ID kann vom Anmeldenamen abweichen (z. B. Anmeldung per E-Mail oder LDAP).
    fn fetch_user_id(&self) -> Result<String> {
        let url = format!("{}/ocs/v2.php/cloud/user?format=json", self.base);
        let mut resp = self.send("GET", &url, &[], ())?;
        let status = resp.status().as_u16();
        if status != 200 {
            return Err(status_error(status, t!("Anmeldung an Nextcloud", "Nextcloud sign-in")));
        }
        let text = resp
            .body_mut()
            .read_to_string()
            .context(t!("Antwort des Servers nicht lesbar", "Could not read the server's response"))?;
        let no_nextcloud = t!("Unter dieser Adresse antwortet kein Nextcloud-Server", "No Nextcloud server answers at this address");
        let json: serde_json::Value = serde_json::from_str(&text).context(no_nextcloud)?;
        json["ocs"]["data"]["id"].as_str().map(str::to_owned).context(no_nextcloud)
    }

    fn files_url(&self, segments: &[&str]) -> String {
        let mut url = if self.plain {
            self.base.clone()
        } else {
            format!("{}/remote.php/dav/files/{}", self.base, encode(&self.user_id))
        };
        for segment in segments {
            url.push('/');
            url.push_str(&encode(segment));
        }
        url
    }

    /// Legt den Zielordner (auch verschachtelt) an, falls er noch nicht existiert.
    pub fn ensure_dir(&self, remote_dir: &str) -> Result<()> {
        let segments = dir_segments(remote_dir);
        for depth in 1..=segments.len() {
            let resp = self.send("MKCOL", &self.files_url(&segments[..depth]), &[], ())?;
            match resp.status().as_u16() {
                201 | 405 => {} // 405 = Ordner gibt es schon
                status => return Err(status_error(status, t!("Ordner auf Nextcloud anlegen", "Creating the folder on Nextcloud"))),
            }
        }
        Ok(())
    }

    /// Lädt `local` als `name` in den Ordner `remote_dir`. Gibt den Pfad auf dem Server zurück.
    pub fn upload(&self, local: &Path, remote_dir: &str, name: &str, rep: &Reporter) -> Result<String> {
        let size = fs::metadata(local)?.len();
        let mut file = File::open(local)?;
        self.ensure_dir(remote_dir)?;

        let mut segments = dir_segments(remote_dir);
        segments.push(name);
        let destination = self.files_url(&segments);
        let mut counter = Counter::new(rep, size);

        if size <= self.chunk_size {
            self.put_range(&destination, &mut file, 0, size, &[], rep, &mut counter)?;
        } else {
            self.upload_chunked(&destination, &mut file, size, rep, &mut counter)?;
        }
        counter.flush();
        Ok(segments.join("/"))
    }

    fn upload_chunked(
        &self,
        destination: &str,
        file: &mut File,
        size: u64,
        rep: &Reporter,
        counter: &mut Counter,
    ) -> Result<()> {
        let chunk_size = self.chunk_size.max(size.div_ceil(MAX_CHUNKS));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let upload_url = format!(
            "{}/remote.php/dav/uploads/{}/zeback-{}-{}",
            self.base,
            encode(&self.user_id),
            std::process::id(),
            nanos
        );
        let total = size.to_string();
        let headers = [("Destination", destination), ("OC-Total-Length", total.as_str())];

        let resp = self.send("MKCOL", &upload_url, &headers, ())?;
        if resp.status().as_u16() != 201 {
            return Err(status_error(resp.status().as_u16(), t!("Upload vorbereiten", "Preparing the upload")));
        }

        let result = (|| -> Result<()> {
            let mut offset = 0u64;
            let mut index = 1u32;
            while offset < size {
                let len = chunk_size.min(size - offset);
                let chunk_url = format!("{upload_url}/{index:05}");
                self.put_range(&chunk_url, file, offset, len, &headers, rep, counter)?;
                offset += len;
                index += 1;
            }
            rep.status(t!("Nextcloud setzt die Datei zusammen …", "Nextcloud is assembling the file …"));
            let resp = self.send("MOVE", &format!("{upload_url}/.file"), &headers, ())?;
            match resp.status().as_u16() {
                200..=299 => Ok(()),
                status => Err(status_error(status, t!("Upload abschließen", "Completing the upload"))),
            }
        })();

        if result.is_err() {
            // Angefangene Teile auf dem Server wieder entfernen.
            let _ = self.send("DELETE", &upload_url, &[], ());
        }
        result
    }

    /// Sendet `len` Bytes ab `offset` per PUT, bei vorübergehenden Fehlern mit Wiederholung.
    #[allow(clippy::too_many_arguments)]
    fn put_range(
        &self,
        url: &str,
        file: &mut File,
        offset: u64,
        len: u64,
        extra_headers: &[(&str, &str)],
        rep: &Reporter,
        counter: &mut Counter,
    ) -> Result<()> {
        let done_before = counter.done();
        let length = len.to_string();
        let mut headers = vec![
            ("Content-Length", length.as_str()),
            ("Content-Type", "application/octet-stream"),
        ];
        headers.extend_from_slice(extra_headers);

        let mut attempt = 1;
        loop {
            rep.check()?;
            file.seek(SeekFrom::Start(offset))?;
            counter.set(done_before);
            let outcome = {
                let mut reader = ProgressReader { inner: (&mut *file).take(len), rep, counter: &mut *counter };
                self.send("PUT", url, &headers, SendBody::from_reader(&mut reader))
            };
            let error = match outcome {
                Ok(resp) => match resp.status().as_u16() {
                    200..=299 => return Ok(()),
                    status if is_retryable(status) && attempt < ATTEMPTS => status_error(status, "Upload"),
                    status => return Err(status_error(status, "Upload")),
                },
                Err(_) if rep.cancelled() => return Err(Cancelled.into()),
                Err(e) if attempt < ATTEMPTS => e,
                Err(e) => return Err(e.context(t!("Upload fehlgeschlagen", "Upload failed"))),
            };
            rep.log(tr!(
                "Upload-Versuch {attempt} fehlgeschlagen ({error}) – neuer Versuch …",
                "Upload attempt {attempt} failed ({error}) – trying again …"
            ));
            std::thread::sleep(Duration::from_secs(3 * attempt as u64));
            attempt += 1;
        }
    }

    /// Listet die ZIP-Dateien im Sicherungsordner auf, neueste zuerst.
    pub fn list_backups(&self, remote_dir: &str) -> Result<Vec<RemoteFile>> {
        const BODY: &str = r#"<?xml version="1.0"?><d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/><d:getlastmodified/><d:resourcetype/></d:prop></d:propfind>"#;
        let url = format!("{}/", self.files_url(&dir_segments(remote_dir)));
        let mut resp = self.send(
            "PROPFIND",
            &url,
            &[("Depth", "1"), ("Content-Type", "application/xml; charset=utf-8")],
            BODY,
        )?;
        match resp.status().as_u16() {
            207 => {}
            404 => return Ok(Vec::new()),
            status => return Err(status_error(status, t!("Sicherungen auf Nextcloud auflisten", "Listing backups on Nextcloud"))),
        }
        let xml = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .context(t!("Antwort des Servers nicht lesbar", "Could not read the server's response"))?;
        let mut files = parse_multistatus(&xml)?;
        files.retain(|f| f.name.to_ascii_lowercase().ends_with(".zip"));
        files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
        Ok(files)
    }

    /// Löscht eine Datei im Sicherungsordner (für das Aufräumen alter Sicherungen).
    pub fn delete(&self, remote_dir: &str, name: &str) -> Result<()> {
        let mut segments = dir_segments(remote_dir);
        segments.push(name);
        let resp = self.send("DELETE", &self.files_url(&segments), &[], ())?;
        match resp.status().as_u16() {
            200..=299 | 404 => Ok(()),
            status => Err(status_error(status, t!("Alte Sicherung auf Nextcloud löschen", "Deleting an old backup on Nextcloud"))),
        }
    }

    /// Lädt eine Sicherung herunter und speichert sie unter `dest`.
    pub fn download(&self, remote_dir: &str, file: &RemoteFile, dest: &Path, rep: &Reporter) -> Result<()> {
        let mut segments = dir_segments(remote_dir);
        segments.push(&file.name);
        let resp = self.send("GET", &self.files_url(&segments), &[], ())?;
        if resp.status().as_u16() != 200 {
            return Err(status_error(resp.status().as_u16(), t!("Download von Nextcloud", "Download from Nextcloud")));
        }
        let total = resp.body().content_length().unwrap_or(file.size);

        let mut part_name = dest.as_os_str().to_owned();
        part_name.push(".part");
        let mut part = TempFile::new(part_name);
        let mut out = BufWriter::with_capacity(
            1 << 20,
            File::create(part.path())
                .with_context(|| tr!("Datei kann nicht angelegt werden: {}", "Cannot create the file: {}", dest.display()))?,
        );
        let mut counter = Counter::new(rep, total);
        let mut reader = resp.into_body().into_reader();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            rep.check()?;
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(anyhow!(e).context(t!("Download abgebrochen – Verbindung verloren", "Download aborted – connection lost")));
                }
            };
            out.write_all(&buf[..n])
                .with_context(|| tr!("Fehler beim Schreiben von {}", "Error writing {}", dest.display()))?;
            counter.add(n as u64);
        }
        out.flush()?;
        drop(out);
        fs::rename(part.path(), dest)?;
        part.keep();
        counter.flush();
        Ok(())
    }
}

/// Wertet die Antwort auf PROPFIND aus (nur Dateien, keine Unterordner).
fn parse_multistatus(xml: &str) -> Result<Vec<RemoteFile>> {
    const DAV: &str = "DAV:";
    let doc = roxmltree::Document::parse(xml).context(t!("Unerwartete Antwort des Servers", "Unexpected response from the server"))?;
    let mut files = Vec::new();
    for response in doc.descendants().filter(|n| n.has_tag_name((DAV, "response"))) {
        let text_of = |tag: &str| {
            response
                .descendants()
                .find(|n| n.has_tag_name((DAV, tag)))
                .and_then(|n| n.text())
                .map(str::trim)
        };
        if response.descendants().any(|n| n.has_tag_name((DAV, "collection"))) {
            continue;
        }
        let Some(href) = text_of("href") else { continue };
        let Some(raw_name) = href.trim_end_matches('/').rsplit('/').next() else { continue };
        let name = percent_decode_str(raw_name).decode_utf8_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let size = text_of("getcontentlength").and_then(|s| s.parse().ok()).unwrap_or(0);
        let modified = text_of("getlastmodified")
            .and_then(|s| DateTime::parse_from_rfc2822(s).ok())
            .map(|d| d.with_timezone(&Local));
        files.push(RemoteFile { name, size, modified });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    const USER: &str = "anna@example.org";
    const PASSWORD: &str = "test-app-password";
    const USER_ID: &str = "anna";

    /// Sehr kleiner WebDAV-Server, der sich wie Nextcloud verhält (nur das, was der Client nutzt).
    #[derive(Default)]
    struct Mock {
        files: Mutex<BTreeMap<String, Vec<u8>>>,
        dirs: Mutex<BTreeSet<String>>,
        requests: Mutex<Vec<String>>,
        /// Anmeldung im Browser: Zahl der bisherigen Nachfragen und der gemeldete Gerätename
        login_polls: Mutex<u32>,
        login_device: Mutex<String>,
    }

    const LOGIN_TOKEN: &str = "geheimes+token/1";

    impl Mock {
        /// Anmeldung im Browser (Login Flow v2); kommt ohne Anmeldedaten aus.
        fn respond_login(&self, method: &str, path: &str, headers: &HashMap<String, String>, body: &[u8]) -> Option<(u16, Vec<u8>)> {
            let host = headers.get("host").cloned().unwrap_or_default();
            match (method, path) {
                ("POST", "/index.php/login/v2") => {
                    *self.login_device.lock().unwrap() = headers.get("user-agent").cloned().unwrap_or_default();
                    let token = LOGIN_TOKEN.replace('/', "\\/");
                    Some((200, format!(
                        r#"{{"poll":{{"token":"{token}","endpoint":"http:\/\/{host}\/login\/v2\/poll"}},"login":"http:\/\/{host}\/login\/v2\/flow\/abc"}}"#
                    ).into_bytes()))
                }
                ("POST", "/login/v2/poll") => {
                    // Das Token muss URL-kodiert ankommen.
                    if body != b"token=geheimes%2Btoken%2F1" {
                        return Some((400, Vec::new()));
                    }
                    let mut polls = self.login_polls.lock().unwrap();
                    *polls += 1;
                    if *polls < 3 {
                        return Some((404, Vec::new())); // noch nicht bestätigt
                    }
                    Some((200, format!(
                        r#"{{"server":"http:\/\/{host}","loginName":"{USER}","appPassword":"{PASSWORD}"}}"#
                    ).into_bytes()))
                }
                _ => None,
            }
        }

        fn respond(&self, method: &str, path: &str, headers: &HashMap<String, String>, body: Vec<u8>) -> (u16, Vec<u8>) {
            self.requests.lock().unwrap().push(format!("{method} {path}"));
            if let Some(answer) = self.respond_login(method, path, headers, &body) {
                return answer;
            }
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASSWORD}"))
            );
            if headers.get("authorization") != Some(&expected) {
                return (401, Vec::new());
            }
            let destination = headers
                .get("destination")
                .map(|d| d.splitn(4, '/').nth(3).map(|p| format!("/{p}")).unwrap_or_default());
            let mut files = self.files.lock().unwrap();
            let mut dirs = self.dirs.lock().unwrap();
            match method {
                "GET" if path.starts_with("/ocs/v2.php/cloud/user") => {
                    (200, format!(r#"{{"ocs":{{"meta":{{"status":"ok"}},"data":{{"id":"{USER_ID}"}}}}}}"#).into_bytes())
                }
                "MKCOL" => {
                    if dirs.insert(path.to_string()) { (201, Vec::new()) } else { (405, Vec::new()) }
                }
                "PUT" => {
                    files.insert(path.to_string(), body);
                    (201, Vec::new())
                }
                "MOVE" if path.ends_with("/.file") => {
                    let prefix = path.trim_end_matches(".file").to_string();
                    let keys: Vec<String> = files.keys().filter(|k| k.starts_with(&prefix)).cloned().collect();
                    let mut assembled = Vec::new();
                    for key in keys {
                        assembled.extend(files.remove(&key).unwrap());
                    }
                    if headers.get("oc-total-length").map(String::as_str) != Some(assembled.len().to_string().as_str()) {
                        return (400, Vec::new());
                    }
                    dirs.remove(prefix.trim_end_matches('/'));
                    files.insert(destination.unwrap(), assembled);
                    (201, Vec::new())
                }
                "DELETE" => {
                    files.retain(|k, _| !k.starts_with(path));
                    dirs.retain(|k| !k.starts_with(path));
                    (204, Vec::new())
                }
                "PROPFIND" => {
                    let dir = path.trim_end_matches('/');
                    if !dirs.contains(dir) {
                        return (404, Vec::new());
                    }
                    let mut xml = String::from(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns">"#);
                    xml.push_str(&format!("<d:response><d:href>{dir}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"));
                    for (key, data) in files.iter().filter(|(k, _)| k.starts_with(&format!("{dir}/"))) {
                        xml.push_str(&format!(
                            "<d:response><d:href>{key}</d:href><d:propstat><d:prop><d:getcontentlength>{}</d:getcontentlength><d:getlastmodified>Tue, 10 Nov 2026 12:00:00 GMT</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
                            data.len()
                        ));
                    }
                    xml.push_str("</d:multistatus>");
                    (207, xml.into_bytes())
                }
                "GET" => match files.get(path) {
                    Some(data) => (200, data.clone()),
                    None => (404, Vec::new()),
                },
                _ => (400, Vec::new()),
            }
        }
    }

    fn serve(stream: TcpStream, mock: Arc<Mock>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let mut parts = line.split_whitespace();
            let (Some(method), Some(path)) = (parts.next(), parts.next()) else { return };
            let mut headers = HashMap::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    return;
                }
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                }
            }
            assert!(
                !headers.contains_key("transfer-encoding"),
                "Der Client soll Content-Length statt Chunked-Encoding senden"
            );
            let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let (status, payload) = mock.respond(method, path, &headers, body);
            let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n\r\n", payload.len());
            if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&payload).is_err() {
                return;
            }
        }
    }

    fn start_mock() -> (String, Arc<Mock>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let mock = Arc::new(Mock::default());
        let shared = mock.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mock = shared.clone();
                std::thread::spawn(move || serve(stream, mock));
            }
        });
        (url, mock)
    }

    fn settings(url: &str, password: &str) -> NcSettings {
        NcSettings {
            url: url.to_string(),
            user: USER.to_string(),
            password: password.to_string(),
            remote_dir: "Backups/Büro PC".to_string(),
        }
    }

    fn reporter() -> Reporter {
        Reporter::new(|| {}).0
    }

    fn test_data(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("zeback-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn normalizes_urls() {
        assert_eq!(normalize_base_url("cloud.example.org").unwrap(), "https://cloud.example.org");
        assert_eq!(normalize_base_url(" https://cloud.example.org/ ").unwrap(), "https://cloud.example.org");
        assert_eq!(
            normalize_base_url("https://example.org/nextcloud/remote.php/dav/files/anna/").unwrap(),
            "https://example.org/nextcloud"
        );
        assert_eq!(
            normalize_base_url("https://cloud.example.org/index.php/apps/files").unwrap(),
            "https://cloud.example.org"
        );
        assert!(normalize_base_url("http://cloud.example.org").is_err());
        assert!(normalize_base_url("").is_err());
        assert!(normalize_base_url("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn parses_nextcloud_listing() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response><d:href>/remote.php/dav/files/anna/Backups/</d:href>
  <d:propstat><d:prop><d:getlastmodified>Mon, 09 Nov 2026 08:00:00 GMT</d:getlastmodified><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/files/anna/Backups/Backup_PC%20B%c3%bcro_2026-11-09.zip</d:href>
  <d:propstat><d:prop><d:getcontentlength>5368709120</d:getcontentlength><d:getlastmodified>Mon, 09 Nov 2026 08:15:30 GMT</d:getlastmodified><d:resourcetype/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;
        let files = parse_multistatus(xml).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "Backup_PC Büro_2026-11-09.zip");
        assert_eq!(files[0].size, 5_368_709_120);
        assert_eq!(
            files[0].modified.unwrap().to_utc().to_rfc3339(),
            "2026-11-09T08:15:30+00:00"
        );
    }

    #[test]
    fn rejects_wrong_password() {
        let (url, _mock) = start_mock();
        let err = NcClient::connect(&settings(&url, "falsch")).err().unwrap();
        assert!(err.to_string().contains("Anmeldung fehlgeschlagen"), "{err}");
    }

    #[test]
    fn browser_login_waits_for_confirmation_and_returns_app_password() {
        let (url, mock) = start_mock();
        let opened = Mutex::new(String::new());
        let login = login_flow(
            &url,
            |address| {
                *opened.lock().unwrap() = address.to_string();
                Ok(())
            },
            &reporter(),
            Duration::from_millis(20),
        )
        .unwrap();
        assert_eq!(*opened.lock().unwrap(), format!("{url}/login/v2/flow/abc"));
        assert_eq!(login, BrowserLogin { server: url.clone(), user: USER.into(), app_password: PASSWORD.into() });
        assert_eq!(*mock.login_polls.lock().unwrap(), 3);
        assert!(mock.login_device.lock().unwrap().starts_with("ZEBack"));
        assert!(!format!("{login:?}").contains(PASSWORD));

        // Mit dem erhaltenen App-Passwort klappt die normale Anmeldung.
        let settings = NcSettings { url: login.server, user: login.user, password: login.app_password, remote_dir: "Backups".into() };
        assert_eq!(NcClient::connect(&settings).unwrap().user_id(), USER_ID);
    }

    /// Öffnet wirklich einen Tab im Standardbrowser (zeigt dort nur eine leere Seite des
    /// Testservers). Von Hand starten: `cargo test real_browser -- --ignored`.
    #[test]
    #[ignore]
    fn browser_login_opens_the_real_browser() {
        let (url, _mock) = start_mock();
        let login = login_flow(&url, crate::sysimage::open_in_browser, &reporter(), Duration::from_millis(300)).unwrap();
        assert_eq!(login.user, USER);
    }

    #[test]
    fn browser_login_can_be_cancelled_while_waiting() {
        let (url, mock) = start_mock();
        let (rep, _rx, cancel) = Reporter::new(|| {});
        let err = login_flow(
            &url,
            |_| {
                // Der Benutzer bricht ab, sobald der Browser offen ist.
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
            &rep,
            Duration::from_millis(20),
        )
        .err()
        .unwrap();
        assert!(err.is::<Cancelled>(), "{err:#}");
        assert_eq!(*mock.login_polls.lock().unwrap(), 0);
    }

    #[test]
    fn browser_login_needs_a_nextcloud_server() {
        // Ein Server, der auf alles mit 404 antwortet.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 2 {
                    line.clear();
                }
                let _ = stream.write_all(b"HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            }
        });
        let err = login_flow(&url, |_| panic!("Browser darf nicht geöffnet werden"), &reporter(), Duration::from_millis(20))
            .err()
            .unwrap();
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn uploads_lists_and_downloads_small_file() {
        let (url, mock) = start_mock();
        let cfg = settings(&url, PASSWORD);
        let client = NcClient::connect(&cfg).unwrap();
        assert_eq!(client.user_id(), USER_ID);

        let dir = temp_dir("small");
        let data = test_data(70_000);
        let local = dir.join("Backup_klein.zip");
        fs::write(&local, &data).unwrap();

        // Noch kein Ordner vorhanden: leere Liste statt Fehler.
        assert!(client.list_backups(&cfg.remote_dir).unwrap().is_empty());

        let remote = client.upload(&local, &cfg.remote_dir, "Backup_klein.zip", &reporter()).unwrap();
        assert_eq!(remote, "Backups/Büro PC/Backup_klein.zip");
        let key = "/remote.php/dav/files/anna/Backups/B%C3%BCro%20PC/Backup_klein.zip";
        assert_eq!(mock.files.lock().unwrap().get(key), Some(&data));

        let list = client.list_backups(&cfg.remote_dir).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Backup_klein.zip");
        assert_eq!(list[0].size, data.len() as u64);

        let dest = dir.join("heruntergeladen.zip");
        client.download(&cfg.remote_dir, &list[0], &dest, &reporter()).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), data);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn plain_webdav_server_works_without_nextcloud_paths() {
        let (url, mock) = start_mock();
        mock.dirs.lock().unwrap().insert("/dav".to_string());
        let cfg = NcSettings { url: format!("{url}/dav/"), user: USER.into(), password: PASSWORD.into(), remote_dir: "Backups".into() };
        let client = NcClient::connect_webdav(&cfg).unwrap();

        let dir = temp_dir("webdav");
        let data = test_data(5_000);
        let local = dir.join("Backup.zip");
        fs::write(&local, &data).unwrap();
        assert_eq!(client.upload(&local, &cfg.remote_dir, "Backup.zip", &reporter()).unwrap(), "Backups/Backup.zip");
        // Die Datei liegt direkt unter der angegebenen Adresse, in einem Stück hochgeladen.
        assert_eq!(mock.files.lock().unwrap().get("/dav/Backups/Backup.zip"), Some(&data));
        assert!(!mock.requests.lock().unwrap().iter().any(|r| r.contains("remote.php") || r.contains("ocs/")));

        let list = client.list_backups(&cfg.remote_dir).unwrap();
        assert_eq!((list.len(), list[0].size), (1, 5_000));
        client.download(&cfg.remote_dir, &list[0], &dir.join("zurueck.zip"), &reporter()).unwrap();
        assert_eq!(fs::read(dir.join("zurueck.zip")).unwrap(), data);
        client.delete(&cfg.remote_dir, "Backup.zip").unwrap();
        assert!(client.list_backups(&cfg.remote_dir).unwrap().is_empty());

        // Falsches Passwort und falsche Adresse werden verständlich gemeldet.
        let wrong = NcSettings { password: "falsch".into(), ..cfg.clone() };
        assert!(NcClient::connect_webdav(&wrong).err().unwrap().to_string().contains("Anmeldung"));
        let missing = NcSettings { url: format!("{url}/gibt-es-nicht"), ..cfg };
        assert!(NcClient::connect_webdav(&missing).err().unwrap().to_string().contains("404"));
        assert!(normalize_webdav_url("http://server.example.de/dav").is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn uploads_large_file_in_chunks() {
        let (url, mock) = start_mock();
        let cfg = settings(&url, PASSWORD);
        let mut client = NcClient::connect(&cfg).unwrap();
        client.chunk_size = 1000;

        let dir = temp_dir("chunked");
        let data = test_data(3_500);
        let local = dir.join("Backup_gross.zip");
        fs::write(&local, &data).unwrap();

        client.upload(&local, &cfg.remote_dir, "Backup_gross.zip", &reporter()).unwrap();

        let key = "/remote.php/dav/files/anna/Backups/B%C3%BCro%20PC/Backup_gross.zip";
        assert_eq!(mock.files.lock().unwrap().get(key), Some(&data));
        let requests = mock.requests.lock().unwrap();
        let chunk_puts = requests.iter().filter(|r| r.starts_with("PUT /remote.php/dav/uploads/anna/")).count();
        assert_eq!(chunk_puts, 4, "{requests:?}");
        assert!(requests.iter().any(|r| r.starts_with("MOVE ") && r.ends_with("/.file")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancelled_upload_stops_and_cleans_up() {
        let (url, mock) = start_mock();
        let cfg = settings(&url, PASSWORD);
        let mut client = NcClient::connect(&cfg).unwrap();
        client.chunk_size = 1000;

        let dir = temp_dir("cancel");
        let local = dir.join("Backup.zip");
        fs::write(&local, test_data(5_000)).unwrap();

        let (rep, _rx, cancel) = Reporter::new(|| {});
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = client.upload(&local, &cfg.remote_dir, "Backup.zip", &rep).err().unwrap();
        assert!(err.is::<Cancelled>(), "{err:#}");
        assert!(mock.files.lock().unwrap().is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
