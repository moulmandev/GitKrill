//! Notifications that come from GitKrill itself on macOS, through the
//! UserNotifications framework, so Notification Center lists them under
//! GitKrill and System Settings > Notifications has a GitKrill entry.
//!
//! The framework works for an app bundle only: it takes the app from the
//! main bundle's identifier (`CFBundleIdentifier` in `Info.plist`; the CI
//! sets `dev.gitkrill.app`), and throws an Objective-C exception, which
//! would end the process, when there is none. [`available`] says whether
//! GitKrill runs from one. The first notification asks macOS for
//! permission (`requestAuthorization`), which asks the user once; macOS
//! answers later requests at once with what the user chose. Everything here
//! runs on the notifications thread and waits there for macOS's answers
//! (the framework may be called from any thread). An error is returned
//! when macOS does not allow the notification or refuses it, and the
//! caller shows it another way.
//!
//! macOS shows no banner for a notification that arrives while its app is
//! the active one, unless the app sets a delegate that asks for one.
//! GitKrill sends them only while its window is in the background, so it
//! sets none.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::autoreleasepool;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNUserNotificationCenter,
};

/// How long a notification waits for the user to answer macOS's question
/// whether GitKrill may show notifications (only the first one asks).
const ASKING: Duration = Duration::from_secs(60);
/// How long macOS gets to take a notification.
const ADDING: Duration = Duration::from_secs(10);

/// Whether GitKrill runs from an app bundle with an identifier, which the
/// framework needs. A bare binary (`cargo run`, the binary copied out of
/// the bundle) has none.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        autoreleasepool(|_| {
            let bundle = NSBundle::mainBundle();
            let id = bundle.bundleIdentifier().map(|id| id.to_string()).unwrap_or_default();
            !id.trim().is_empty() && bundle.bundlePath().to_string().ends_with(".app")
        })
    })
}

/// Shows `title` and `body` as a notification of GitKrill's, once macOS
/// allows it. Only call it when [`available`].
pub fn show(title: &str, body: &str) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    autoreleasepool(|_| {
        let center = UNUserNotificationCenter::currentNotificationCenter();
        authorize(&center)?;

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(title));
        content.setBody(&NSString::from_str(body));
        let id = format!("gitkrill-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        // No trigger: shown at once.
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str(&id), &content, None);

        let (tx, rx) = channel();
        let added = RcBlock::new(move |error: *mut NSError| {
            let _ = tx.send(describe(error));
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&added));
        match rx.recv_timeout(ADDING) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => Err(format!("macOS refused the notification: {why}")),
            Err(_) => Err("macOS did not take the notification in time".to_owned()),
        }
    })
}

/// Asks macOS whether GitKrill may show banners (no sound, as before).
fn authorize(center: &UNUserNotificationCenter) -> Result<(), String> {
    let (tx, rx) = channel();
    let answered = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        let _ = tx.send(match describe(error) {
            Some(why) => Err(format!("macOS does not allow GitKrill's notifications: {why}")),
            None if granted.as_bool() => Ok(()),
            None => Err("notifications are turned off for GitKrill in System Settings".to_owned()),
        });
    });
    center.requestAuthorizationWithOptions_completionHandler(UNAuthorizationOptions::Alert, &answered);
    rx.recv_timeout(ASKING).unwrap_or_else(|_| Err("macOS did not say whether GitKrill may show notifications".to_owned()))
}

/// What an error handed to a completion handler says; `None` for none.
fn describe(error: *mut NSError) -> Option<String> {
    // SAFETY: the framework passes a valid NSError or null, alive for the
    // handler's call.
    let error = unsafe { error.as_ref() }?;
    Some(format!("{} ({})", error.localizedDescription(), error.code()))
}
