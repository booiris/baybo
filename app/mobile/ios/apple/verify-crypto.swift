// verify-crypto.swift — reproducible NSE↔Rust AEAD interop check.
//
// Runs the NSE's crypto-critical decrypt (the same CryptoKit ChaChaPoly path as
// NotificationService.open) against the vector pinned in
// `device_proto::fixtures`, with NO Xcode / iOS SDK — just CryptoKit, which
// ships with macOS. Proves the Swift consumer agrees with the Rust producer
// byte-for-byte.
//
//   $ swift app/mobile/ios/apple/verify-crypto.swift
//
// Pairs with the Rust test `device_proto::fixtures::pinned_vector_is_reproduced`.
// If the AEAD output ever drifts, update CIPHERTEXT_HEX in BOTH places together.

import CryptoKit
import Foundation

struct Preview: Decodable { let title: String; let body: String }

/// Identical to `NotificationService.open`: combined = nonce(12) || ct || tag(16),
/// empty AAD.
func openPreview(key: SymmetricKey, nonce: Data, ciphertextAndTag: Data) -> Preview? {
    guard nonce.count == 12, ciphertextAndTag.count >= 16 else { return nil }
    guard let box = try? ChaChaPoly.SealedBox(combined: nonce + ciphertextAndTag),
          let pt = try? ChaChaPoly.open(box, using: key) else { return nil }
    return try? JSONDecoder().decode(Preview.self, from: pt)
}

extension Data {
    init?(hexString: String) {
        let chars = Array(hexString)
        guard chars.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        var i = 0
        while i < chars.count {
            guard let b = UInt8(String(chars[i ... i + 1]), radix: 16) else { return nil }
            bytes.append(b)
            i += 2
        }
        self.init(bytes)
    }
}

// device_proto::fixtures — KEY (0x00..=0x1f), NONCE (0xa0..=0xab), CIPHERTEXT_HEX.
let key = Data((0x00 ... 0x1f).map { UInt8($0) })
let nonce = Data((0xa0 ... 0xab).map { UInt8($0) })
let ciphertextHex =
    "77890c36398aa78f9a2db2618e9bdfd7bf3cbcdb3a485a81e0b0be911005a662" +
    "18a070e3c0a08ce2a3a8d5cf7821143f23aa2d162b71a68f69fd2b8d48cff9e1" +
    "cc8670a8738b"

guard let ct = Data(hexString: ciphertextHex) else {
    print("FAIL: bad fixture hex"); exit(1)
}
guard let p = openPreview(key: SymmetricKey(data: key), nonce: nonce, ciphertextAndTag: ct) else {
    print("FAIL: decrypt returned nil"); exit(1)
}
guard p.title == "Aura", p.body == "The agent finished replying." else {
    print("FAIL: got title=\(p.title) body=\(p.body)"); exit(1)
}
print("PASS: CryptoKit decrypt matches the Rust AEAD fixture (title=\(p.title), body=\(p.body))")

// A wrong key must fall back to nil (NSE keeps the generic placeholder).
guard openPreview(
    key: SymmetricKey(data: Data(repeating: 0xff, count: 32)),
    nonce: nonce,
    ciphertextAndTag: ct
) == nil else {
    print("FAIL: wrong key did not fail"); exit(1)
}
print("PASS: wrong key -> nil (placeholder kept)")
