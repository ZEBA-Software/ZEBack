//! Windows-Benachrichtigung (unten rechts bzw. in der Mitteilungszentrale) über ein
//! vorübergehendes Symbol im Infobereich – ohne zusätzliche Bibliotheken.

use std::time::Duration;

/// So lange bleibt das Symbol (und damit die Benachrichtigung) bestehen.
const VISIBLE_FOR: Duration = Duration::from_secs(8);

#[cfg(windows)]
pub(crate) mod ffi {
    #[repr(C)]
    pub struct NotifyIconData {
        pub size: u32,
        pub window: isize,
        pub id: u32,
        pub flags: u32,
        pub callback_message: u32,
        pub icon: isize,
        pub tip: [u16; 128],
        pub state: u32,
        pub state_mask: u32,
        pub info: [u16; 256],
        pub timeout_or_version: u32,
        pub info_title: [u16; 64],
        pub info_flags: u32,
        pub guid: [u8; 16],
        pub balloon_icon: isize,
    }

    #[link(name = "shell32")]
    unsafe extern "system" {
        pub fn Shell_NotifyIconW(message: u32, data: *const NotifyIconData) -> i32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        #[allow(clippy::too_many_arguments)]
        pub fn CreateWindowExW(
            ex_style: u32,
            class: *const u16,
            title: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: isize,
            menu: isize,
            instance: isize,
            param: *const core::ffi::c_void,
        ) -> isize;
        pub fn DestroyWindow(window: isize) -> i32;
        pub fn LoadIconW(instance: isize, name: *const u16) -> isize;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetModuleHandleW(name: *const u16) -> isize;
    }
}

/// Kopiert Text in ein Feld fester Länge (abgeschnitten, immer nullterminiert).
#[cfg(windows)]
fn fill<const N: usize>(text: &str) -> [u16; N] {
    let mut field = [0u16; N];
    for (slot, unit) in field.iter_mut().take(N - 1).zip(text.encode_utf16()) {
        *slot = unit;
    }
    field
}

/// Zeigt eine Benachrichtigung und kehrt zurück, wenn sie wieder entfernt ist (einige Sekunden).
/// Rückgabe: Hat Windows die Benachrichtigung angenommen?
#[cfg(windows)]
pub fn show_blocking(title: &str, text: &str, error: bool) -> bool {
    const NIM_ADD: u32 = 0;
    const NIM_DELETE: u32 = 2;
    const NIF_ICON: u32 = 0x2;
    const NIF_TIP: u32 = 0x4;
    const NIF_INFO: u32 = 0x10;
    const NIIF_INFO: u32 = 0x1;
    const NIIF_ERROR: u32 = 0x3;
    const IDI_APPLICATION: usize = 32512;

    let class: Vec<u16> = "STATIC".encode_utf16().chain(Some(0)).collect();
    let name: Vec<u16> = "ZEBack".encode_utf16().chain(Some(0)).collect();
    // SAFETY: Alle Zeichenketten sind nullterminiert und leben bis zum Ende der Aufrufe; die
    // Struktur hat die von Windows erwartete Größe und Anordnung.
    unsafe {
        let module = ffi::GetModuleHandleW(std::ptr::null());
        // Unsichtbares Fenster, dem das Symbol gehört.
        let window = ffi::CreateWindowExW(0, class.as_ptr(), name.as_ptr(), 0, 0, 0, 0, 0, 0, 0, module, std::ptr::null());
        if window == 0 {
            return false;
        }
        // Das Programmsymbol ist als Ressource 1 eingebettet; sonst das Standardsymbol.
        let mut icon = ffi::LoadIconW(module, 1 as *const u16);
        if icon == 0 {
            icon = ffi::LoadIconW(0, IDI_APPLICATION as *const u16);
        }
        let data = ffi::NotifyIconData {
            size: size_of::<ffi::NotifyIconData>() as u32,
            window,
            id: 1,
            flags: NIF_ICON | NIF_TIP | NIF_INFO,
            callback_message: 0,
            icon,
            tip: fill("ZEBack"),
            state: 0,
            state_mask: 0,
            info: fill(text),
            timeout_or_version: 0,
            info_title: fill(title),
            info_flags: if error { NIIF_ERROR } else { NIIF_INFO },
            guid: [0; 16],
            balloon_icon: 0,
        };
        let shown = ffi::Shell_NotifyIconW(NIM_ADD, &data) != 0;
        if shown {
            std::thread::sleep(VISIBLE_FOR);
            ffi::Shell_NotifyIconW(NIM_DELETE, &data);
        }
        ffi::DestroyWindow(window);
        shown
    }
}

#[cfg(not(windows))]
pub fn show_blocking(_title: &str, _text: &str, _error: bool) -> bool {
    let _ = VISIBLE_FOR;
    false
}

/// Zeigt eine Benachrichtigung, ohne zu warten (für die Oberfläche).
pub fn show(title: &str, text: &str, error: bool) {
    let (title, text) = (title.to_string(), text.to_string());
    std::thread::spawn(move || show_blocking(&title, &text, error));
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn structure_matches_what_windows_expects() {
        // Größe von NOTIFYICONDATAW auf 64-Bit-Windows
        assert_eq!(size_of::<ffi::NotifyIconData>(), 976);
        let field: [u16; 4] = fill("Sicherung");
        assert_eq!(String::from_utf16_lossy(&field), "Sic\0");
    }

    /// Zeigt wirklich eine Benachrichtigung an. Von Hand starten: `cargo test real_toast -- --ignored`.
    #[test]
    #[ignore]
    fn shows_a_real_toast() {
        assert!(show_blocking("ZEBack – Test", "So sieht die Benachrichtigung nach einem Auftrag aus.", false));
    }
}
