//! E-Mail-Berichte über SMTP. Gesendet wird ausschließlich verschlüsselt: entweder TLS von
//! Anfang an (üblich auf Port 465) oder STARTTLS (üblich auf Port 587).

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use chrono::Local;

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct Mail {
    pub server: String,
    pub port: u16,
    /// Erst unverschlüsselt verbinden und dann auf TLS umschalten (STARTTLS)
    pub starttls: bool,
    /// Leer = ohne Anmeldung senden
    pub user: String,
    pub password: String,
    pub from: String,
    /// Ein oder mehrere Empfänger, mit Komma oder Semikolon getrennt
    pub to: String,
}

fn b64(data: impl AsRef<[u8]>) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Prüft eine E-Mail-Adresse so weit, dass sie gefahrlos in Befehle und Kopfzeilen passt.
fn address(text: &str) -> Result<String> {
    let address = text.trim();
    let valid = address.contains('@')
        && !address.starts_with('@')
        && !address.ends_with('@')
        && address.chars().all(|c| c.is_ascii_graphic() && !matches!(c, '<' | '>' | ',' | ';' | '"'));
    if !valid {
        bail!(tr!("„{address}“ ist keine gültige E-Mail-Adresse.", "“{address}” is not a valid e-mail address."));
    }
    Ok(address.to_string())
}

fn recipients(text: &str) -> Result<Vec<String>> {
    let list: Vec<String> = text.split([',', ';']).filter(|part| !part.trim().is_empty()).map(address).collect::<Result<_>>()?;
    if list.is_empty() {
        bail!(t!("Bitte mindestens einen Empfänger eintragen.", "Please enter at least one recipient."));
    }
    Ok(list)
}

/// Die fertige Nachricht: Kopfzeilen und Text (UTF-8, als Base64 – so stören weder Umlaute
/// noch lange Zeilen).
fn message(from: &str, to: &[String], subject: &str, body: &str) -> String {
    let subject: String = subject.chars().filter(|c| !c.is_control()).collect();
    let host = from.rsplit('@').next().unwrap_or("localhost");
    let id = format!("{}.{}@{host}", Local::now().timestamp_micros(), std::process::id());
    let mut text = format!(
        "From: ZEBack <{from}>\r\nTo: {}\r\nSubject: =?UTF-8?B?{}?=\r\nDate: {}\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n",
        to.join(", "),
        b64(&subject),
        Local::now().to_rfc2822(),
    );
    let encoded = b64(body.replace("\r\n", "\n").replace('\n', "\r\n"));
    for line in encoded.as_bytes().chunks(76) {
        text.push_str(std::str::from_utf8(line).unwrap_or_default());
        text.push_str("\r\n");
    }
    text
}

/// Eine SMTP-Verbindung: Befehle senden, Antworten lesen.
struct Session<S> {
    stream: S,
}

impl<S: Read + Write> Session<S> {
    /// Liest eine (auch mehrzeilige) Antwort: Statuscode und Text.
    fn reply(&mut self) -> Result<(u16, String)> {
        let mut text = String::new();
        loop {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                if self.stream.read(&mut byte).context(t!("Der Mailserver antwortet nicht", "The mail server does not respond"))? == 0 {
                    bail!(t!("Der Mailserver hat die Verbindung beendet.", "The mail server closed the connection."));
                }
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > 4096 {
                    bail!(t!("Unerwartete Antwort des Mailservers.", "Unexpected response from the mail server."));
                }
            }
            let line = String::from_utf8_lossy(&line).trim_end().to_string();
            let code: u16 = line
                .get(..3)
                .and_then(|c| c.parse().ok())
                .context(t!("Unerwartete Antwort des Mailservers", "Unexpected response from the mail server"))?;
            text.push_str(line.get(4..).unwrap_or_default());
            // "250-…" kündigt weitere Zeilen an, "250 …" ist die letzte.
            if line.as_bytes().get(3) != Some(&b'-') {
                return Ok((code, text));
            }
            text.push('\n');
        }
    }

    /// Sendet einen Befehl und erwartet den Statuscode `expect`. `what` beschreibt den Schritt
    /// für die Fehlermeldung (der Befehl selbst kann ein Passwort enthalten).
    fn command(&mut self, line: &str, expect: u16, what: &str) -> Result<String> {
        self.stream
            .write_all(line.as_bytes())
            .and_then(|_| self.stream.write_all(b"\r\n"))
            .context(t!("Verbindung zum Mailserver verloren", "Lost the connection to the mail server"))?;
        self.stream.flush().ok();
        match self.reply()? {
            (code, text) if code == expect => Ok(text),
            (535 | 534 | 530, _) => bail!(tr!(
                "{what}: Anmeldung abgelehnt – Benutzername und Passwort prüfen",
                "{what}: sign-in rejected – check user name and password"
            )),
            (code, text) => bail!(tr!(
                "{what}: Der Mailserver antwortete mit {code} {}",
                "{what}: the mail server replied with {code} {}",
                text.lines().last().unwrap_or_default()
            )),
        }
    }

    fn greeting(&mut self) -> Result<()> {
        match self.reply()? {
            (220, _) => Ok(()),
            (code, text) => bail!(tr!("Der Mailserver meldet sich mit {code} {text}", "The mail server answers with {code} {text}")),
        }
    }

    /// Begrüßung; die Antwort nennt, was der Server kann (z. B. `AUTH PLAIN LOGIN`).
    fn hello(&mut self) -> Result<String> {
        let name = std::env::var("COMPUTERNAME").unwrap_or_default();
        let name: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        self.command(&format!("EHLO {}", if name.is_empty() { "zeback" } else { name.as_str() }), 250, t!("Begrüßung", "Greeting"))
    }

    /// Anmelden, Absender und Empfänger nennen, Nachricht übergeben.
    fn deliver(&mut self, mail: &Mail, capabilities: &str, to: &[String], message: &str) -> Result<()> {
        if !mail.user.trim().is_empty() {
            let user = mail.user.trim();
            let auth = capabilities.lines().find(|l| l.to_ascii_uppercase().starts_with("AUTH")).unwrap_or_default().to_ascii_uppercase();
            let sign_in = t!("Anmeldung", "Sign-in");
            if auth.contains("PLAIN") || !auth.contains("LOGIN") {
                self.command(&format!("AUTH PLAIN {}", b64(format!("\0{user}\0{}", mail.password))), 235, sign_in)?;
            } else {
                self.command("AUTH LOGIN", 334, sign_in)?;
                self.command(&b64(user), 334, sign_in)?;
                self.command(&b64(&mail.password), 235, sign_in)?;
            }
        }
        self.command(&format!("MAIL FROM:<{}>", address(&mail.from)?), 250, t!("Absender", "Sender"))?;
        for recipient in to {
            self.command(&format!("RCPT TO:<{recipient}>"), 250, t!("Empfänger", "Recipient"))?;
        }
        let hand_over = t!("Nachricht übergeben", "Handing over the message");
        self.command("DATA", 354, hand_over)?;
        // Die Nachricht endet auf CRLF; ein einzelner Punkt in einer Zeile schließt sie ab.
        self.command(&format!("{message}."), 250, hand_over)?;
        let _ = self.command("QUIT", 221, t!("Abmelden", "Sign-out"));
        Ok(())
    }
}

/// Sendet eine E-Mail mit diesem Betreff und Text.
pub fn send(mail: &Mail, subject: &str, body: &str) -> Result<()> {
    let server = mail.server.trim();
    if server.is_empty() || !server.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')) {
        bail!(t!(
            "Bitte den Namen des Mailservers eintragen, z. B. smtp.example.de.",
            "Please enter the name of the mail server, e.g. smtp.example.com."
        ));
    }
    let from = address(&mail.from).context(t!("Absender", "Sender"))?;
    let to = recipients(&mail.to).context(t!("Empfänger", "Recipient"))?;
    let message = message(&from, &to, subject, body);

    let target = (server, mail.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .with_context(|| tr!("Der Mailserver {server} wurde nicht gefunden", "The mail server {server} was not found"))?;
    let tcp = TcpStream::connect_timeout(&target, TIMEOUT).with_context(|| {
        tr!("Der Mailserver {server} ist auf Port {} nicht erreichbar", "The mail server {server} is not reachable on port {}", mail.port)
    })?;
    tcp.set_read_timeout(Some(TIMEOUT)).ok();
    tcp.set_write_timeout(Some(TIMEOUT)).ok();
    let connector = native_tls::TlsConnector::new()
        .context(t!("TLS ist auf diesem PC nicht verfügbar", "TLS is not available on this PC"))?;
    let secure = |stream: TcpStream| {
        connector.connect(server, stream).map_err(|e| {
            anyhow!(tr!(
                "Verschlüsselte Verbindung zum Mailserver {server} fehlgeschlagen ({e})",
                "Encrypted connection to the mail server {server} failed ({e})"
            ))
        })
    };

    let mut session = if mail.starttls {
        let mut plain = Session { stream: tcp };
        plain.greeting()?;
        plain.hello()?;
        plain.command("STARTTLS", 220, t!("Verschlüsselung einschalten", "Switching on encryption"))?;
        Session { stream: secure(plain.stream)? }
    } else {
        let mut session = Session { stream: secure(tcp)? };
        session.greeting()?;
        session
    };
    // Erst auf der verschlüsselten Verbindung wird angemeldet und gesendet.
    let capabilities = session.hello()?;
    session.deliver(mail, &capabilities, &to, &message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    fn mail() -> Mail {
        Mail {
            server: "smtp.example.de".into(),
            port: 465,
            starttls: false,
            user: "berichte@example.de".into(),
            password: "streng geheim".into(),
            from: "berichte@example.de".into(),
            to: "anna@example.de; it@example.de".into(),
        }
    }

    /// Kleiner Mailserver ohne TLS: spricht das Protokoll und merkt sich, was der Client sendet.
    fn start_server(auth_line: &'static str) -> (TcpStream, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut stream = stream;
            let mut seen = Vec::new();
            let mut in_data = false;
            let mut login_step = 0;
            stream.write_all(b"220 test ESMTP\r\n").unwrap();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim_end().to_string();
                seen.push(line.clone());
                let answer = if in_data {
                    if line != "." {
                        continue;
                    }
                    in_data = false;
                    "250 OK".to_string()
                } else if login_step > 0 {
                    login_step -= 1;
                    if login_step == 0 { "235 OK".to_string() } else { "334 UGFzc3dvcmQ6".to_string() }
                } else if line.starts_with("EHLO") {
                    format!("250-test\r\n250-SIZE 1000000\r\n250 {auth_line}")
                } else if line.starts_with("AUTH PLAIN") {
                    "235 OK".to_string()
                } else if line == "AUTH LOGIN" {
                    login_step = 2;
                    "334 VXNlcm5hbWU6".to_string()
                } else if line == "DATA" {
                    in_data = true;
                    "354 Los".to_string()
                } else if line == "QUIT" {
                    stream.write_all(b"221 Bye\r\n").unwrap();
                    break;
                } else if line.starts_with("RCPT TO:<gesperrt@") {
                    "550 Empfaenger unbekannt".to_string()
                } else {
                    "250 OK".to_string()
                };
                stream.write_all(format!("{answer}\r\n").as_bytes()).unwrap();
            }
            seen
        });
        (client, server)
    }

    fn decode(text: &str) -> String {
        String::from_utf8(base64::engine::general_purpose::STANDARD.decode(text).unwrap()).unwrap()
    }

    #[test]
    fn delivers_a_report_with_umlauts() {
        let mail = mail();
        let (client, server) = start_server("AUTH PLAIN LOGIN");
        let mut session = Session { stream: client };
        session.greeting().unwrap();
        let capabilities = session.hello().unwrap();
        let to = recipients(&mail.to).unwrap();
        let text = message("berichte@example.de", &to, "ZEBack: Sicherung „Büro“ gelungen", "Alles in Ordnung.\nGrüße");
        session.deliver(&mail, &capabilities, &to, &text).unwrap();
        let seen = server.join().unwrap();

        let auth = seen.iter().find_map(|l| l.strip_prefix("AUTH PLAIN ")).unwrap();
        assert_eq!(decode(auth), "\0berichte@example.de\0streng geheim");
        assert!(seen.contains(&"MAIL FROM:<berichte@example.de>".to_string()));
        assert!(seen.contains(&"RCPT TO:<anna@example.de>".to_string()) && seen.contains(&"RCPT TO:<it@example.de>".to_string()));
        let subject = seen.iter().find_map(|l| l.strip_prefix("Subject: =?UTF-8?B?")).unwrap().trim_end_matches("?=");
        assert_eq!(decode(subject), "ZEBack: Sicherung „Büro“ gelungen");
        // Der Text steht hinter der Leerzeile, bis zum abschließenden Punkt.
        let blank = seen.iter().position(String::is_empty).unwrap();
        let end = seen.iter().position(|l| l == ".").unwrap();
        assert_eq!(decode(&seen[blank + 1..end].concat()), "Alles in Ordnung.\r\nGrüße");
        assert_eq!(seen.last().map(String::as_str), Some("QUIT"));
    }

    #[test]
    fn falls_back_to_auth_login_and_reports_rejections() {
        let mut mail = mail();
        let (client, server) = start_server("AUTH LOGIN");
        let mut session = Session { stream: client };
        session.greeting().unwrap();
        let capabilities = session.hello().unwrap();
        mail.to = "gesperrt@example.de".into();
        let to = recipients(&mail.to).unwrap();
        let error = session.deliver(&mail, &capabilities, &to, "x\r\n").err().unwrap().to_string();
        assert!(error.contains("Empfänger") && error.contains("550"), "{error}");
        drop(session);
        let seen = server.join().unwrap();
        let login = seen.iter().position(|l| l == "AUTH LOGIN").unwrap();
        assert_eq!((decode(&seen[login + 1]), decode(&seen[login + 2])), ("berichte@example.de".to_string(), "streng geheim".to_string()));
        // Das Passwort taucht in keiner Fehlermeldung auf.
        assert!(!error.contains("geheim"));
    }

    #[test]
    fn refuses_addresses_that_could_smuggle_commands() {
        assert!(address("anna@example.de").is_ok());
        for bad in ["", "anna", "anna@example.de\r\nRCPT TO:<x@y.z>", "a b@example.de", "<anna@example.de>", "@example.de"] {
            assert!(address(bad).is_err(), "{bad}");
        }
        assert_eq!(recipients("a@b.de, c@d.de;").unwrap(), ["a@b.de", "c@d.de"]);
        assert!(recipients(" ; ").is_err());
        assert!(send(&Mail { server: "smtp.example.de/x".into(), ..mail() }, "x", "y").is_err());
    }
}
