//! System notifications with a message (Preferences > Notifications): a
//! toast on Windows, Notification Center on macOS, a freedesktop
//! notification on Linux. They are shown from a thread of their own, so
//! sending one never slows a frame, and scripted runs print them instead.
//!
//! - **Windows** toasts need an application id that Windows knows. GitKrill
//!   registers [`APP_ID`] for the current user (`HKCU\Software\Classes\
//!   AppUserModelId\Moulman.GitKrill`: its name and icon), which works the
//!   same for the installed and the portable copy and needs no Start menu
//!   shortcut; the installer removes the key on uninstall.
//! - **macOS** shows them as GitKrill's own (the UserNotifications
//!   framework, [`macos`]) when GitKrill runs from its app bundle, and
//!   through `osascript` (`display notification`, listed under Script
//!   Editor) otherwise: a bare binary, a bundle without an identifier, or
//!   notifications macOS does not allow GitKrill or refuses.
//! - **Linux** uses `notify-send`, or `gdbus` when it is missing.

#[cfg(target_os = "macos")]
mod macos;

use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock, PoisonError};

/// The application id toasts are shown under on Windows.
#[cfg_attr(not(windows), allow(dead_code))]
pub const APP_ID: &str = "Moulman.GitKrill";

/// A title and a body, on their way to the notifications thread.
type Queue = Mutex<Option<Sender<(String, String)>>>;

static QUEUE: OnceLock<Queue> = OnceLock::new();

/// Shows `title` and `body` (its first lines) in a system notification.
pub fn send(title: &str, body: &str) {
    let body = shorten(body);
    if std::env::var_os("GITKRILL_SHOT").is_some() {
        println!("system notification: {title} | {}", body.replace('\n', " / "));
        return;
    }
    if cfg!(test) {
        return;
    }
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = channel::<(String, String)>();
        let started = std::thread::Builder::new().name("notifications".into()).spawn(move || {
            #[cfg(windows)]
            // SAFETY: once, on this thread, before any WinRT call.
            unsafe {
                let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
            }
            for (title, body) in rx {
                if let Err(e) = show(&title, &body) {
                    eprintln!("system notification failed: {e}");
                }
            }
        });
        Mutex::new(started.ok().map(|_| tx))
    });
    if let Some(tx) = queue.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        let _ = tx.send((title.to_owned(), body));
    }
}

/// At most four lines of at most 200 characters in all: a notification
/// is a glance; the toast in the window has the rest.
fn shorten(body: &str) -> String {
    let lines: Vec<&str> = body.lines().map(str::trim).filter(|line| !line.is_empty()).take(4).collect();
    let text = lines.join("\n");
    if text.chars().count() <= 200 {
        return text;
    }
    let mut cut: String = text.chars().take(199).collect();
    cut.push('…');
    cut
}

/// The app icon as a PNG file next to the settings, written once per run,
/// for the notifications that take a picture by path.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn icon_file() -> Option<PathBuf> {
    static ICON: OnceLock<Option<PathBuf>> = OnceLock::new();
    ICON.get_or_init(|| {
        let dir = match std::env::var_os("GITKRILL_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir()?.join("GitKrill"),
        };
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("notification-icon.png");
        let icon = crate::theme::app_icon();
        let image = image::RgbaImage::from_raw(icon.width, icon.height, icon.rgba)?;
        image.save(&path).ok()?;
        Some(path)
    })
    .clone()
}

#[cfg(windows)]
fn show(title: &str, body: &str) -> Result<(), String> {
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
    REGISTERED.get_or_init(|| register(APP_ID, "GitKrill")).clone()?;
    toast(APP_ID, title, body, false)
}

/// Shows a toast under `app_id`; `quiet` puts it in the notification
/// centre without a banner.
#[cfg(windows)]
fn toast(app_id: &str, title: &str, body: &str, quiet: bool) -> Result<(), String> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    use windows::core::HSTRING;

    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_xml(title),
        escape_xml(body)
    );
    let fail = |e: windows::core::Error| e.message();
    let document = XmlDocument::new().map_err(fail)?;
    document.LoadXml(&HSTRING::from(xml)).map_err(fail)?;
    let toast = ToastNotification::CreateToastNotification(&document).map_err(fail)?;
    if quiet {
        toast.SetSuppressPopup(true).map_err(fail)?;
    }
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id)).map_err(fail)?;
    notifier.Show(&toast).map_err(fail)
}

/// Tells Windows the name and icon of `app_id` ([`APP_ID`]), for the
/// current user.
#[cfg(windows)]
fn register(app_id: &str, name: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RegSetKeyValueW};
    use windows::core::HSTRING;

    let key = HSTRING::from(format!(r"Software\Classes\AppUserModelId\{app_id}"));
    let mut values = vec![("DisplayName", name.to_owned())];
    if let Some(icon) = icon_file() {
        values.push(("IconUri", icon.to_string_lossy().into_owned()));
    }
    for (name, value) in values {
        let wide: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        // SAFETY: the pointer and size describe `wide`, which outlives the call.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &key,
                &HSTRING::from(name),
                REG_SZ.0,
                Some(wide.as_ptr().cast()),
                (wide.len() * 2) as u32,
            )
        };
        if status.is_err() {
            return Err(format!("could not register the notification sender ({})", status.0));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn show(title: &str, body: &str) -> Result<(), String> {
    if macos::available() {
        match macos::show(title, body) {
            Ok(()) => return Ok(()),
            Err(why) => {
                // Said once: it usually stays so for the whole run.
                static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("system notification through osascript instead: {why}");
                }
            }
        }
    }
    // The texts are arguments of the script, never part of it.
    let status = std::process::Command::new("osascript")
        .args(["-e", "on run argv", "-e", "display notification (item 2 of argv) with title (item 1 of argv)", "-e", "end run"])
        .arg(title)
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("osascript failed ({status})")) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn show(title: &str, body: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};
    let quiet = |cmd: &mut Command| {
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    };
    let icon = icon_file().map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();

    let mut notify = Command::new("notify-send");
    notify.args(["-a", "GitKrill"]);
    if !icon.is_empty() {
        notify.args(["-i", &icon]);
    }
    notify.args(["--", title, body]);
    quiet(&mut notify);
    match notify.status() {
        Ok(status) if status.success() => return Ok(()),
        Ok(_) => {}
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
        Err(_) => {}
    }

    // The same request over D-Bus, through glib's tool.
    let mut gdbus = Command::new("gdbus");
    gdbus.args([
        "call",
        "--session",
        "--dest",
        "org.freedesktop.Notifications",
        "--object-path",
        "/org/freedesktop/Notifications",
        "--method",
        "org.freedesktop.Notifications.Notify",
        &gvariant("GitKrill"),
        "0",
        &gvariant(&icon),
        &gvariant(title),
        &gvariant(body),
        "[]",
        "{}",
        "-1",
    ]);
    quiet(&mut gdbus);
    let status = gdbus.status().map_err(|e| format!("neither notify-send nor gdbus could be run: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("gdbus failed ({status})")) }
}

/// A string in GVariant's text format, as `gdbus call` reads it.
#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn gvariant(text: &str) -> String {
    let mut out = String::from("'");
    for c in text.chars() {
        match c {
            '\\' => out.push_str(r"\\"),
            '\'' => out.push_str(r"\'"),
            '\n' => out.push_str(r"\n"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_are_kept_short() {
        assert_eq!(shorten("  one\n\n two \nthree\nfour\nfive"), "one\ntwo\nthree\nfour");
        let long = "é".repeat(300);
        let short = shorten(&long);
        assert_eq!(short.chars().count(), 200);
        assert!(short.ends_with('…'));
    }

    /// Run by hand on Windows: a toast under a throwaway application id,
    /// without a banner, reaches the notification centre; the toast and
    /// the id are removed again.
    #[cfg(windows)]
    #[test]
    #[ignore = "writes HKCU and the notification centre (both cleaned up)"]
    fn a_windows_toast_reaches_the_notification_centre() {
        use windows::UI::Notifications::ToastNotificationManager;
        use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RegDeleteTreeW};
        use windows::core::HSTRING;

        const ID: &str = "Moulman.GitKrill.Test";
        // SAFETY: before any WinRT call on this thread.
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
        }
        register(ID, "GitKrill Test").unwrap();
        let sent = toast(ID, "GitKrill test", "Delivery check <&>", true);
        std::thread::sleep(std::time::Duration::from_millis(800));
        let history = ToastNotificationManager::History().unwrap();
        let delivered = history.GetHistoryWithId(&HSTRING::from(ID)).and_then(|list| list.Size());
        let _ = history.ClearWithId(&HSTRING::from(ID));
        // SAFETY: deletes the key made above.
        let _ = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &HSTRING::from(format!(r"Software\Classes\AppUserModelId\{ID}"))) };
        sent.unwrap();
        assert_eq!(delivered.unwrap(), 1);
    }

    /// The test binary is not an app bundle: the framework, which would
    /// throw without one, is never called.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_bare_binary_does_not_use_the_notification_framework() {
        assert!(!macos::available());
    }

    #[test]
    fn gvariant_strings_are_quoted() {
        assert_eq!(gvariant(r"it's a \ test"), r"'it\'s a \\ test'");
        assert_eq!(gvariant("two\nlines"), r"'two\nlines'");
    }
}
