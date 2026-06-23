// NotificationService.swift — the Aura push Notification Service Extension (NSE).
//
// APNs delivers a generic, visible "New message" alert with `mutable-content: 1`
// plus the operator-encrypted lock-screen preview (`enc`/`n`/`kid`/`bid`). This
// extension wakes, decrypts the preview *locally* with CryptoKit, and rewrites
// the visible title/body. It never touches the network and holds no remote-host
// (C) credential — C relayed the ciphertext blind.
//
// The crypto MUST match `aura_device_proto::aead` byte-for-byte:
//   ChaCha20-Poly1305, 32-byte push key, 12-byte nonce carried as `n`,
//   ciphertext = `ciphertext || 16-byte tag`, empty AAD.
// The interop vector is pinned in `aura_device_proto::fixtures` and asserted by
// `NotificationServiceTests` here — a drift on either side fails a test.

import CryptoKit
import Foundation
import UserNotifications

final class NotificationService: UNNotificationServiceExtension {
    private var contentHandler: ((UNNotificationContent) -> Void)?
    private var bestAttempt: UNMutableNotificationContent?

    override func didReceive(
        _ request: UNNotificationRequest,
        withContentHandler contentHandler: @escaping (UNNotificationContent) -> Void
    ) {
        self.contentHandler = contentHandler
        let mutable = request.content.mutableCopy() as? UNMutableNotificationContent
        bestAttempt = mutable
        guard let mutable else {
            contentHandler(request.content)
            return
        }

        // Decrypt; on ANY failure keep the generic placeholder. A bad key,
        // wrong nonce, or tampered tag are indistinguishable, and the only safe
        // response is to show "New message" rather than crash or leak.
        if let preview = Self.decryptPreview(userInfo: request.content.userInfo) {
            mutable.title = preview.title
            mutable.body = preview.body
        }
        contentHandler(mutable)
    }

    override func serviceExtensionTimeWillExpire() {
        if let handler = contentHandler, let content = bestAttempt {
            handler(content)
        }
    }

    /// The decrypted preview shape A encrypts (see the pinned `PLAINTEXT`).
    struct Preview: Decodable {
        let title: String
        let body: String
    }

    /// Parse the APNs payload, load the push key for `bid`, and decrypt.
    /// Returns nil on any missing field / lookup miss / crypto failure.
    static func decryptPreview(userInfo: [AnyHashable: Any]) -> Preview? {
        guard
            let encB64 = userInfo["enc"] as? String,
            let nonceB64 = userInfo["n"] as? String,
            let bid = userInfo["bid"] as? String,
            let enc = Data(base64Encoded: encB64),
            let nonce = Data(base64Encoded: nonceB64),
            let key = PushKeyStore.pushKey(forBinding: bid)
        else { return nil }
        return open(key: key, nonce: nonce, ciphertextAndTag: enc)
    }

    /// CryptoKit ChaChaPoly open matching `aura_device_proto::aead::open`.
    /// `combined = nonce(12) || ciphertext || tag(16)`, empty AAD. Static + pure
    /// so `NotificationServiceTests` drives it with the pinned fixture.
    static func open(key: SymmetricKey, nonce: Data, ciphertextAndTag: Data) -> Preview? {
        guard nonce.count == 12, ciphertextAndTag.count >= 16 else { return nil }
        guard
            let box = try? ChaChaPoly.SealedBox(combined: nonce + ciphertextAndTag),
            let plaintext = try? ChaChaPoly.open(box, using: key)
        else { return nil }
        return try? JSONDecoder().decode(Preview.self, from: plaintext)
    }
}
