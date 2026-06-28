//! iOS push registration + APNs device-token capture.
//!
//! Requests **provisional** notification authorization (granted silently, so the
//! `mutable-content` push lands in Notification Center / on the lock screen — the
//! lock-screen-preview path), registers for remote notifications to start APNs
//! device-token issuance, and hooks the UIApplicationDelegate so the token that
//! iOS delivers to `application:didRegisterForRemoteNotificationsWithDeviceToken:`
//! is captured into a process-global ([`apns_token`]); pairing threads it into
//! `DeviceHello` when available, and a paired app also re-registers the token
//! directly with C's `/register` when iOS delivers it later.
//!
//! The delegate is owned by the Tauri/wry runtime, so the two APNs callbacks are
//! added to its class at launch via the Objective-C runtime (`class_addMethod`).
//! The runtime delegate implements neither, so the add succeeds and the system
//! then routes the token (and any failure) to us.
//!
//! No-op off iOS.

use std::sync::Mutex;

use remote_host_protocol::push::{ApnsEnv, RegisterRequest};

/// The captured APNs device token as lowercase hex, set once
/// `didRegisterForRemoteNotifications` fires (a few seconds after launch). `None`
/// until then, and always `None` off iOS.
static APNS_TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// The captured APNs device token (hex), if registration has completed. Pairing
/// reads this for `DeviceHello.apns_token`; if the token arrives later, the
/// paired app POSTs C's `/register` directly with the same relay admission key.
pub fn apns_token() -> Option<String> {
    APNS_TOKEN.lock().ok().and_then(|t| t.clone())
}

/// If an APNs token is already available, try to bind it to the current paired
/// record at C. Used after pairing completes, because the iOS token may have
/// arrived before the app had a durable `PairedRecord` to register against.
pub fn spawn_register_current_token() {
    if let Some(token) = apns_token() {
        spawn_register_token(token);
    }
}

fn spawn_register_token(token: String) {
    if token.is_empty() {
        return;
    }
    tauri::async_runtime::spawn(async move {
        if let Err(e) = register_token_with_remote(&token).await {
            eprintln!("baybo: APNs remote registration skipped: {e}");
        }
    });
}

async fn register_token_with_remote(token: &str) -> Result<(), String> {
    let Some(record) = crate::pairing::load_paired_record()? else {
        return Ok(());
    };
    if record.remote_api_key.is_empty()
        || record.relay_url.is_empty()
        || record.device_id.is_empty()
    {
        return Ok(());
    }
    let base = relay_url_to_http_base(&record.relay_url);
    if base.is_empty() {
        return Ok(());
    }
    let body = RegisterRequest {
        remote_api_key: record.remote_api_key,
        device_id: record.device_id,
        apns_token: token.to_string(),
        env: current_apns_env(),
    };
    let url = remote_host_protocol::push::register_url(&base);
    let resp = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("register post: {e}"))?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!("register status {}", resp.status()))
    }
}

fn relay_url_to_http_base(relay_url: &str) -> String {
    if let Some(rest) = relay_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = relay_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        relay_url.to_string()
    }
}

fn current_apns_env() -> ApnsEnv {
    if cfg!(debug_assertions) {
        ApnsEnv::Sandbox
    } else {
        ApnsEnv::Production
    }
}

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
        // Install the token-capture callbacks BEFORE kicking off registration so
        // the async token delivery can't race ahead of the method being added.
        install_token_capture(&app);
        app.registerForRemoteNotifications();
    }
}

#[cfg(target_os = "ios")]
fn set_apns_token(hex: String) {
    if let Ok(mut slot) = APNS_TOKEN.lock() {
        *slot = Some(hex.clone());
    }
    spawn_register_token(hex);
}

/// iOS delivers the APNs device token here; capture it as hex.
#[cfg(target_os = "ios")]
extern "C-unwind" fn did_register(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _app: *mut objc2::runtime::AnyObject,
    token: *mut objc2_foundation::NSData,
) {
    if token.is_null() {
        return;
    }
    let data: &objc2_foundation::NSData = unsafe { &*token };
    set_apns_token(hex::encode(data.to_vec()));
}

/// iOS reports APNs registration failure here; log and leave the token unset.
/// A later successful token callback will retry registration once paired.
#[cfg(target_os = "ios")]
extern "C-unwind" fn did_fail(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
    _app: *mut objc2::runtime::AnyObject,
    _error: *mut objc2_foundation::NSError,
) {
    eprintln!("baybo: APNs registration failed");
}

/// Add the two APNs delegate callbacks to the live app-delegate's class.
#[cfg(target_os = "ios")]
fn install_token_capture(app: &objc2_ui_kit::UIApplication) {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{ffi, msg_send, sel};

    // Objective-C type encoding for both selectors: void return (`v`), then
    // self (`@`), _cmd (`:`), and two object args (`@@`) — UIApplication* plus
    // NSData* / NSError*.
    const TYPES: &std::ffi::CStr = c"v@:@@";

    let delegate: *mut AnyObject = unsafe { msg_send![app, delegate] };
    if delegate.is_null() {
        eprintln!("baybo: no app delegate; APNs token capture not installed");
        return;
    }
    let cls = unsafe { ffi::object_getClass(delegate) } as *mut AnyClass;
    if cls.is_null() {
        return;
    }

    // SAFETY: the IMP signatures match the selectors' Objective-C signatures
    // (the `v@:@@` encoding above), so the runtime invokes them correctly.
    let did_register_imp: Imp = unsafe {
        std::mem::transmute::<
            extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut objc2_foundation::NSData,
            ),
            Imp,
        >(did_register)
    };
    let did_fail_imp: Imp = unsafe {
        std::mem::transmute::<
            extern "C-unwind" fn(
                *mut AnyObject,
                Sel,
                *mut AnyObject,
                *mut objc2_foundation::NSError,
            ),
            Imp,
        >(did_fail)
    };

    unsafe {
        let added = ffi::class_addMethod(
            cls,
            sel!(application:didRegisterForRemoteNotificationsWithDeviceToken:),
            did_register_imp,
            TYPES.as_ptr(),
        );
        if !added.as_bool() {
            eprintln!(
                "baybo: APNs token capture not installed (delegate already implements the callback)"
            );
        }
        let _ = ffi::class_addMethod(
            cls,
            sel!(application:didFailToRegisterForRemoteNotificationsWithError:),
            did_fail_imp,
            TYPES.as_ptr(),
        );
    }
}

#[cfg(not(target_os = "ios"))]
pub fn register() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_ws_base_maps_to_http_for_register() {
        assert_eq!(
            relay_url_to_http_base("wss://proxy.baybo.space"),
            "https://proxy.baybo.space"
        );
        assert_eq!(
            relay_url_to_http_base("ws://127.0.0.1:9000"),
            "http://127.0.0.1:9000"
        );
        assert_eq!(
            relay_url_to_http_base("https://proxy.baybo.space"),
            "https://proxy.baybo.space"
        );
    }
}
