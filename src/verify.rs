//! Prüfung einer Sicherung: Die ZIP-Datei wird vollständig gelesen; dabei vergleicht die
//! ZIP-Bibliothek jede Datei mit ihrer gespeicherten Prüfsumme (CRC-32).

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};
use zip::result::ZipError;
use zip::{ZipArchive, ZipReadOptions};

use crate::backup::MANIFEST_NAME;
use crate::job::{Counter, Reporter};
use crate::util::fmt_bytes;

const BUFFER: usize = 1 << 20;

#[derive(Debug)]
pub struct VerifyReport {
    /// Anzahl der geprüften Dateien (ohne die Inhaltsbeschreibung)
    pub files: u64,
    pub bytes: u64,
}

/// Liest alle Dateien der Sicherung und meldet einen Fehler, sobald eine beschädigt ist.
/// Mit `expected` (Anzahl, Bytes) wird zusätzlich geprüft, ob die Sicherung vollständig ist.
/// Verschlüsselte Sicherungen brauchen das `password`; dabei wird jede Datei entschlüsselt
/// und ihre Echtheit geprüft.
pub fn verify_archive(
    path: &Path,
    expected: Option<(u64, u64)>,
    password: Option<&str>,
    rep: &Reporter,
) -> Result<VerifyReport> {
    let file = File::open(path)
        .with_context(|| tr!("Datei kann nicht geöffnet werden: {}", "Cannot open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(BufReader::with_capacity(BUFFER, file))
        .with_context(|| tr!("{} ist nicht als ZIP-Datei lesbar", "{} cannot be read as a ZIP file", path.display()))?;
    let total = archive.decompressed_size().map(|s| s.min(u64::MAX as u128) as u64).unwrap_or(0);

    let mut counter = Counter::new(rep, total);
    let mut buf = vec![0u8; BUFFER];
    let (mut files, mut bytes) = (0u64, 0u64);

    for index in 0..archive.len() {
        rep.check()?;
        let options = ZipReadOptions::new().password(password.map(str::as_bytes));
        let mut entry = match archive.by_index_with_options(index, options) {
            Ok(entry) => entry,
            Err(ZipError::InvalidPassword) => bail!(t!("Das Passwort ist falsch", "The password is wrong")),
            Err(ZipError::UnsupportedArchive(ZipError::PASSWORD_REQUIRED)) => {
                bail!(t!(
                    "Die Sicherung ist verschlüsselt – bitte das Passwort angeben",
                    "The backup is encrypted – please enter the password"
                ))
            }
            Err(e) => bail!(tr!(
                "Eintrag {index} der Sicherung ist beschädigt ({e})",
                "Entry {index} of the backup is damaged ({e})"
            )),
        };
        if !entry.is_file() {
            continue;
        }
        let name =
            entry.name().map(|n| n.into_owned()).unwrap_or_else(|_| tr!("Eintrag {index}", "Entry {index}"));
        let expected_size = entry.size();
        let mut read = 0u64;
        loop {
            rep.check()?;
            match entry.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    read += n as u64;
                    counter.add(n as u64);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Hier landet auch eine falsche Prüfsumme.
                Err(e) => bail!(tr!(
                    "„{name}“ ist in der Sicherung beschädigt ({e})",
                    "“{name}” is damaged in the backup ({e})"
                )),
            }
        }
        if read != expected_size {
            bail!(tr!(
                "„{name}“ ist in der Sicherung unvollständig ({read} statt {expected_size} Bytes)",
                "“{name}” is incomplete in the backup ({read} instead of {expected_size} bytes)"
            ));
        }
        if name != MANIFEST_NAME {
            files += 1;
            bytes += read;
        }
    }
    counter.flush();

    if let Some((expected_files, expected_bytes)) = expected
        && (expected_files != files || expected_bytes != bytes)
    {
        bail!(tr!(
            "Die Sicherung ist unvollständig: {files} Dateien ({}) gefunden, erwartet waren {expected_files} ({})",
            "The backup is incomplete: found {files} files ({}), expected {expected_files} ({})",
            fmt_bytes(bytes),
            fmt_bytes(expected_bytes)
        ));
    }
    Ok(VerifyReport { files, bytes })
}
