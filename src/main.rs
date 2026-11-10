// Im Release-Build kein Konsolenfenster öffnen.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// Zuerst, damit die Makros `t!` und `tr!` in allen Modulen bekannt sind.
#[macro_use]
mod i18n;

mod app;
mod backup;
mod config;
mod disk;
mod job;
mod jobs;
mod logo;
mod mail;
mod nextcloud;
mod notify;
mod profiles;
mod rate;
mod remote;
mod restore;
mod s3;
mod schedule;
mod sftp;
mod sizes;
mod smb;
mod sysimage;
mod theme;
mod tray;
mod update;
mod util;
mod verify;
mod widgets;

use eframe::egui;

/// Kennung hinter `--run-job`, falls das Programm ohne Fenster einen Auftrag ausführen soll.
fn job_to_run(args: &[String]) -> Option<Result<u64, String>> {
    let position = args.iter().position(|arg| arg == "--run-job")?;
    Some(match args.get(position + 1).map(|value| value.parse::<u64>()) {
        Some(Ok(id)) => Ok(id),
        _ => Err(t!(
            "Hinter --run-job fehlt die Kennung des Auftrags, z. B. --run-job 1.",
            "--run-job must be followed by the job number, e.g. --run-job 1."
        )
        .to_string()),
    })
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Von OpenSSH als Hilfsprogramm für die Passwortabfrage gestartet (SFTP-Ziele):
    // Antwort ausgeben und sofort beenden, kein Fenster.
    if let Some(answer) = sftp::answer_askpass(&args) {
        use std::io::Write as _;
        std::process::exit(match answer {
            Ok(secret) => {
                let mut out = std::io::stdout();
                let _ = writeln!(out, "{secret}");
                let _ = out.flush();
                0
            }
            Err(()) => 1,
        });
    }
    // Erster Start unter neuem Namen: Einstellungen, Verlauf und Stände des Vorgängers übernehmen.
    config::Config::adopt_previous();
    i18n::apply(config::Config::load().language);
    match job_to_run(&args) {
        Some(Ok(id)) => std::process::exit(jobs::run_headless(id)),
        Some(Err(message)) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
        None => {}
    }

    prefer_frugal_graphics();
    const ICON_SIZE: u32 = 64;
    let icon = egui::IconData {
        rgba: logo::render_rgba(ICON_SIZE),
        width: ICON_SIZE,
        height: ICON_SIZE,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZEBack")
            .with_icon(icon)
            .with_inner_size(start_size())
            // Die Oberfläche passt sich an: schmale Fenster bekommen eine schmale Seitenleiste
            // und eine Spalte statt zwei.
            .with_min_inner_size([560.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ZEBack",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}

/// Fenstergröße beim Start. Entwicklungs-Builds: `ZEBACK_SIZE=700x560` wählt eine andere
/// (für Bildschirmfotos schmaler Fenster).
fn start_size() -> [f32; 2] {
    #[cfg(debug_assertions)]
    if let Ok(size) = std::env::var("ZEBACK_SIZE")
        && let Some((width, height)) = size.split_once('x')
        && let (Ok(width), Ok(height)) = (width.parse(), height.parse())
    {
        return [width, height];
    }
    [1080.0, 800.0]
}

/// Die Oberfläche ist schlicht und braucht keine starke Grafikkarte. Ohne diese Vorgaben würde
/// die Grafikbibliothek auf Notebooks die Hochleistungs-Grafikkarte aufwecken (mehr Strom,
/// Einblendungen des Grafiktreibers) und unter Umständen über Vulkan zeichnen, wo sich Zusatz-
/// programme der Treiber einklinken. Wer etwas anderes braucht, setzt die Variablen selbst.
fn prefer_frugal_graphics() {
    for (name, value) in [("WGPU_POWER_PREF", "low"), ("WGPU_BACKEND", "dx12,gl")] {
        if std::env::var_os(name).is_none() {
            // SAFETY: Aufruf am Anfang von `main`, bevor das Programm weitere Threads startet.
            unsafe { std::env::set_var(name, value) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::job_to_run;

    #[test]
    fn reads_the_job_from_the_command_line() {
        let args = |list: &[&str]| list.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(job_to_run(&args(&["--mode=drive"])), None);
        assert_eq!(job_to_run(&args(&["--run-job", "12"])), Some(Ok(12)));
        assert!(matches!(job_to_run(&args(&["--run-job"])), Some(Err(_))));
        assert!(matches!(job_to_run(&args(&["--run-job", "abc"])), Some(Err(_))));
    }
}
