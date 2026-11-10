//! Gemeinsame Schnittstelle für alle Ziele, die über das Netz angesprochen werden:
//! Nextcloud, gewöhnliches WebDAV und S3-kompatibler Speicher.

use std::path::Path;

use anyhow::Result;

use crate::job::Reporter;
use crate::nextcloud::{NcClient, NcSettings, RemoteFile};
use crate::s3::{S3Client, S3Settings};
use crate::sftp::{SftpClient, SftpSettings};

/// Zugangsdaten eines Cloud-Ziels.
#[derive(Clone, Debug)]
pub enum RemoteSettings {
    Nextcloud(NcSettings),
    WebDav(NcSettings),
    S3(S3Settings),
    Sftp(SftpSettings),
}

/// Ein verbundenes Cloud-Ziel; arbeitet immer im Sicherungsordner des Ziels.
pub trait Remote {
    /// Lädt die Datei hoch und gibt zurück, wo sie auf dem Server liegt.
    fn upload(&self, local: &Path, name: &str, rep: &Reporter) -> Result<String>;
    /// ZIP-Dateien im Sicherungsordner, neueste zuerst.
    fn list(&self) -> Result<Vec<RemoteFile>>;
    fn delete(&self, name: &str) -> Result<()>;
    fn download(&self, file: &RemoteFile, dest: &Path, rep: &Reporter) -> Result<()>;
    /// Ein Satz für "Ziel prüfen", z. B. `Angemeldet als „anna“ an https://cloud.example.de.`
    fn describe(&self) -> String;
}

impl RemoteSettings {
    /// Baut die Verbindung auf und prüft dabei die Anmeldung.
    pub fn connect(&self) -> Result<Box<dyn Remote>> {
        Ok(match self {
            RemoteSettings::Nextcloud(settings) => {
                Box::new(Dav { client: NcClient::connect(settings)?, dir: settings.remote_dir.clone() })
            }
            RemoteSettings::WebDav(settings) => {
                Box::new(Dav { client: NcClient::connect_webdav(settings)?, dir: settings.remote_dir.clone() })
            }
            RemoteSettings::S3(settings) => Box::new(S3Client::connect(settings)?),
            RemoteSettings::Sftp(settings) => Box::new(SftpClient::connect(settings)?),
        })
    }
}

/// Nextcloud und gewöhnliches WebDAV.
struct Dav {
    client: NcClient,
    dir: String,
}

impl Remote for Dav {
    fn upload(&self, local: &Path, name: &str, rep: &Reporter) -> Result<String> {
        self.client.upload(local, &self.dir, name, rep)
    }

    fn list(&self) -> Result<Vec<RemoteFile>> {
        self.client.list_backups(&self.dir)
    }

    fn delete(&self, name: &str) -> Result<()> {
        self.client.delete(&self.dir, name)
    }

    fn download(&self, file: &RemoteFile, dest: &Path, rep: &Reporter) -> Result<()> {
        self.client.download(&self.dir, file, dest, rep)
    }

    fn describe(&self) -> String {
        tr!("Angemeldet als „{}“ an {}.", "Signed in as “{}” to {}.", self.client.user_id(), self.client.base_url())
    }
}
