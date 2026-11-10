# ZEBack

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
- Aufträge ohne Fenster starten: `ZEBack.exe --run-job <Nummer>`

Passwörter liegen ausschließlich in der Windows-Anmeldeinformationsverwaltung, nie in einer Datei.

## Bauen

Voraussetzung: [Rust](https://rustup.rs) mit den Zielen `x86_64-pc-windows-msvc` und
`aarch64-pc-windows-msvc`.

```powershell
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target aarch64-pc-windows-msvc
```

Die fertige Datei liegt danach unter `target\<Ziel>\release\ZEBack.exe`. Sie braucht keine
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

### Veröffentlichen (Updates)

ZEBack fragt beim Start bei GitHub nach der neuesten Veröffentlichung dieses Repositorys und
bietet an, sie herunterzuladen und zu installieren. Damit das funktioniert, braucht jede neue
Version ein Release:

1. Versionsnummer in `Cargo.toml` erhöhen, committen und hochladen.
2. Installer bauen: `powershell -File .\installer\build-msi.ps1`
3. Auf GitHub ein Release mit der Marke `v<Version>` anlegen (z. B. `v0.16.0`) und die beiden
   Dateien `ZEBack-<Version>-x64.msi` und `ZEBack-<Version>-arm64.msi` aus `dist\` anhängen.
   Die Dateinamen müssen genau so bleiben – daran erkennt ZEBack den passenden Installer.

Der Text des Releases erscheint im Programm als Beschreibung der Neuerungen. Heruntergeladene
Installer werden gegen die Prüfsumme geprüft, die GitHub zu jeder Datei angibt.

## Aufbau

| Datei | Inhalt |
|---|---|
| `src/app.rs` | Oberfläche (egui) |
| `src/backup.rs`, `src/restore.rs`, `src/verify.rs` | Sichern, Wiederherstellen, Prüfen |
| `src/jobs.rs`, `src/schedule.rs` | Aufträge, Zeitplan, Aufräumen, Lauf ohne Fenster |
| `src/sysimage.rs` | Systemabbild über `wbadmin` |
| `src/nextcloud.rs`, `src/s3.rs`, `src/sftp.rs`, `src/smb.rs`, `src/remote.rs` | Sicherungsziele im Netz |
| `src/mail.rs`, `src/notify.rs`, `src/update.rs` | Berichte und Update-Hinweis |
| `src/config.rs` | Einstellungen (`%APPDATA%\ZEBack`) |
| `src/i18n.rs` | Sprachumschaltung; Texte stehen mit `t!` / `tr!` in beiden Sprachen an Ort und Stelle |
| `src/theme.rs`, `src/widgets.rs`, `src/logo.rs` | Aussehen, Bedienelemente, Logo |
| `installer/` | WiX-Quelle und Bauskript für die MSI |

## Lizenz

ZEBack ist Open Source unter der [Apache-Lizenz 2.0](LICENSE).

Du darfst das Programm nutzen, verändern und weitergeben, auch in eigenen Projekten. Dabei gilt:

- **Der Hinweis auf ZEBA Software UG bleibt erhalten.** Wer ZEBack oder eine veränderte Fassung
  weitergibt, muss die Datei [NOTICE](NOTICE) und die Urheberhinweise mitliefern und Änderungen
  kenntlich machen.
- **Die Namen gehören nicht zur Lizenz.** „ZEBack“ und „ZEBA Software“ dürfen nicht als Name einer
  veränderten Fassung verwendet werden – nur, um auf die Herkunft hinzuweisen.

## Schrift

Die Oberfläche verwendet [JetBrains Mono](https://www.jetbrains.com/lp/mono/) unter der
SIL Open Font License 1.1 (`assets/fonts/OFL.txt`).
