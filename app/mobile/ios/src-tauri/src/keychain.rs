//! Persist the per-device push key into the shared App Group keychain so the
//! Notification Service Extension can decrypt lock-screen previews.
//!
//! The NSE reads the key in Swift (`apple/NotificationExtension/PushKeyStore
//! .swift`); this is the matching WRITE side. It calls the Security framework
//! (`SecItemAdd`) directly from Rust — the framework is already linked into the
//! app (`Security.framework` in `project.yml`), so no extra Swift in the app
//! target is needed. On non-iOS targets (the desktop dev build of the shell)
//! it is a no-op: there is no shared keychain off-device.

use device_proto::aead::KEY_LEN;

#[cfg(target_os = "ios")]
mod imp {
    use super::KEY_LEN;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::data::CFData;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;
    use core_foundation_sys::base::{CFTypeRef, OSStatus};
    use core_foundation_sys::dictionary::CFDictionaryRef;
    use core_foundation_sys::string::CFStringRef;

    // The Security framework keychain item API. Hand-declared (rather than via a
    // -sys crate) to keep the dependency surface to core-foundation only.
    use core_foundation::boolean::CFBoolean;
    use core_foundation_sys::data::CFDataRef;

    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecItemAdd(attributes: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        fn SecItemDelete(query: CFDictionaryRef) -> OSStatus;
        fn SecItemCopyMatching(query: CFDictionaryRef, result: *mut CFTypeRef) -> OSStatus;
        static kSecClass: CFStringRef;
        static kSecClassGenericPassword: CFStringRef;
        static kSecAttrAccount: CFStringRef;
        static kSecAttrAccessGroup: CFStringRef;
        static kSecValueData: CFStringRef;
        static kSecAttrAccessible: CFStringRef;
        static kSecAttrAccessibleAfterFirstUnlock: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecMatchLimit: CFStringRef;
        static kSecMatchLimitOne: CFStringRef;
    }

    const ERR_SEC_SUCCESS: OSStatus = 0;
    /// MUST match `PushKeyStore.accessGroup` / `.accountPrefix` in the NSE.
    const ACCESS_GROUP: &str = "group.com.baybo.app";
    const ACCOUNT_PREFIX: &str = "baybo.push-key.";

    /// Wrap a `static` Security-framework `CFStringRef` constant as a borrowed
    /// `CFString` (get-rule: we don't own it, so don't release it).
    unsafe fn constant(r: CFStringRef) -> CFType {
        unsafe { CFString::wrap_under_get_rule(r) }.as_CFType()
    }

    pub(super) fn store_push_key(bid: &str, key: &[u8; KEY_LEN]) -> Result<(), String> {
        let account = CFString::new(&format!("{ACCOUNT_PREFIX}{bid}")).as_CFType();
        let group = CFString::new(ACCESS_GROUP).as_CFType();
        let data = CFData::from_buffer(&key[..]).as_CFType();

        // SAFETY: the kSec* statics are valid CFStringRefs from the linked
        // Security framework; the dictionaries outlive each Sec* call.
        unsafe {
            let generic_pw = constant(kSecClassGenericPassword);
            let identity = vec![
                (constant(kSecClass), generic_pw.clone()),
                (constant(kSecAttrAccount), account.clone()),
                (constant(kSecAttrAccessGroup), group.clone()),
            ];
            // Upsert: a re-pair re-derives the same key; delete-then-add avoids
            // an errSecDuplicateItem.
            let query = CFDictionary::from_CFType_pairs(&identity);
            SecItemDelete(query.as_concrete_TypeRef());

            let mut attrs = identity;
            attrs.push((constant(kSecValueData), data));
            // `AfterFirstUnlock` so the NSE can read it on the lock screen (after
            // the first unlock since boot), matching the preview's threat model.
            attrs.push((
                constant(kSecAttrAccessible),
                constant(kSecAttrAccessibleAfterFirstUnlock),
            ));
            let add = CFDictionary::from_CFType_pairs(&attrs);
            let status = SecItemAdd(add.as_concrete_TypeRef(), std::ptr::null_mut());
            if status == ERR_SEC_SUCCESS {
                Ok(())
            } else {
                Err(format!("SecItemAdd failed (OSStatus {status})"))
            }
        }
    }

    /// Read the push key back from the shared keychain — the same lookup the
    /// NSE's `PushKeyStore` does. Used by the debug self-check to prove the
    /// access-group round-trip works on-device. `Ok(None)` = not found.
    #[cfg(debug_assertions)]
    pub(super) fn read_push_key(bid: &str) -> Result<Option<[u8; KEY_LEN]>, String> {
        let account = CFString::new(&format!("{ACCOUNT_PREFIX}{bid}")).as_CFType();
        let group = CFString::new(ACCESS_GROUP).as_CFType();
        // SAFETY: as in `store_push_key` — valid constants, dictionary outlives
        // the call, and the returned CFData is owned (create rule).
        unsafe {
            let query = CFDictionary::from_CFType_pairs(&[
                (constant(kSecClass), constant(kSecClassGenericPassword)),
                (constant(kSecAttrAccount), account),
                (constant(kSecAttrAccessGroup), group),
                (constant(kSecReturnData), CFBoolean::true_value().as_CFType()),
                (constant(kSecMatchLimit), constant(kSecMatchLimitOne)),
            ]);
            let mut out: CFTypeRef = std::ptr::null();
            let status = SecItemCopyMatching(query.as_concrete_TypeRef(), &mut out);
            if status != ERR_SEC_SUCCESS || out.is_null() {
                return Ok(None);
            }
            let data = CFData::wrap_under_create_rule(out as CFDataRef);
            data.bytes()
                .try_into()
                .map(Some)
                .map_err(|_| "stored push key has the wrong length".to_string())
        }
    }
}

#[cfg(not(target_os = "ios"))]
mod imp {
    use super::KEY_LEN;
    pub(super) fn store_push_key(_bid: &str, _key: &[u8; KEY_LEN]) -> Result<(), String> {
        Ok(())
    }
}

/// Write the device's push key to the shared App Group keychain at account
/// `baybo.push-key.<bid>`. `bid` is the device id the gateway stamps into every
/// push payload, so the NSE can look the key back up.
pub fn store_push_key(bid: &str, key: &[u8; KEY_LEN]) -> Result<(), String> {
    imp::store_push_key(bid, key)
}

/// Debug self-check: read the key back (the same lookup the NSE does) to prove
/// the access-group round-trip works on-device. iOS debug builds only.
#[cfg(all(debug_assertions, target_os = "ios"))]
pub fn read_push_key(bid: &str) -> Result<Option<[u8; KEY_LEN]>, String> {
    imp::read_push_key(bid)
}
