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

    /// Fetch the 32-byte push key for a binding id, or nil if absent / wrong size.
    static func pushKey(forBinding bid: String) -> SymmetricKey? {
        guard let accessGroup else {
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
        guard status == errSecSuccess, let data = item as? Data, data.count == 32 else {
            return nil
        }
        return SymmetricKey(data: data)
    }
}
