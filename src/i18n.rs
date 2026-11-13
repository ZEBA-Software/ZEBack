//! Sprache der Oberfläche: Deutsch oder Englisch.
//!
//! Texte stehen an Ort und Stelle in beiden Sprachen:
//! - `t!("Sichern", "Back up")` für feste Texte (ergibt `&'static str`),
//! - `tr!("{n} Dateien", "{n} files")` für Texte mit Platzhaltern (ergibt `String`, wie `format!`).
//!
//! Das Programm startet auf Englisch, solange niemand Deutsch gewählt hat (`Config::language`).
//! Nur solange `apply` noch nicht aufgerufen wurde, liefern die Makros Deutsch – so laufen die Tests.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::config::Language;

static ENGLISH: AtomicBool = AtomicBool::new(false);

/// Ist die Oberfläche gerade englisch?
pub fn english() -> bool {
    ENGLISH.load(Ordering::Relaxed)
}

/// Stellt die Sprache ein. "Auto" aus älteren Einstellungen richtet sich nach der Anzeigesprache
/// von Windows.
pub fn apply(language: Language) {
    let english = match language {
        Language::German => false,
        Language::English => true,
        Language::Auto => !system_is_german(),
    };
    ENGLISH.store(english, Ordering::Relaxed);
}

#[cfg(windows)]
fn system_is_german() -> bool {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    const LANG_GERMAN: u16 = 0x07;
    // SAFETY: Der Aufruf hat keine Parameter und keine Vorbedingungen.
    let language = unsafe { GetUserDefaultUILanguage() };
    // Die unteren zehn Bit nennen die Sprache, der Rest das Land.
    language & 0x3ff == LANG_GERMAN
}

#[cfg(not(windows))]
fn system_is_german() -> bool {
    true
}

/// Fester Text in der eingestellten Sprache: `t!("Deutsch", "English")`.
#[macro_export]
macro_rules! t {
    ($de:expr, $en:expr $(,)?) => {
        if $crate::i18n::english() { $en } else { $de }
    };
}

/// Text mit Platzhaltern in der eingestellten Sprache, wie `format!`:
/// `tr!("{n} Dateien", "{n} files")` oder `tr!("{} von {}", "{} of {}", a, b)`.
#[macro_export]
macro_rules! tr {
    ($de:literal, $en:literal $(, $($arg:tt)*)?) => {
        if $crate::i18n::english() { format!($en $(, $($arg)*)?) } else { format!($de $(, $($arg)*)?) }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn without_apply_texts_are_german_and_placeholders_work() {
        let n = 3;
        assert_eq!(t!("Sichern", "Back up"), "Sichern");
        assert_eq!(tr!("{n} Dateien", "{n} files"), "3 Dateien");
        assert_eq!(tr!("{} von {}", "{} of {}", 1, n), "1 von 3");
    }
}
