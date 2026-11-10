//! Update-Hinweis: fragt beim Hersteller nach der neuesten Version. Heruntergeladen oder
//! installiert wird nichts – das Programm zeigt nur an, dass es eine neuere Version gibt.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// Hier liegt die Angabe zur neuesten Version, als kleine JSON-Datei:
/// `{ "version": "1.2.3", "url": "https://zeba.xyz/zeback", "notes": "Was neu ist" }`
/// (`url` und `notes` dürfen fehlen).
pub const VERSION_URL: &str = "https://zeba.xyz/zeback/version.json";
/// Seite, die geöffnet wird, wenn die Angabe keinen eigenen Link enthält.
pub const DOWNLOAD_URL: &str = "https://zeba.xyz";
/// Nur Links auf diese Website werden aus der Angabe übernommen.
const TRUSTED_PREFIX: &str = "https://zeba.xyz/";

/// Eine neuere Version, die zum Herunterladen bereitsteht.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// Seite mit dem Download
    pub url: String,
    /// Kurze Beschreibung der Neuerungen (kann leer sein)
    pub notes: String,
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

/// Liest die Angabe zur neuesten Version; `None`, wenn sie nicht neuer ist als `current`.
fn parse(text: &str, current: &str) -> Result<Option<Release>> {
    let json: serde_json::Value =
        serde_json::from_str(text).context(t!("Die Antwort ist keine gültige Versionsangabe", "The answer is not a valid version file"))?;
    let version = json["version"]
        .as_str()
        .map(|v| v.trim().trim_start_matches(['v', 'V']).to_string())
        // Nur Ziffern und Punkte: Die Angabe wird angezeigt und darf nichts anderes einschleusen.
        .filter(|v| !v.is_empty() && v.len() <= 20 && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .context(t!("In der Antwort fehlt die Versionsnummer", "The answer contains no version number"))?;
    if !is_newer(&version, current) {
        return Ok(None);
    }
    let url = json["url"]
        .as_str()
        .map(str::trim)
        .filter(|url| url.starts_with(TRUSTED_PREFIX) && !url.chars().any(|c| c.is_control() || c == ' ' || c == '"'))
        .unwrap_or(DOWNLOAD_URL)
        .to_string();
    let notes: String = json["notes"].as_str().unwrap_or_default().chars().filter(|c| !c.is_control()).take(300).collect();
    Ok(Some(Release { version, url, notes }))
}

/// Fragt unter `url` nach und vergleicht mit `current`.
fn check_at(url: &str, current: &str) -> Result<Option<Release>> {
    let agent: Agent = Agent::config_builder()
        .tls_config(TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build())
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(concat!("ZEBack/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!(tr!("Die Update-Abfrage ist fehlgeschlagen ({e})", "The update check failed ({e})")))?;
    let status = response.status().as_u16();
    if status != 200 {
        return Err(anyhow!(tr!(
            "Die Update-Abfrage ist fehlgeschlagen (HTTP {status})",
            "The update check failed (HTTP {status})"
        )));
    }
    let text = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_string()
        .context(t!("Die Antwort auf die Update-Abfrage ist nicht lesbar", "The answer to the update check is not readable"))?;
    parse(&text, current)
}

/// Fragt beim Hersteller nach einer neueren Version als der laufenden.
pub fn check() -> Result<Option<Release>> {
    check_at(VERSION_URL, env!("CARGO_PKG_VERSION"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

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
    fn reads_the_version_file() {
        let newer = parse(r#"{"version":"v2.0.0","url":"https://zeba.xyz/zeback/download","notes":"Neu: alles.\nZweite Zeile"}"#, "1.9.0").unwrap().unwrap();
        assert_eq!((newer.version.as_str(), newer.url.as_str()), ("2.0.0", "https://zeba.xyz/zeback/download"));
        assert_eq!(newer.notes, "Neu: alles.Zweite Zeile");
        // Gleich alt oder älter: kein Hinweis.
        assert_eq!(parse(r#"{"version":"1.9.0"}"#, "1.9.0").unwrap(), None);
        // Links auf fremde Seiten werden nicht übernommen.
        for url in ["https://example.com/zeback.exe", "http://zeba.xyz/x", "https://zeba.xyz.evil.example/x", "file:///C:/x.exe"] {
            let release = parse(&format!(r#"{{"version":"9.0","url":"{url}"}}"#), "1.0").unwrap().unwrap();
            assert_eq!(release.url, DOWNLOAD_URL, "{url}");
        }
        // Keine oder eine merkwürdige Versionsnummer ist ein Fehler, kein Hinweis.
        assert!(parse(r#"{"neu":true}"#, "1.0").is_err());
        assert!(parse(r#"{"version":"2.0 <b>jetzt laden</b>"}"#, "1.0").is_err());
        assert!(parse("<html>Seite nicht gefunden</html>", "1.0").is_err());
    }

    /// Fragt wirklich bei zeba.xyz nach und zeigt die Antwort.
    /// Von Hand starten: `cargo test real_update_server -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn asks_the_real_update_server() {
        println!("Antwort von {VERSION_URL}: {:?}", check().map_err(|e| format!("{e:#}")));
    }

    #[test]
    fn asks_a_server_and_reports_failures() {
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
                let (status, body) = if request.contains("/version.json") { (200, r#"{"version":"3.1.0"}"#) } else { (404, "nicht da") };
                let _ = write!(stream, "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        let found = check_at(&format!("{base}/version.json"), "3.0.9").unwrap().unwrap();
        assert_eq!((found.version.as_str(), found.url.as_str()), ("3.1.0", DOWNLOAD_URL));
        assert_eq!(check_at(&format!("{base}/version.json"), "3.1.0").unwrap(), None);
        assert!(check_at(&format!("{base}/fehlt.json"), "1.0").err().unwrap().to_string().contains("404"));
    }
}
