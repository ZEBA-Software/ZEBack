# Packup

Backup- und Wiederherstellungsprogramm für Windows, geschrieben in Rust.
Ein Produkt der ZEBA Software UG.

Die Oberfläche gibt es auf Deutsch und Englisch.

## Was es kann

**Sichern**

- Benutzerprofile (Standardordner, Browser, Mail, Office und eigene Ordner) als ZIP-Datei
- Ganze Laufwerke als ZIP-Datei
- Startfähige Systemabbilder über die Windows-Abbildsicherung (`wbadmin`)
- Verschlüsselung mit AES-256, Prüfung der fertigen Sicherung
- Aufträge: gespeicherte Sicherungen mit Zeitplan (Windows-Aufgabenplanung), Aufräumen alter
  Sicherungen und „Nur Geändertes sichern“ (Teilsicherungen)

**Sicherungsziele** – mehrere gleichzeitig möglich

- Ordner, USB-Stick, externe Festplatte
- Netzwerkfreigabe (SMB)
- Nextcloud (mit Anmeldung über den Browser), WebDAV
- S3-kompatibler Speicher
- SFTP (über den OpenSSH-Client von Windows)

**Wiederherstellen**

- In ein Benutzerprofil oder einen frei gewählten Ordner
- Ganze Ordner oder einzelne Dateien
- Ketten aus Voll- und Teilsicherungen in einem Zug
- Suche nach Sicherungen auf allen Zielen

**Außerdem**

- Berichte per Windows-Benachrichtigung und E-Mail
- Update-Hinweis
- Aufträge ohne Fenster starten: `Packup.exe --run-job <Nummer>`

Passwörter liegen ausschließlich in der Windows-Anmeldeinformationsverwaltung, nie in einer Datei.

## Bauen

Voraussetzung: [Rust](https://rustup.rs) mit den Zielen `x86_64-pc-windows-msvc` und
`aarch64-pc-windows-msvc`.

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

Die fertige Datei liegt danach unter `target\<Ziel>\release\Packup.exe`. Sie braucht keine
weiteren Dateien.

### Tests

```powershell
cargo test
```

Einige Tests greifen wirklich ins System ein (Aufgabenplanung, Benachrichtigung, Browser) und
laufen deshalb nur auf Wunsch: `cargo test -- --ignored`.

### Installer (MSI)

Voraussetzung: das Binärpaket des WiX Toolset 3.14 im Ordner `tools\wix314`.

```powershell
powershell -File .\installer\build-msi.ps1
```

Das Skript baut beide Architekturen und legt EXE und MSI in `dist\` ab. Mit `-SkipCargo` werden
nur die Installer aus den vorhandenen EXE-Dateien erzeugt. Die Versionsnummer kommt aus
`Cargo.toml`.

## Aufbau

| Datei | Inhalt |
|---|---|
| `src/app.rs` | Oberfläche (egui) |
| `src/backup.rs`, `src/restore.rs`, `src/verify.rs` | Sichern, Wiederherstellen, Prüfen |
| `src/jobs.rs`, `src/schedule.rs` | Aufträge, Zeitplan, Aufräumen, Lauf ohne Fenster |
| `src/sysimage.rs` | Systemabbild über `wbadmin` |
| `src/nextcloud.rs`, `src/s3.rs`, `src/sftp.rs`, `src/smb.rs`, `src/remote.rs` | Sicherungsziele im Netz |
| `src/mail.rs`, `src/notify.rs`, `src/update.rs` | Berichte und Update-Hinweis |
| `src/config.rs` | Einstellungen (`%APPDATA%\Packup`) |
| `src/i18n.rs` | Sprachumschaltung; Texte stehen mit `t!` / `tr!` in beiden Sprachen an Ort und Stelle |
| `src/theme.rs`, `src/widgets.rs`, `src/logo.rs` | Aussehen, Bedienelemente, Logo |
| `installer/` | WiX-Quelle und Bauskript für die MSI |

## Schrift

Die Oberfläche verwendet [JetBrains Mono](https://www.jetbrains.com/lp/mono/) unter der
SIL Open Font License 1.1 (`assets/fonts/OFL.txt`).
