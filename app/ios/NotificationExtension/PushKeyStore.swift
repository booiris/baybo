// PushKeyStore.swift — read the per-binding push key from the shared keychain.
//
// At pairing the app derives the 32-byte push key and writes it to the App
// shared keychain access group. The NSE only *reads* it,
// so a preview can be decrypted without waking the host app. Key isolation: the
// push key never leaves the device and is scoped to the shared keychain group,
// so only the app + its extensions can read it.

import CryptoKit
import Foundation
import Security
import os

enum PushKeyStore {
    /// Keychain account prefix; the per-binding key is stored at
    /// `accountPrefix + bid`. The host app writes the same account at pairing.
    static let accountPrefix = "baybo.push-key."
    static let accessGroupInfoKey = "BayboKeychainAccessGroup"

    /// The keychain access group. MUST match the `keychain-access-groups`
    /// entitlement shared by the app target and this extension target.
    static var accessGroup: String? {
        guard let value = Bundle.main.object(forInfoDictionaryKey: accessGroupInfoKey) as? String,
              !value.isEmpty
        else {
            return nil
        }
        return value
    }

    /// Why a lookup produced no key. Mirrors the Rust core's
    /// `keychain::classify_read`: **absence and failure are different answers**
    /// and must not collapse into one another.
    ///
    /// Unlike the app side, no case here changes what the NSE DOES — every one
    /// delivers the unencrypted fallback notification, which is the only thing
    /// available on the lock screen. The distinction is for diagnosis: these
    /// four states are indistinguishable to a user reporting "previews stopped
    /// working", and this process cannot reach the app's file logger.
    enum Miss: Equatable, Error {
        /// No key stored for this binding — never push-registered here, or the
        /// binding was forgotten. A steady state, not a fault.
        case absent
        /// The read itself failed. On the lock screen the likely status is
        /// `errSecInteractionNotAllowed` (-25308): the key is
        /// `kSecAttrAccessibleAfterFirstUnlock`, so a device that rebooted and
        /// has not been unlocked since cannot serve it. Transient — the next
        /// push after an unlock decrypts normally, so this must never be read
        /// as "this device has no key".
        case readFailed(OSStatus)
        /// Stored, but not a 32-byte key.
        case malformed(byteCount: Int)
        /// Success with no data — an API-contract violation, never expected.
        case noData
        /// No `BayboKeychainAccessGroup` in this extension's `Info.plist`, so
        /// the query names no group and can never match. A packaging fault that
        /// breaks EVERY preview, not a device state.
        case noAccessGroup
    }

    /// The classification, pure so `BayboTests` can drive it with no keychain —
    /// the same reason the Rust side splits `classify_read` out.
    static func classify(status: OSStatus, item: Any?) -> Result<SymmetricKey, Miss> {
        if status == errSecItemNotFound {
            return .failure(.absent)
        }
        guard status == errSecSuccess else {
            return .failure(.readFailed(status))
        }
        guard let data = item as? Data else {
            return .failure(.noData)
        }
        guard data.count == 32 else {
            return .failure(.malformed(byteCount: data.count))
        }
        return .success(SymmetricKey(data: data))
    }

    /// Fetch the 32-byte push key for a binding id. `nil` = no key available,
    /// with the reason logged — see [Miss].
    static func pushKey(forBinding bid: String) -> SymmetricKey? {
        guard let accessGroup else {
            log(.noAccessGroup)
            return nil
        }
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrAccount as String: accountPrefix + bid,
            kSecAttrAccessGroup as String: accessGroup,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        switch classify(status: status, item: item) {
        case let .success(key):
            return key
        case let .failure(miss):
            log(miss)
            return nil
        }
    }

    /// `os_log` is the only channel available: this is a separate process on the
    /// lock screen with no reach into the app's file logger. **Never log the key
    /// or the binding id** — only which outcome, and the raw status.
    private static let logger = Logger(
        subsystem: "com.baybo.app.NotificationExtension",
        category: "pushkey"
    )

    private static func log(_ miss: Miss) {
        switch miss {
        case .absent:
            logger.info("push key absent for this binding; delivering the fallback preview")
        case let .readFailed(status):
            logger.error(
                "push key read failed (OSStatus \(status, privacy: .public)) — transient if the device has not been unlocked since boot"
            )
        case let .malformed(byteCount):
            logger.error("push key is \(byteCount, privacy: .public) bytes, expected 32")
        case .noData:
            logger.error("push key read reported success with no data")
        case .noAccessGroup:
            logger.error("no BayboKeychainAccessGroup in the NSE Info.plist; every preview will fail")
        }
    }
}
