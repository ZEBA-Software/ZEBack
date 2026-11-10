//! Netzwerkfreigaben (SMB): Verbindung mit Benutzername und Passwort herstellen.
//! Danach ist die Freigabe über ihren UNC-Pfad wie ein normaler Ordner benutzbar.

use anyhow::{Result, bail};

/// Liefert den Freigabe-Teil eines UNC-Pfads: `\\server\freigabe\ordner` → `\\server\freigabe`.
pub fn share_root(path: &str) -> Option<String> {
    let rest = path.trim().strip_prefix("\\\\")?;
    let mut parts = rest.split('\\').filter(|p| !p.is_empty());
    let (server, share) = (parts.next()?, parts.next()?);
    Some(format!("\\\\{server}\\{share}"))
}

/// Verständliche Meldung zu einem Windows-Netzwerkfehler.
fn describe(code: u32) -> String {
    match code {
        5 => t!("Zugriff verweigert", "Access denied").to_string(),
        53 | 67 | 51 => t!("Server oder Freigabe nicht gefunden", "Server or network share not found").to_string(),
        86 | 1326 | 1327 | 1331 => t!("Benutzername oder Passwort falsch", "Wrong user name or password").to_string(),
        1219 => t!(
            "Zu diesem Server besteht bereits eine Verbindung mit einer anderen Anmeldung",
            "There is already a connection to this server with different credentials"
        )
        .to_string(),
        1203 | 1222 => t!("Kein Netzwerk erreichbar", "No network available").to_string(),
        other => tr!("Windows-Fehler {other}", "Windows error {other}"),
    }
}

#[cfg(windows)]
mod ffi {
    #[repr(C)]
    pub struct NetResource {
        pub scope: u32,
        pub kind: u32,
        pub display_type: u32,
        pub usage: u32,
        pub local_name: *mut u16,
        pub remote_name: *mut u16,
        pub comment: *mut u16,
        pub provider: *mut u16,
    }

    #[link(name = "mpr")]
    unsafe extern "system" {
        pub fn WNetAddConnection2W(resource: *const NetResource, password: *const u16, user: *const u16, flags: u32) -> u32;
    }
}

/// Meldet sich an der Freigabe an. Ohne Benutzernamen passiert nichts: dann gilt die
/// Windows-Anmeldung des angemeldeten Benutzers.
#[cfg(windows)]
pub fn connect(path: &str, user: &str, password: &str) -> Result<()> {
    use std::ptr::null_mut;
    const RESOURCETYPE_DISK: u32 = 1;
    const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;

    let user = user.trim();
    if user.is_empty() {
        return Ok(());
    }
    let Some(root) = share_root(path) else {
        bail!(t!(
            "Die Freigabe muss die Form \\\\server\\freigabe haben.",
            "The network share must have the form \\\\server\\share."
        ));
    };
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let (mut remote, user_wide, password_wide) = (wide(&root), wide(user), wide(password));
    let resource = ffi::NetResource {
        scope: 0,
        kind: RESOURCETYPE_DISK,
        display_type: 0,
        usage: 0,
        local_name: null_mut(), // ohne Laufwerksbuchstaben verbinden
        remote_name: remote.as_mut_ptr(),
        comment: null_mut(),
        provider: null_mut(),
    };
    // SAFETY: Alle Zeichenketten sind nullterminiert und leben bis zum Ende des Aufrufs.
    let code = unsafe { ffi::WNetAddConnection2W(&resource, password_wide.as_ptr(), user_wide.as_ptr(), 0) };
    match code {
        0 => Ok(()),
        // Es besteht schon eine Verbindung zu diesem Server – sie wird weiterverwendet.
        ERROR_SESSION_CREDENTIAL_CONFLICT if std::path::Path::new(&root).is_dir() => Ok(()),
        code => bail!(tr!("Anmeldung an {root} fehlgeschlagen: {}", "Sign-in to {root} failed: {}", describe(code))),
    }
}

#[cfg(not(windows))]
pub fn connect(_path: &str, _user: &str, _password: &str) -> Result<()> {
    bail!(t!("Netzwerkfreigaben werden nur unter Windows unterstützt", "Network shares are only supported on Windows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_share_in_a_unc_path() {
        assert_eq!(share_root("\\\\nas\\backup").as_deref(), Some("\\\\nas\\backup"));
        assert_eq!(share_root("\\\\nas\\backup\\pc01\\2026").as_deref(), Some("\\\\nas\\backup"));
        assert_eq!(share_root(" \\\\nas\\backup\\ ").as_deref(), Some("\\\\nas\\backup"));
        assert_eq!(share_root("\\\\nas"), None);
        assert_eq!(share_root("E:\\Backups"), None);
    }

    #[test]
    fn connecting_without_user_does_nothing() {
        assert!(connect("\\\\gibt-es-nicht\\freigabe", "", "").is_ok());
    }

    #[test]
    fn describes_common_errors() {
        assert!(describe(1326).contains("Passwort"));
        assert!(describe(53).contains("nicht gefunden"));
        assert!(describe(4711).contains("4711"));
    }
}
