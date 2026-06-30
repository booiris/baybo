//! Strip the iOS form-assistant bar from the keyboard.
//!
//! WKWebView shows a UIKit "input accessory" strip above the keyboard for any
//! focused web `<input>`/`<textarea>` — prev/next-field chevrons on the left, a
//! Done button on the right. The strip is owned by the webview's private content
//! view (`WKContentView`) via its `inputAccessoryView` getter, and Tauri/wry
//! expose no switch for it. We override that getter on the class to return `nil`.
//! The app runs a single webview, so patching the class (rather than chasing the
//! one content-view instance, which WebKit can recreate) is both the simplest hook
//! and total — every content view inherits the override.
//!
//! Best-effort + idempotent: the `WKContentView` class only exists once WebKit has
//! loaded (i.e. after the webview is built), so this is called from `setup` and
//! re-tried on foreground until it lands. (iPhone keyboard only — the iPad
//! hardware-keyboard shortcut bar is a separate `inputAssistantItem` and untouched.)
//!
//! No-op off iOS.

#[cfg(target_os = "ios")]
pub fn hide_input_accessory_bar() {
    use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
    use objc2::{ffi, sel};
    use std::sync::atomic::{AtomicBool, Ordering};

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.load(Ordering::Relaxed) {
        return;
    }

    // None until WebKit registers the private content-view class (after the
    // webview exists); leave INSTALLED false so the next call retries.
    let Some(cls) = AnyClass::get(c"WKContentView") else {
        return;
    };

    // Objective-C type encoding for `- (UIView *)inputAccessoryView`: object
    // return (`@`), then self (`@`) and _cmd (`:`).
    const TYPES: &std::ffi::CStr = c"@@:";

    // SAFETY: the IMP's signature (self, _cmd -> id) matches the `@@:` encoding,
    // so the runtime invokes it correctly; a null return is a valid `nil` id.
    let imp: Imp = unsafe {
        std::mem::transmute::<extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject, Imp>(
            no_accessory_view,
        )
    };

    // `class_replaceMethod` installs the override whether or not WKContentView
    // already implements the getter (it does) — unlike `class_addMethod`, which
    // would no-op when the method is already present.
    unsafe {
        ffi::class_replaceMethod(
            (cls as *const AnyClass).cast_mut(),
            sel!(inputAccessoryView),
            imp,
            TYPES.as_ptr(),
        );
    }
    INSTALLED.store(true, Ordering::Relaxed);
}

/// Returns `nil` so a focused web field shows no accessory bar above the keyboard.
/// (See [`hide_input_accessory_bar`].)
#[cfg(target_os = "ios")]
extern "C-unwind" fn no_accessory_view(
    _this: *mut objc2::runtime::AnyObject,
    _cmd: objc2::runtime::Sel,
) -> *mut objc2::runtime::AnyObject {
    std::ptr::null_mut()
}

#[cfg(not(target_os = "ios"))]
pub fn hide_input_accessory_bar() {}
