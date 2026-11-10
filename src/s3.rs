//! S3-kompatibler Speicher (Amazon S3, MinIO, Wasabi, Backblaze B2 …): Hochladen (große
//! Dateien in Teilen), Auflisten, Herunterladen und Löschen von Sicherungen.
//! Angesprochen wird der Speicher im Pfad-Stil: `https://endpunkt/bucket/ordner/datei.zip`.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use hmac::{Hmac, KeyInit, Mac};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use sha2::{Digest, Sha256};
use ureq::http::{Request, Response};
use ureq::{Agent, AsSendBody, Body, SendBody};

use crate::job::{Cancelled, Counter, Reporter};
use crate::nextcloud::{ProgressReader, RemoteFile, is_retryable, new_agent};
use crate::remote::Remote;
use crate::util::TempFile;

/// Größe eines Upload-Teils; bis zu dieser Größe geht eine Datei in einem Stück hoch.
const DEFAULT_PART_SIZE: u64 = 64 * 1024 * 1024;
/// S3 erlaubt höchstens 10 000 Teile pro Upload.
const MAX_PARTS: u64 = 9_000;
const ATTEMPTS: u32 = 3;
/// SHA-256 eines leeren Inhalts.
const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// Inhalt wird nicht mitsigniert (die Übertragung selbst ist per TLS geschützt).
const UNSIGNED: &str = "UNSIGNED-PAYLOAD";

const URI: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'~');

#[derive(Clone)]
pub struct S3Settings {
    /// Adresse des Dienstes, z. B. `https://s3.eu-central-1.amazonaws.com`
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// Ordner im Bucket
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for S3Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Der Geheimschlüssel soll in keiner Fehlermeldung und keinem Protokoll auftauchen.
        write!(f, "S3Settings {{ endpoint: {:?}, bucket: {:?}, access_key: {:?}, secret_key: *** }}", self.endpoint, self.bucket, self.access_key)
    }
}

pub struct S3Client {
    agent: Agent,
    /// `https://host[:port]`
    origin: String,
    /// `host[:port]`, so wie er in der Kopfzeile "Host" steht
    host: String,
    /// Pfad vor dem Bucket (fast immer leer)
    base_path: String,
    region: String,
    bucket: String,
    /// Ordner im Bucket mit `/` am Ende, oder leer
    prefix: String,
    access_key: String,
    secret_key: String,
    part_size: u64,
}

fn encode(text: &str) -> String {
    utf8_percent_encode(text, URI).to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC nimmt Schlüssel jeder Länge");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Anfrageparameter in der Form, die signiert und gesendet wird: nach Namen sortiert, kodiert.
fn canonical_query(params: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = params.iter().map(|(k, v)| (encode(k), encode(v))).collect();
    pairs.sort();
    pairs.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&")
}

/// Inhalt der Kopfzeile "Authorization" nach AWS Signature Version 4. Signiert werden
/// `host`, `x-amz-content-sha256` und `x-amz-date`. `amz_date` hat die Form `20260131T120000Z`.
#[allow(clippy::too_many_arguments)]
fn authorization(
    access_key: &str,
    secret_key: &str,
    region: &str,
    method: &str,
    host: &str,
    canonical_uri: &str,
    canonical_query: &str,
    payload_hash: &str,
    amz_date: &str,
) -> String {
    const SIGNED_HEADERS: &str = "host;x-amz-content-sha256;x-amz-date";
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/s3/aws4_request");
    let canonical_request = format!(
        "{method}\n{canonical_uri}\n{canonical_query}\nhost:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n\n{SIGNED_HEADERS}\n{payload_hash}"
    );
    let string_to_sign = format!("AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}", sha256_hex(canonical_request.as_bytes()));
    let mut key = hmac(format!("AWS4{secret_key}").as_bytes(), date);
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex(&hmac(&key, &string_to_sign));
    format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={SIGNED_HEADERS}, Signature={signature}")
}

/// Zerlegt die Adresse des Dienstes in Ursprung (`https://host`), Host und Pfad.
fn parse_endpoint(input: &str) -> Result<(String, String, String)> {
    let mut url = input.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        bail!(t!(
            "Bitte die Adresse des S3-Dienstes eintragen, z. B. https://s3.eu-central-1.amazonaws.com.",
            "Please enter the address of the S3 service, e.g. https://s3.eu-central-1.amazonaws.com."
        ));
    }
    if !url.contains("://") {
        url = format!("https://{url}");
    }
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", &url));
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    let name = authority.split(':').next().unwrap_or_default();
    match scheme {
        "https" => {}
        // Unverschlüsselt ginge der Inhalt der Sicherung offen über die Leitung – nur lokal zulassen.
        "http" if name == "localhost" || name == "127.0.0.1" => {}
        "http" => bail!(t!(
            "Unverschlüsselte Verbindungen (http://) werden nicht unterstützt. Bitte https:// verwenden.",
            "Unencrypted connections (http://) are not supported. Please use https://."
        )),
        _ => bail!(t!("Die Adresse des S3-Dienstes muss mit https:// beginnen.", "The address of the S3 service must start with https://.")),
    }
    if name.is_empty() {
        bail!(t!("Die Adresse des S3-Dienstes ist unvollständig.", "The address of the S3 service is incomplete."));
    }
    // Der Standard-Port gehört nicht in die signierte Kopfzeile "Host".
    let default_port = if scheme == "https" { ":443" } else { ":80" };
    let host = authority.strip_suffix(default_port).unwrap_or(authority).to_ascii_lowercase();
    Ok((format!("{scheme}://{host}"), host, path))
}

/// Fehlermeldung aus Statuscode und der XML-Antwort des Dienstes.
fn s3_error(status: u16, action: &str, body: &str) -> anyhow::Error {
    let tag = |name: &str| -> Option<&str> {
        let start = body.find(&format!("<{name}>"))? + name.len() + 2;
        let end = body[start..].find(&format!("</{name}>"))? + start;
        Some(&body[start..end])
    };
    let reason = match (status, tag("Code")) {
        (_, Some("SignatureDoesNotMatch")) => {
            t!("der Geheimschlüssel passt nicht (Signatur abgelehnt)", "the secret key does not match (signature rejected)").to_string()
        }
        (_, Some("InvalidAccessKeyId")) => {
            t!("den Zugangsschlüssel kennt der Dienst nicht", "the service does not know the access key").to_string()
        }
        (_, Some("NoSuchBucket")) => t!("den Bucket gibt es nicht", "the bucket does not exist").to_string(),
        (_, Some("AuthorizationHeaderMalformed")) | (301, _) => {
            t!("die Region stimmt nicht mit der des Buckets überein", "the region does not match the bucket's region").to_string()
        }
        (403, _) => t!(
            "Zugriff verweigert – Zugangsschlüssel, Geheimschlüssel und Rechte am Bucket prüfen",
            "access denied – check access key, secret key and permissions on the bucket"
        )
        .to_string(),
        (404, _) => t!("nicht gefunden (404) – Adresse und Bucket prüfen", "not found (404) – check address and bucket").to_string(),
        (_, Some(code)) => format!("{code} (HTTP {status})"),
        _ => tr!("der Dienst antwortete mit HTTP {status}", "the service replied with HTTP {status}"),
    };
    anyhow!("{action}: {reason}")
}

impl S3Client {
    /// Baut die Verbindung auf und prüft dabei Zugangsdaten und Bucket.
    pub fn connect(settings: &S3Settings) -> Result<Self> {
        let (origin, host, base_path) = parse_endpoint(&settings.endpoint)?;
        let bucket = settings.bucket.trim().to_string();
        if bucket.is_empty() || bucket.contains('/') {
            bail!(t!("Bitte den Namen des Buckets eintragen (ohne Schrägstriche).", "Please enter the name of the bucket (without slashes)."));
        }
        if settings.access_key.trim().is_empty() || settings.secret_key.is_empty() {
            bail!(t!("Bitte Zugangsschlüssel und Geheimschlüssel eintragen.", "Please enter access key and secret key."));
        }
        let folders: Vec<&str> =
            settings.prefix.split(['/', '\\']).map(str::trim).filter(|s| !s.is_empty() && *s != "." && *s != "..").collect();
        let prefix = if folders.is_empty() { String::new() } else { format!("{}/", folders.join("/")) };
        let region = match settings.region.trim() {
            "" => "us-east-1".to_string(),
            region => region.to_string(),
        };
        let client = Self {
            agent: new_agent(),
            origin,
            host,
            base_path,
            region,
            bucket,
            prefix,
            access_key: settings.access_key.trim().to_string(),
            secret_key: settings.secret_key.clone(),
            part_size: DEFAULT_PART_SIZE,
        };
        let mut resp = client.send("GET", None, &[("list-type", "2"), ("max-keys", "1")], &[], EMPTY_HASH, ())?;
        match resp.status().as_u16() {
            200 => Ok(client),
            status => Err(s3_error(
                status,
                t!("Anmeldung am S3-Speicher", "Signing in to the S3 storage"),
                &resp.body_mut().read_to_string().unwrap_or_default(),
            )),
        }
    }

    /// Pfad einer Datei im Bucket (`key` = Name im Bucket) bzw. des Buckets selbst.
    fn path(&self, key: Option<&str>) -> String {
        let mut path = format!("{}/{}", self.base_path, encode(&self.bucket));
        if let Some(key) = key {
            for segment in key.split('/') {
                path.push('/');
                path.push_str(&encode(segment));
            }
        }
        path
    }

    fn send(
        &self,
        method: &str,
        key: Option<&str>,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        payload_hash: &str,
        body: impl AsSendBody,
    ) -> Result<Response<Body>> {
        let path = self.path(key);
        let query = canonical_query(query);
        let amz_date = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let auth = authorization(
            &self.access_key,
            &self.secret_key,
            &self.region,
            method,
            &self.host,
            &path,
            &query,
            payload_hash,
            &amz_date,
        );
        let url = if query.is_empty() { format!("{}{path}", self.origin) } else { format!("{}{path}?{query}", self.origin) };
        let mut builder = Request::builder()
            .method(method)
            .uri(url)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", payload_hash)
            .header("Authorization", auth);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder.body(body).context(t!("Ungültige Adresse des S3-Dienstes", "Invalid address of the S3 service"))?;
        self.agent
            .run(request)
            .map_err(|e| anyhow!(tr!("Der S3-Dienst {} ist nicht erreichbar ({e})", "The S3 service {} is not reachable ({e})", self.origin)))
    }

    /// Erwartet eine Antwort mit 2xx und gibt ihren Text zurück.
    fn expect_ok(mut resp: Response<Body>, action: &str) -> Result<String> {
        let status = resp.status().as_u16();
        let text = resp.body_mut().read_to_string().unwrap_or_default();
        if (200..300).contains(&status) { Ok(text) } else { Err(s3_error(status, action, &text)) }
    }

    /// Sendet `len` Bytes ab `offset` per PUT, bei vorübergehenden Fehlern mit Wiederholung.
    /// Gibt die Kennung (ETag) zurück, die der Dienst für das Hochgeladene vergibt.
    #[allow(clippy::too_many_arguments)]
    fn put(
        &self,
        key: &str,
        query: &[(&str, &str)],
        file: &mut File,
        offset: u64,
        len: u64,
        rep: &Reporter,
        counter: &mut Counter,
    ) -> Result<String> {
        let done_before = counter.done();
        let length = len.to_string();
        let headers = [("Content-Length", length.as_str()), ("Content-Type", "application/zip")];
        let mut attempt = 1;
        loop {
            rep.check()?;
            file.seek(SeekFrom::Start(offset))?;
            counter.set(done_before);
            let outcome = {
                let mut reader = ProgressReader { inner: (&mut *file).take(len), rep, counter: &mut *counter };
                self.send("PUT", Some(key), query, &headers, UNSIGNED, SendBody::from_reader(&mut reader))
            };
            let error = match outcome {
                Ok(mut resp) => {
                    let status = resp.status().as_u16();
                    if (200..300).contains(&status) {
                        let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).unwrap_or_default();
                        return Ok(etag.to_string());
                    }
                    let error = s3_error(status, "Upload", &resp.body_mut().read_to_string().unwrap_or_default());
                    if !(is_retryable(status) || status == 503) || attempt >= ATTEMPTS {
                        return Err(error);
                    }
                    error
                }
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

    fn upload_in_parts(&self, key: &str, file: &mut File, size: u64, rep: &Reporter, counter: &mut Counter) -> Result<()> {
        let part_size = self.part_size.max(size.div_ceil(MAX_PARTS));
        // Leerer Text statt "kein Inhalt": so sendet der Client "Content-Length: 0".
        let started = self.send("POST", Some(key), &[("uploads", "")], &[], EMPTY_HASH, "")?;
        let answer = Self::expect_ok(started, t!("Upload vorbereiten", "Preparing the upload"))?;
        let doc = roxmltree::Document::parse(&answer)
            .context(t!("Unerwartete Antwort des S3-Dienstes", "Unexpected response from the S3 service"))?;
        let upload_id = doc
            .descendants()
            .find(|n| n.tag_name().name() == "UploadId")
            .and_then(|n| n.text())
            .context(t!("Der S3-Dienst hat keine Upload-Kennung geliefert", "The S3 service did not return an upload ID"))?
            .to_string();

        let result = (|| -> Result<()> {
            let mut parts = String::from("<CompleteMultipartUpload>");
            let (mut offset, mut number) = (0u64, 1u32);
            while offset < size {
                let len = part_size.min(size - offset);
                let index = number.to_string();
                let etag = self.put(key, &[("partNumber", &index), ("uploadId", &upload_id)], file, offset, len, rep, counter)?;
                parts.push_str(&format!("<Part><PartNumber>{number}</PartNumber><ETag>{etag}</ETag></Part>"));
                offset += len;
                number += 1;
            }
            parts.push_str("</CompleteMultipartUpload>");
            rep.status(t!("Der S3-Dienst setzt die Datei zusammen …", "The S3 service is assembling the file …"));
            let done = self.send(
                "POST",
                Some(key),
                &[("uploadId", &upload_id)],
                &[("Content-Type", "application/xml")],
                &sha256_hex(parts.as_bytes()),
                parts.as_str(),
            )?;
            // Der Dienst kann auch mit Status 200 einen Fehler im Text melden.
            let completing = t!("Upload abschließen", "Completing the upload");
            let answer = Self::expect_ok(done, completing)?;
            if answer.contains("<Error>") {
                return Err(s3_error(200, completing, &answer));
            }
            Ok(())
        })();
        if result.is_err() {
            // Angefangene Teile auf dem Server wieder entfernen.
            let _ = self.send("DELETE", Some(key), &[("uploadId", &upload_id)], &[], EMPTY_HASH, ());
        }
        result
    }
}

impl Remote for S3Client {
    fn upload(&self, local: &Path, name: &str, rep: &Reporter) -> Result<String> {
        let size = fs::metadata(local)?.len();
        let mut file = File::open(local)?;
        let key = format!("{}{name}", self.prefix);
        let mut counter = Counter::new(rep, size);
        if size <= self.part_size {
            self.put(&key, &[], &mut file, 0, size, rep, &mut counter)?;
        } else {
            self.upload_in_parts(&key, &mut file, size, rep, &mut counter)?;
        }
        counter.flush();
        Ok(format!("{}/{key}", self.bucket))
    }

    fn list(&self) -> Result<Vec<RemoteFile>> {
        let mut files = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("delimiter", "/"), ("prefix", self.prefix.as_str())];
            if let Some(token) = &token {
                query.push(("continuation-token", token.as_str()));
            }
            let resp = self.send("GET", None, &query, &[], EMPTY_HASH, ())?;
            let text = Self::expect_ok(resp, t!("Sicherungen im S3-Speicher auflisten", "Listing backups in the S3 storage"))?;
            let doc = roxmltree::Document::parse(&text)
                .context(t!("Unerwartete Antwort des S3-Dienstes", "Unexpected response from the S3 service"))?;
            let text_of = |node: roxmltree::Node, tag: &str| {
                node.children().find(|n| n.tag_name().name() == tag).and_then(|n| n.text()).map(str::to_string)
            };
            for item in doc.descendants().filter(|n| n.tag_name().name() == "Contents") {
                let Some(key) = text_of(item, "Key") else { continue };
                let Some(name) = key.strip_prefix(&self.prefix) else { continue };
                if name.is_empty() || name.contains('/') || !name.to_ascii_lowercase().ends_with(".zip") {
                    continue;
                }
                files.push(RemoteFile {
                    name: name.to_string(),
                    size: text_of(item, "Size").and_then(|s| s.parse().ok()).unwrap_or(0),
                    modified: text_of(item, "LastModified")
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|d| d.with_timezone(&Local)),
                });
            }
            let root = doc.root_element();
            token = match text_of(root, "IsTruncated").as_deref() {
                Some("true") => text_of(root, "NextContinuationToken"),
                _ => None,
            };
            if token.is_none() {
                break;
            }
        }
        files.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.name.cmp(&a.name)));
        Ok(files)
    }

    fn delete(&self, name: &str) -> Result<()> {
        let key = format!("{}{name}", self.prefix);
        let mut resp = self.send("DELETE", Some(&key), &[], &[], EMPTY_HASH, ())?;
        match resp.status().as_u16() {
            200..=299 | 404 => Ok(()),
            status => Err(s3_error(
                status,
                t!("Alte Sicherung im S3-Speicher löschen", "Deleting an old backup in the S3 storage"),
                &resp.body_mut().read_to_string().unwrap_or_default(),
            )),
        }
    }

    fn download(&self, file: &RemoteFile, dest: &Path, rep: &Reporter) -> Result<()> {
        let key = format!("{}{}", self.prefix, file.name);
        let mut resp = self.send("GET", Some(&key), &[], &[], EMPTY_HASH, ())?;
        if resp.status().as_u16() != 200 {
            let status = resp.status().as_u16();
            return Err(s3_error(
                status,
                t!("Download aus dem S3-Speicher", "Download from the S3 storage"),
                &resp.body_mut().read_to_string().unwrap_or_default(),
            ));
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
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(anyhow!(e).context(t!("Download abgebrochen – Verbindung verloren", "Download aborted – connection lost")));
                }
            };
            out.write_all(&buf[..n]).with_context(|| tr!("Fehler beim Schreiben von {}", "Error writing {}", dest.display()))?;
            counter.add(n as u64);
        }
        out.flush()?;
        drop(out);
        fs::rename(part.path(), dest)?;
        part.keep();
        counter.flush();
        Ok(())
    }

    fn describe(&self) -> String {
        tr!(
            "Bucket „{}“ auf {} ist erreichbar (Region {}).",
            "Bucket “{}” on {} is reachable (region {}).",
            self.bucket,
            self.host,
            self.region
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    const ACCESS: &str = "AKIAIOSFODNN7EXAMPLE";
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    /// Die Beispiele aus der AWS-Dokumentation zu Signature Version 4.
    #[test]
    fn signs_like_the_aws_examples() {
        let sign = |query: &[(&str, &str)]| {
            authorization(ACCESS, SECRET, "us-east-1", "GET", "examplebucket.s3.amazonaws.com", "/", &canonical_query(query), EMPTY_HASH, "20130524T000000Z")
        };
        let lifecycle = sign(&[("lifecycle", "")]);
        assert!(lifecycle.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="), "{lifecycle}");
        assert!(lifecycle.ends_with("Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543"), "{lifecycle}");
        let list = sign(&[("prefix", "J"), ("max-keys", "2")]);
        assert!(list.ends_with("Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"), "{list}");
        assert_eq!(sha256_hex(b""), EMPTY_HASH);
    }

    #[test]
    fn reads_endpoints() {
        let parsed = |input: &str| parse_endpoint(input).unwrap();
        assert_eq!(parsed("s3.eu-central-1.amazonaws.com"), ("https://s3.eu-central-1.amazonaws.com".into(), "s3.eu-central-1.amazonaws.com".into(), String::new()));
        assert_eq!(parsed("https://Speicher.example.de:443/"), ("https://speicher.example.de".into(), "speicher.example.de".into(), String::new()));
        assert_eq!(parsed("https://nas.example.de:9000/s3"), ("https://nas.example.de:9000".into(), "nas.example.de:9000".into(), "/s3".into()));
        assert!(parse_endpoint("http://speicher.example.de").is_err());
        assert!(parse_endpoint("").is_err());
        assert_eq!(canonical_query(&[("prefix", "Büro PC/"), ("delimiter", "/")]), "delimiter=%2F&prefix=B%C3%BCro%20PC%2F");
    }

    /// Sehr kleiner S3-Dienst (nur das, was der Client nutzt).
    #[derive(Default)]
    struct Mock {
        objects: Mutex<BTreeMap<String, Vec<u8>>>,
        parts: Mutex<BTreeMap<u32, Vec<u8>>>,
        requests: Mutex<Vec<String>>,
    }

    impl Mock {
        fn respond(&self, method: &str, target: &str, headers: &HashMap<String, String>, body: Vec<u8>) -> (u16, String, Vec<u8>) {
            self.requests.lock().unwrap().push(format!("{method} {target}"));
            let signed = headers.get("authorization").is_some_and(|a| a.starts_with(&format!("AWS4-HMAC-SHA256 Credential={ACCESS}/")))
                && headers.contains_key("x-amz-date")
                && headers.contains_key("x-amz-content-sha256");
            if !signed {
                return (403, String::new(), b"<Error><Code>InvalidAccessKeyId</Code></Error>".to_vec());
            }
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let mut objects = self.objects.lock().unwrap();
            let mut parts = self.parts.lock().unwrap();
            match method {
                "GET" if path == "/sicherungen" => {
                    let mut xml = String::from("<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>false</IsTruncated>");
                    for (key, data) in objects.iter() {
                        let key = key.trim_start_matches("/sicherungen/").replace("%20", " ").replace("%C3%BC", "ü");
                        xml.push_str(&format!("<Contents><Key>{key}</Key><LastModified>2026-11-10T12:00:00.000Z</LastModified><Size>{}</Size></Contents>", data.len()));
                    }
                    xml.push_str("</ListBucketResult>");
                    (200, String::new(), xml.into_bytes())
                }
                "POST" if query == "uploads=" => {
                    parts.clear();
                    (200, String::new(), b"<InitiateMultipartUploadResult><UploadId>upload-1</UploadId></InitiateMultipartUploadResult>".to_vec())
                }
                "PUT" if query.contains("uploadId=upload-1") => {
                    let number: u32 = query.split('&').find_map(|p| p.strip_prefix("partNumber=")).unwrap().parse().unwrap();
                    parts.insert(number, body);
                    (200, format!("ETag: \"teil-{number}\"\r\n"), Vec::new())
                }
                "POST" if query == "uploadId=upload-1" => {
                    let listed = String::from_utf8_lossy(&body).to_string();
                    assert!(listed.contains("<PartNumber>1</PartNumber><ETag>\"teil-1\"</ETag>"), "{listed}");
                    let assembled: Vec<u8> = std::mem::take(&mut *parts).into_values().flatten().collect();
                    objects.insert(path.to_string(), assembled);
                    (200, String::new(), b"<CompleteMultipartUploadResult/>".to_vec())
                }
                "PUT" => {
                    objects.insert(path.to_string(), body);
                    (200, "ETag: \"ganz\"\r\n".to_string(), Vec::new())
                }
                "GET" => match objects.get(path) {
                    Some(data) => (200, String::new(), data.clone()),
                    None => (404, String::new(), b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                },
                "DELETE" => {
                    objects.remove(path);
                    (204, String::new(), Vec::new())
                }
                _ => (400, String::new(), Vec::new()),
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
            let mut words = line.split_whitespace();
            let (Some(method), Some(target)) = (words.next(), words.next()) else { return };
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
            assert!(!headers.contains_key("transfer-encoding"), "Der Client soll Content-Length senden");
            let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            let mut body = vec![0u8; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let (status, extra, payload) = mock.respond(method, target, &headers, body);
            let head = format!("HTTP/1.1 {status} X\r\n{extra}Content-Length: {}\r\n\r\n", payload.len());
            if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&payload).is_err() {
                return;
            }
        }
    }

    fn start_mock() -> (S3Settings, Arc<Mock>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let mock = Arc::new(Mock::default());
        let shared = mock.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mock = shared.clone();
                std::thread::spawn(move || serve(stream, mock));
            }
        });
        let settings = S3Settings {
            endpoint,
            region: String::new(),
            bucket: "sicherungen".into(),
            prefix: "Büro PC".into(),
            access_key: ACCESS.into(),
            secret_key: SECRET.into(),
        };
        (settings, mock)
    }

    #[test]
    fn uploads_lists_downloads_and_deletes() {
        let (settings, mock) = start_mock();
        let mut client = S3Client::connect(&settings).unwrap();
        client.part_size = 1000;
        let (rep, _rx, _cancel) = Reporter::new(|| {});
        let dir = std::env::temp_dir().join(format!("zeback-test-s3-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        // Kleine Datei in einem Stück, große in Teilen.
        let small: Vec<u8> = (0..700).map(|i| (i % 251) as u8).collect();
        let large: Vec<u8> = (0..3500).map(|i| (i * 7 % 253) as u8).collect();
        fs::write(dir.join("klein.zip"), &small).unwrap();
        fs::write(dir.join("gross.zip"), &large).unwrap();
        assert_eq!(client.upload(&dir.join("klein.zip"), "klein.zip", &rep).unwrap(), "sicherungen/Büro PC/klein.zip");
        client.upload(&dir.join("gross.zip"), "gross.zip", &rep).unwrap();
        {
            let objects = mock.objects.lock().unwrap();
            assert_eq!(objects.get("/sicherungen/B%C3%BCro%20PC/klein.zip"), Some(&small));
            assert_eq!(objects.get("/sicherungen/B%C3%BCro%20PC/gross.zip"), Some(&large));
            let requests = mock.requests.lock().unwrap();
            assert_eq!(requests.iter().filter(|r| r.contains("partNumber=")).count(), 4, "{requests:?}");
        }

        let mut names: Vec<String> = client.list().unwrap().into_iter().map(|f| f.name).collect();
        names.sort();
        assert_eq!(names, ["gross.zip", "klein.zip"]);

        let file = RemoteFile { name: "gross.zip".into(), size: large.len() as u64, modified: None };
        client.download(&file, &dir.join("zurueck.zip"), &rep).unwrap();
        assert_eq!(fs::read(dir.join("zurueck.zip")).unwrap(), large);

        client.delete("klein.zip").unwrap();
        assert_eq!(client.list().unwrap().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn explains_rejected_credentials() {
        let (mut settings, _mock) = start_mock();
        settings.access_key = "FALSCH".into();
        let error = S3Client::connect(&settings).err().unwrap().to_string();
        assert!(error.contains("Zugangsschlüssel"), "{error}");
        assert!(!format!("{settings:?}").contains(SECRET));
    }
}
