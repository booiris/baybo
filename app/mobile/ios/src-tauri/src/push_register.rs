//! iOS push registration.
//!
//! Requests **provisional** notification authorization so the system delivers
//! `mutable-content` pushes to the NSE (provisional is granted silently — no
//! permission prompt — and the notification arrives quietly in Notification
//! Center / on the lock screen, which is exactly the lock-screen-preview path),
//! and registers for remote notifications to kick off APNs device-token
//! issuance. Capturing the token requires hooking the UIApplicationDelegate
//! (owned by the Tauri/wry runtime) and is a follow-up for the real-APNs (M4)
//! path; the trigger is here so registration begins at launch.
//!
//! No-op off iOS.

#[cfg(target_os = "ios")]
pub fn register() {
    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2::runtime::Bool;
    use objc2_foundation::NSError;
    use objc2_ui_kit::UIApplication;
    use objc2_user_notifications::{UNAuthorizationOptions, UNUserNotificationCenter};

    let options = UNAuthorizationOptions::Provisional
        | UNAuthorizationOptions::Alert
        | UNAuthorizationOptions::Sound;
    let handler = RcBlock::new(|_granted: Bool, _err: *mut NSError| {});

    // The completion block is reference-counted (RcBlock) and outlives the async
    // authorization request.
    let center = UNUserNotificationCenter::currentNotificationCenter();
    center.requestAuthorizationWithOptions_completionHandler(options, &handler);

    // Remote-notification registration must run on the main thread.
    if let Some(mtm) = MainThreadMarker::new() {
        let app = UIApplication::sharedApplication(mtm);
        app.registerForRemoteNotifications();
    }
}

#[cfg(not(target_os = "ios"))]
pub fn register() {}
