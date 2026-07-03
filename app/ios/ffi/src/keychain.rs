//! Persist the per-device push key into the shared keychain access group so the
//! Notification Service Extension can decrypt lock-screen previews. Longer-lived
//! app-only credentials stay in the app's private keychain.
//!
//! The NSE reads the key in Swift (`apple/NotificationExtension/PushKeyStore
//! .swift`); this is the matching WRITE side. It calls the Security framework
//! (`SecItemAdd`) directly from Rust — the framework is already linked into the
//! app (`Security.framework` in `project.yml`), so no extra Swift in the app
//! target is needed. On non-iOS targets (the desktop dev build of the shell)
//! it is a no-op: there is no keychain off-device.

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
        static kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly: CFStringRef;
        static kSecReturnData: CFStringRef;
        static kSecMatchLimit: CFStringRef;
        static kSecMatchLimitOne: CFStringRef;
    }

    const ERR_SEC_SUCCESS: OSStatus = 0;
    /// `errSecItemNotFound` — a delete of an absent item is a benign no-op.
    const ERR_SEC_ITEM_NOT_FOUND: OSStatus = -25300;
    const MISSING_ACCESS_GROUP: &str = "missing BAYBO_IOS_KEYCHAIN_ACCESS_GROUP; build through Xcode/Tauri iOS or set BAYBO_IOS_KEYCHAIN_ACCESS_GROUP explicitly";
    const ACCOUNT_PREFIX: &str = "baybo.push-key.";

    /// Wrap a `static` Security-framework `CFStringRef` constant as a borrowed
    /// `CFString` (get-rule: we don't own it, so don't release it).
    unsafe fn constant(r: CFStringRef) -> CFType {
        unsafe { CFString::wrap_under_get_rule(r) }.as_CFType()
    }

    fn access_group() -> Result<CFType, String> {
        option_env!("BAYBO_IOS_KEYCHAIN_ACCESS_GROUP")
            .filter(|group| !group.is_empty())
            .map(|group| CFString::new(group).as_CFType())
            .ok_or_else(|| MISSING_ACCESS_GROUP.to_string())
    }

    /// Keychain item accessibility — controls device-binding, not just unlock.
    enum Accessibility {
        /// `…ThisDeviceOnly`: readable after first unlock, but bound to this
        /// device's hardware key — never included in a backup that could restore
        /// onto another device. Used for app-private secrets.
        DeviceOnly,
        /// `…AfterFirstUnlock`: readable cross-process (the NSE on the lock
        /// screen) and migratable via backup. Used for the shared push key.
        Shared,
    }

    /// Upsert an opaque blob into the keychain under `account`.
    fn store_blob(
        account: &str,
        bytes: &[u8],
        shared: bool,
        accessible: Accessibility,
    ) -> Result<(), String> {
        let account = CFString::new(account).as_CFType();
        let data = CFData::from_buffer(bytes).as_CFType();

        // SAFETY: the kSec* statics are valid CFStringRefs from the linked
        // Security framework; the dictionaries outlive each Sec* call.
        unsafe {
            let mut identity = vec![
                (constant(kSecClass), constant(kSecClassGenericPassword)),
                (constant(kSecAttrAccount), account.clone()),
            ];
            if shared {
                identity.push((constant(kSecAttrAccessGroup), access_group()?));
            }
            // Upsert: delete-then-add avoids an errSecDuplicateItem on re-pair.
            let query = CFDictionary::from_CFType_pairs(&identity);
            SecItemDelete(query.as_concrete_TypeRef());

            let mut attrs = identity;
            attrs.push((constant(kSecValueData), data));
            // Both classes are readable after the first unlock since boot (so an
            // app relaunch / the NSE works); they differ only in device-binding.
            let accessible_const = match accessible {
                Accessibility::DeviceOnly => {
                    constant(kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly)
                }
                Accessibility::Shared => constant(kSecAttrAccessibleAfterFirstUnlock),
            };
            attrs.push((constant(kSecAttrAccessible), accessible_const));
            let add = CFDictionary::from_CFType_pairs(&attrs);
            let status = SecItemAdd(add.as_concrete_TypeRef(), std::ptr::null_mut());
            if status == ERR_SEC_SUCCESS {
                Ok(())
            } else {
                Err(format!("SecItemAdd failed (OSStatus {status})"))
            }
        }
    }

    /// Read an opaque blob back. `Ok(None)` = not found.
    fn read_blob(account: &str, shared: bool) -> Result<Option<Vec<u8>>, String> {
        let account = CFString::new(account).as_CFType();
        // SAFETY: as in `store_blob` — valid constants, the dictionary outlives
        // the call, and the returned CFData is owned (create rule).
        unsafe {
            let mut attrs = vec![
                (constant(kSecClass), constant(kSecClassGenericPassword)),
                (constant(kSecAttrAccount), account),
                (
                    constant(kSecReturnData),
                    CFBoolean::true_value().as_CFType(),
                ),
                (constant(kSecMatchLimit), constant(kSecMatchLimitOne)),
            ];
            if shared {
                attrs.push((constant(kSecAttrAccessGroup), access_group()?));
            }
            let query = CFDictionary::from_CFType_pairs(&attrs);
            let mut out: CFTypeRef = std::ptr::null();
            let status = SecItemCopyMatching(query.as_concrete_TypeRef(), &mut out);
            if status != ERR_SEC_SUCCESS || out.is_null() {
                return Ok(None);
            }
            let data = CFData::wrap_under_create_rule(out as CFDataRef);
            Ok(Some(data.bytes().to_vec()))
        }
    }

    /// Delete an item from the keychain. A missing item (`errSecItemNotFound`) is
    /// treated as success — the unpair path is idempotent.
    fn delete_blob(account: &str, shared: bool) -> Result<(), String> {
        let account = CFString::new(account).as_CFType();
        // SAFETY: as in `store_blob` — valid constants, the dictionary outlives
        // the call.
        unsafe {
            let mut attrs = vec![
                (constant(kSecClass), constant(kSecClassGenericPassword)),
                (constant(kSecAttrAccount), account),
            ];
            if shared {
                attrs.push((constant(kSecAttrAccessGroup), access_group()?));
            }
            let query = CFDictionary::from_CFType_pairs(&attrs);
            let status = SecItemDelete(query.as_concrete_TypeRef());
            if status == ERR_SEC_SUCCESS || status == ERR_SEC_ITEM_NOT_FOUND {
                Ok(())
            } else {
                Err(format!("SecItemDelete failed (OSStatus {status})"))
            }
        }
    }

    pub(super) fn store_private_blob(account: &str, bytes: &[u8]) -> Result<(), String> {
        store_blob(account, bytes, false, Accessibility::DeviceOnly)
    }

    pub(super) fn read_private_blob(account: &str) -> Result<Option<Vec<u8>>, String> {
        read_blob(account, false)
    }

    pub(super) fn delete_private_blob(account: &str) -> Result<(), String> {
        delete_blob(account, false)
    }

    pub(super) fn store_push_key(bid: &str, key: &[u8; KEY_LEN]) -> Result<(), String> {
        store_blob(
            &format!("{ACCOUNT_PREFIX}{bid}"),
            &key[..],
            true,
            Accessibility::Shared,
        )
    }

    pub(super) fn delete_push_key(bid: &str) -> Result<(), String> {
        delete_blob(&format!("{ACCOUNT_PREFIX}{bid}"), true)
    }

    /// Read the push key back — the same lookup the NSE's `PushKeyStore` does.
    /// Backs `load_or_create_push_key` (so a re-register reuses the existing key)
    /// and the debug access-group self-check.
    pub(super) fn read_push_key(bid: &str) -> Result<Option<[u8; KEY_LEN]>, String> {
        match read_blob(&format!("{ACCOUNT_PREFIX}{bid}"), true)? {
            Some(b) => b
                .as_slice()
                .try_into()
                .map(Some)
                .map_err(|_| "stored push key has the wrong length".to_string()),
            None => Ok(None),
        }
    }
}

#[cfg(not(target_os = "ios"))]
mod imp {
    use super::KEY_LEN;
    pub(super) fn store_push_key(_bid: &str, _key: &[u8; KEY_LEN]) -> Result<(), String> {
        Ok(())
    }
    pub(super) fn read_push_key(_bid: &str) -> Result<Option<[u8; KEY_LEN]>, String> {
        Ok(None)
    }
    pub(super) fn store_private_blob(_account: &str, _bytes: &[u8]) -> Result<(), String> {
        Ok(())
    }
    pub(super) fn read_private_blob(_account: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }
    pub(super) fn delete_private_blob(_account: &str) -> Result<(), String> {
        Ok(())
    }
    pub(super) fn delete_push_key(_bid: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Write the device's push key to the shared App Group keychain at account
/// `baybo.push-key.<bid>`. `bid` is the device id the gateway stamps into every
/// push payload, so the NSE can look the key back up.
pub fn store_push_key(bid: &str, key: &[u8; KEY_LEN]) -> Result<(), String> {
    imp::store_push_key(bid, key)
}

/// Load the device's push key from the shared App-Group keychain, minting +
/// persisting a fresh random one on first use. Read back rather than regenerated,
/// so it is **stable** across re-registers — the NSE never holds a key that
/// mismatches an in-flight push, and re-registering on each foreground is a no-op.
/// Mirrors [`load_or_create_device_sign_key`]. (On the non-iOS dev build there is
/// no keychain, so the read is always empty and a fresh ephemeral key is minted
/// per call — fine off-device, where push never fires.)
pub fn load_or_create_push_key(bid: &str) -> Result<[u8; KEY_LEN], String> {
    if let Some(key) = imp::read_push_key(bid)? {
        return Ok(key);
    }
    let key = device_proto::delegation::generate_signing_key().to_bytes();
    imp::store_push_key(bid, &key)?;
    Ok(key)
}

/// Account holding the serialized paired-gateway record — the content-session
/// material (auth token, gateway static key, routing candidates, and the app's
/// Noise static secret) the app needs to reconnect after a relaunch.
const PAIRED_RECORD_ACCOUNT: &str = "baybo.paired-gateway";

/// Persist the paired-gateway record (an opaque serialized blob).
pub fn store_paired_record(bytes: &[u8]) -> Result<(), String> {
    imp::store_private_blob(PAIRED_RECORD_ACCOUNT, bytes)
}

/// Read the persisted paired-gateway record. `Ok(None)` = not paired yet.
pub fn read_paired_record() -> Result<Option<Vec<u8>>, String> {
    imp::read_private_blob(PAIRED_RECORD_ACCOUNT)
}

/// Delete the persisted paired-gateway record (the unpair / "forget" action).
/// Idempotent — succeeds even if nothing was stored.
pub fn delete_paired_record() -> Result<(), String> {
    imp::delete_private_blob(PAIRED_RECORD_ACCOUNT)
}

/// Account holding the device's long-term Noise static identity, stored as the
/// 32-byte secret followed by its 32-byte X25519 public (64 bytes). This is what
/// makes the derived `device_id` (`ios-<public[..8]>`) stable across re-pairings
/// and launches: pairing loads this key instead of minting a fresh one each
/// time. Kept in its own account (not only inside the paired record) so it
/// survives an unpair/"forget" and exists before the first pairing completes.
const DEVICE_IDENTITY_ACCOUNT: &str = "baybo.device-identity";

/// A persisted Noise static identity as `(secret, public)`, each `KEY_LEN` bytes.
pub type DeviceIdentity = ([u8; KEY_LEN], [u8; KEY_LEN]);

/// Persist the device's Noise static identity (`secret` ‖ `public`).
pub fn store_device_identity(secret: &[u8; KEY_LEN], public: &[u8; KEY_LEN]) -> Result<(), String> {
    let mut blob = Vec::with_capacity(KEY_LEN * 2);
    blob.extend_from_slice(secret);
    blob.extend_from_slice(public);
    imp::store_private_blob(DEVICE_IDENTITY_ACCOUNT, &blob)
}

/// Read the persisted identity back as `(secret, public)`. `Ok(None)` = unset
/// (first launch, or a desktop dev build with no on-device keychain).
pub fn read_device_identity() -> Result<Option<DeviceIdentity>, String> {
    let Some(blob) = imp::read_private_blob(DEVICE_IDENTITY_ACCOUNT)? else {
        return Ok(None);
    };
    if blob.len() != KEY_LEN * 2 {
        return Err("stored device identity has the wrong length".to_string());
    }
    let mut secret = [0u8; KEY_LEN];
    let mut public = [0u8; KEY_LEN];
    secret.copy_from_slice(&blob[..KEY_LEN]);
    public.copy_from_slice(&blob[KEY_LEN..]);
    Ok(Some((secret, public)))
}

/// Account holding the device's long-term **Ed25519 push-delegation identity**
/// (a 32-byte seed), kept in the app's private keychain. Its public half is the
/// `device_id` (`ios-<hex(pub)>`), and at pairing the app signs a delegation
/// with it authorizing the gateway's push key — so it must be stable across
/// re-pairings (a re-pair under the same physical device keeps the same id).
const DEVICE_SIGN_KEY_ACCOUNT: &str = "baybo.device-sign-key";

/// Persist the device's Ed25519 push-signing seed (32 bytes).
pub fn store_device_sign_key(seed: &[u8; KEY_LEN]) -> Result<(), String> {
    imp::store_private_blob(DEVICE_SIGN_KEY_ACCOUNT, seed)
}

/// Read the persisted Ed25519 push-signing seed. `Ok(None)` = unset (first
/// launch, or a desktop dev build with no on-device keychain).
pub fn read_device_sign_key() -> Result<Option<[u8; KEY_LEN]>, String> {
    match imp::read_private_blob(DEVICE_SIGN_KEY_ACCOUNT)? {
        Some(b) => b
            .as_slice()
            .try_into()
            .map(Some)
            .map_err(|_| "stored device sign key has the wrong length".to_string()),
        None => Ok(None),
    }
}

/// Load the device's Ed25519 push-delegation identity, minting + persisting one
/// on first use. Its public half IS the `device_id` (`ios-<hex(pub)>`). Shared by
/// the relay (scan-to-pair) and direct (push-register) paths so a phone has ONE
/// stable push identity regardless of how it connects — both store under
/// [`DEVICE_SIGN_KEY_ACCOUNT`], so the `device_id` and its `baybo.push-key.<id>`
/// keychain entry line up across modes.
pub fn load_or_create_device_sign_key() -> Result<device_proto::delegation::SigningKey, String> {
    use device_proto::delegation;
    if let Some(seed) = read_device_sign_key()? {
        return Ok(delegation::SigningKey::from_bytes(&seed));
    }
    let key = delegation::generate_signing_key();
    store_device_sign_key(&key.to_bytes())?;
    Ok(key)
}

/// Account holding the **direct-connection** credentials (a serialized
/// `{base_url, token}`): the gateway base URL + admin Bearer token entered on the
/// direct-login screen, the web-dashboard style of access. The admin token is a
/// broad credential, so it lives in the app's PRIVATE keychain (not the shared
/// App Group) and is wiped on disconnect.
const DIRECT_CREDENTIALS_ACCOUNT: &str = "baybo.direct-credentials";

/// Persist the direct-connection credentials (an opaque serialized blob).
pub fn store_direct_credentials(bytes: &[u8]) -> Result<(), String> {
    imp::store_private_blob(DIRECT_CREDENTIALS_ACCOUNT, bytes)
}

/// Read the persisted direct-connection credentials. `Ok(None)` = not connected.
pub fn read_direct_credentials() -> Result<Option<Vec<u8>>, String> {
    imp::read_private_blob(DIRECT_CREDENTIALS_ACCOUNT)
}

/// Delete the direct-connection credentials (the direct "disconnect" action).
/// Idempotent — succeeds even if nothing was stored.
pub fn delete_direct_credentials() -> Result<(), String> {
    imp::delete_private_blob(DIRECT_CREDENTIALS_ACCOUNT)
}

/// Account holding the **active-binding marker** (`"direct"` / `"relay"`): which
/// bind happened most recently. "One app binds one Baybo", so the two credential
/// sets are mutually exclusive — but the losing side's cleanup is best-effort, so a
/// keychain hiccup can transiently leave both present. This marker records the
/// intended binding so the resolver ([`crate::binding`]) breaks that tie correctly
/// instead of guessing by static precedence (which could pick the stale one).
const ACTIVE_BINDING_ACCOUNT: &str = "baybo.active-binding";

/// Record the most-recently-bound leg (written by `direct::login` / `finish_pair`).
pub fn store_active_binding(kind: &str) -> Result<(), String> {
    imp::store_private_blob(ACTIVE_BINDING_ACCOUNT, kind.as_bytes())
}

/// Read the active-binding marker, if one was ever written. `Ok(None)` on a legacy
/// install that bound before the marker existed — the resolver then falls back.
pub fn read_active_binding() -> Result<Option<String>, String> {
    Ok(imp::read_private_blob(ACTIVE_BINDING_ACCOUNT)?.map(|b| String::from_utf8_lossy(&b).into()))
}

/// Delete a device's push key from the shared keychain (the write side is
/// [`store_push_key`]). Called on unpair so a stale per-device key can't linger.
pub fn delete_push_key(bid: &str) -> Result<(), String> {
    imp::delete_push_key(bid)
}

/// Debug self-check: read the key back (the same lookup the NSE does) to prove
/// the access-group round-trip works on-device. iOS debug builds only.
#[cfg(all(debug_assertions, target_os = "ios"))]
pub fn read_push_key(bid: &str) -> Result<Option<[u8; KEY_LEN]>, String> {
    imp::read_push_key(bid)
}
