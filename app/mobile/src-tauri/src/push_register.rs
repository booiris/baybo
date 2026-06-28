//! iOS push registration + APNs device-token capture.
//!
//! Requests **provisional** notification authorization (granted silently, so the
//! `mutable-content` push lands in Notification Center / on the lock screen — the
//! lock-screen-preview path), registers for remote notifications to start APNs
//! device-token issuance, and hooks the UIApplicationDelegate so the token that
//! iOS delivers to `application:didRegisterForRemoteNotificationsWithDeviceToken:`
//! is captured into a process-global ([`apns_token`]); pairing threads it into
//! `DeviceHello`, and the gateway registers it with C under a gateway-signed
//! binding (the app holds no gateway push key, so it cannot register directly).
//!
//! The delegate is owned by the Tauri/wry runtime, so the two APNs callbacks are
//! added to its class at launch via the Objective-C runtime (`class_addMethod`).
//! The runtime delegate implements neither, so the add succeeds and the system
//! then routes the token (and any failure) to us.
//!
//! No-op off iOS.

use std::sync::Mutex;

/// The captured APNs device token as lowercase hex, set once
/// `didRegisterForRemoteNotifications` fires (a few seconds after launch). `None`
/// until then, and always `None` off iOS.
static APNS_TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// The captured APNs device token (hex), if registration has completed. Pairing
/// reads this for `DeviceHello.apns_token`; the **gateway** then persists it and
/// registers it with C under a gateway-signed binding.
///
/// The app no longer registers with C directly: C now requires the binding to be
/// signed by the gateway push key the device delegated at pairing, and the app
/// holds no such key. A token that missed `DeviceHello`, or rotated after pairing,
/// instead reaches the gateway over the content channel — the content pump sends a
/// `Frame::UpdateApnsToken` with this value on every connect, and the gateway
/// persists it and re-registers (signed) when it changed.
pub fn apns_token() -> Option<String> {
    APNS_TOKEN.lock().ok().and_then(|t| t.clone())
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
        *slot = Some(hex);
    }
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
