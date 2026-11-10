//! Infobereich (neben der Uhr): Während ein Vorgang läuft, lässt sich das Fenster dorthin
//! verstecken. Ein Symbol bleibt sichtbar; ein Klick darauf holt das Fenster zurück.
//!
//! Das Verstecken geschieht direkt über Windows, an der Oberflächen-Bibliothek vorbei – so
//! funktioniert das Zurückholen auch dann, wenn das versteckte Fenster nicht mehr gezeichnet wird.

/// Text für die Meldung am Symbol, wenn der Vorgang fertig ist, während das Fenster versteckt ist.
pub struct Done<'a> {
    pub title: &'a str,
    pub text: &'a str,
    pub error: bool,
}

#[cfg(windows)]
mod imp {
    use std::sync::atomic::{AtomicIsize, Ordering};

    use super::Done;
    use crate::notify::ffi::{CreateWindowExW, DestroyWindow, GetModuleHandleW, LoadIconW, NotifyIconData, Shell_NotifyIconW};

    /// Das Hauptfenster, solange es versteckt ist; sonst 0.
    static MAIN_WINDOW: AtomicIsize = AtomicIsize::new(0);
    /// Das unsichtbare Fenster, dem das Symbol gehört; 0 = kein Symbol.
    static TRAY_WINDOW: AtomicIsize = AtomicIsize::new(0);

    const CLASS_NAME: &str = "ZEBackTray";
    /// Nachricht, mit der Windows Klicks auf das Symbol meldet (WM_APP + 1).
    const CALLBACK: u32 = 0x8001;
    const WM_LBUTTONUP: u32 = 0x0202;
    const WM_LBUTTONDBLCLK: u32 = 0x0203;
    const WM_RBUTTONUP: u32 = 0x0205;
    /// Klick auf die Meldung am Symbol
    const NIN_BALLOONUSERCLICK: u32 = 0x0405;
    const NIM_ADD: u32 = 0;
    const NIM_MODIFY: u32 = 1;
    const NIM_DELETE: u32 = 2;
    const NIF_MESSAGE: u32 = 0x1;
    const NIF_ICON: u32 = 0x2;
    const NIF_TIP: u32 = 0x4;
    const NIF_INFO: u32 = 0x10;
    const SW_HIDE: i32 = 0;
    const SW_SHOW: i32 = 5;
    const SW_RESTORE: i32 = 9;
    /// Übergeordnetes "Fenster" für Fenster, die nur Nachrichten empfangen
    const HWND_MESSAGE: isize = -3;

    type WndProc = unsafe extern "system" fn(isize, u32, usize, isize) -> isize;

    #[repr(C)]
    struct WndClass {
        style: u32,
        wnd_proc: Option<WndProc>,
        class_extra: i32,
        window_extra: i32,
        instance: isize,
        icon: isize,
        cursor: isize,
        background: isize,
        menu_name: *const u16,
        class_name: *const u16,
    }

    #[repr(C)]
    struct Msg {
        window: isize,
        message: u32,
        wparam: usize,
        lparam: isize,
        time: u32,
        x: i32,
        y: i32,
        private: u32,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn FindWindowExW(parent: isize, after: isize, class: *const u16, title: *const u16) -> isize;
        fn GetWindowThreadProcessId(window: isize, process_id: *mut u32) -> u32;
        fn ShowWindow(window: isize, command: i32) -> i32;
        fn SetForegroundWindow(window: isize) -> i32;
        fn IsWindowVisible(window: isize) -> i32;
        fn RegisterClassW(class: *const WndClass) -> u16;
        fn DefWindowProcW(window: isize, message: u32, wparam: usize, lparam: isize) -> isize;
        fn GetMessageW(message: *mut Msg, window: isize, min: u32, max: u32) -> i32;
        fn TranslateMessage(message: *const Msg) -> i32;
        fn DispatchMessageW(message: *const Msg) -> isize;
        fn PostQuitMessage(code: i32);
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcessId() -> u32;
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    fn fill<const N: usize>(text: &str) -> [u16; N] {
        let mut field = [0u16; N];
        for (slot, unit) in field.iter_mut().take(N - 1).zip(text.encode_utf16()) {
            *slot = unit;
        }
        field
    }

    /// Angaben zum Symbol; `flags` bestimmt, welche Felder gelten.
    fn icon_data(tray: isize, flags: u32, tip: &str, title: &str, info: &str, error: bool) -> NotifyIconData {
        const IDI_APPLICATION: usize = 32512;
        // SAFETY: Abfragen ohne Vorbedingungen; ein fehlendes Symbol ergibt 0 und wird ersetzt.
        let icon = unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            match LoadIconW(module, 1 as *const u16) {
                0 => LoadIconW(0, IDI_APPLICATION as *const u16),
                icon => icon,
            }
        };
        NotifyIconData {
            size: size_of::<NotifyIconData>() as u32,
            window: tray,
            id: 1,
            flags,
            callback_message: CALLBACK,
            icon,
            tip: fill(tip),
            state: 0,
            state_mask: 0,
            info: fill(info),
            timeout_or_version: 0,
            info_title: fill(title),
            info_flags: if error { 0x3 } else { 0x1 },
            guid: [0; 16],
            balloon_icon: 0,
        }
    }

    /// Das sichtbare Hauptfenster dieses Prozesses mit dem Titel `title`.
    fn find_main_window(title: &str) -> Option<isize> {
        let title = wide(title);
        let mut window = 0;
        // SAFETY: Der Titel ist nullterminiert; die Schleife endet, wenn Windows 0 liefert.
        unsafe {
            loop {
                window = FindWindowExW(0, window, std::ptr::null(), title.as_ptr());
                if window == 0 {
                    return None;
                }
                let mut process = 0u32;
                GetWindowThreadProcessId(window, &mut process);
                if process == GetCurrentProcessId() && IsWindowVisible(window) != 0 {
                    return Some(window);
                }
            }
        }
    }

    /// Holt das Hauptfenster zurück und entfernt das Symbol.
    unsafe fn restore(tray: isize) {
        // SAFETY: Alle Handles stammen aus diesem Modul; ungültige werden von Windows abgelehnt.
        unsafe {
            let main = MAIN_WINDOW.swap(0, Ordering::SeqCst);
            if main != 0 {
                ShowWindow(main, SW_SHOW);
                ShowWindow(main, SW_RESTORE);
                SetForegroundWindow(main);
            }
            Shell_NotifyIconW(NIM_DELETE, &icon_data(tray, 0, "", "", "", false));
            TRAY_WINDOW.store(0, Ordering::SeqCst);
            DestroyWindow(tray);
            PostQuitMessage(0);
        }
    }

    unsafe extern "system" fn window_proc(window: isize, message: u32, wparam: usize, lparam: isize) -> isize {
        // SAFETY: Aufruf durch Windows mit gültigem Fenster; siehe `restore`.
        unsafe {
            if message == CALLBACK {
                let event = lparam as u32 & 0xFFFF;
                if matches!(event, WM_LBUTTONUP | WM_LBUTTONDBLCLK | WM_RBUTTONUP | NIN_BALLOONUSERCLICK) {
                    restore(window);
                }
                return 0;
            }
            DefWindowProcW(window, message, wparam, lparam)
        }
    }

    fn is_hidden() -> bool {
        MAIN_WINDOW.load(Ordering::SeqCst) != 0
    }

    pub fn hide(window_title: &str, tooltip: &str) -> bool {
        if is_hidden() {
            return true;
        }
        let Some(main) = find_main_window(window_title) else { return false };
        let tooltip = tooltip.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        // Das Symbol braucht ein eigenes Fenster mit eigener Nachrichtenschleife.
        std::thread::spawn(move || {
            let class_name = wide(CLASS_NAME);
            // SAFETY: Zeichenketten und Strukturen leben bis zum Ende der Aufrufe; die
            // Nachrichtenschleife läuft, bis `restore` sie beendet.
            unsafe {
                let module = GetModuleHandleW(std::ptr::null());
                let class = WndClass {
                    style: 0,
                    wnd_proc: Some(window_proc),
                    class_extra: 0,
                    window_extra: 0,
                    instance: module,
                    icon: 0,
                    cursor: 0,
                    background: 0,
                    menu_name: std::ptr::null(),
                    class_name: class_name.as_ptr(),
                };
                // Beim zweiten Verstecken gibt es die Klasse schon – das ist in Ordnung.
                RegisterClassW(&class);
                let tray = CreateWindowExW(0, class_name.as_ptr(), class_name.as_ptr(), 0, 0, 0, 0, 0, HWND_MESSAGE, 0, module, std::ptr::null());
                let added = tray != 0
                    && Shell_NotifyIconW(NIM_ADD, &icon_data(tray, NIF_MESSAGE | NIF_ICON | NIF_TIP, &tooltip, "", "", false)) != 0;
                if !added {
                    if tray != 0 {
                        DestroyWindow(tray);
                    }
                    let _ = ready_tx.send(false);
                    return;
                }
                TRAY_WINDOW.store(tray, Ordering::SeqCst);
                let _ = ready_tx.send(true);
                let mut message = std::mem::zeroed::<Msg>();
                while GetMessageW(&mut message, 0, 0, 0) > 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
        // Erst verstecken, wenn das Symbol wirklich da ist – sonst käme niemand mehr an das Fenster.
        if ready_rx.recv_timeout(std::time::Duration::from_secs(5)) != Ok(true) {
            return false;
        }
        MAIN_WINDOW.store(main, Ordering::SeqCst);
        // SAFETY: `main` ist das eben gefundene Fenster dieses Prozesses.
        unsafe { ShowWindow(main, SW_HIDE) };
        true
    }

    pub fn finished(done: &Done) {
        let tray = TRAY_WINDOW.load(Ordering::SeqCst);
        if tray == 0 {
            return;
        }
        let data = icon_data(tray, NIF_INFO | NIF_TIP, done.title, done.title, done.text, done.error);
        // SAFETY: Die Struktur ist vollständig gefüllt; ein inzwischen entferntes Symbol lehnt Windows ab.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Done;

    pub fn hide(_window_title: &str, _tooltip: &str) -> bool {
        false
    }

    pub fn finished(_done: &Done) {}
}

/// Versteckt das Fenster mit dem Titel `window_title` und zeigt dafür ein Symbol im Infobereich
/// (`tooltip` erscheint beim Überfahren). Ein Klick auf das Symbol holt das Fenster zurück.
/// Rückgabe: Hat es geklappt? Wenn nicht, bleibt das Fenster sichtbar.
pub fn hide(window_title: &str, tooltip: &str) -> bool {
    imp::hide(window_title, tooltip)
}

/// Meldet am Symbol, dass der Vorgang fertig ist (nur wenn das Fenster versteckt ist).
/// Ein Klick auf die Meldung oder das Symbol holt das Fenster zurück.
pub fn finished(done: &Done) {
    imp::finished(done);
}
