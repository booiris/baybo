// NotificationServiceTests.swift — proves the CryptoKit NSE decrypt agrees with
// the Rust AEAD byte-for-byte, using the vector pinned in
// `device_proto::fixtures`. KEEP THESE CONSTANTS IN SYNC with that module:
// the Rust test `pinned_vector_is_reproduced` guards the producer side, this
// guards the consumer side, so any AEAD drift fails on one side or the other.

import CryptoKit
import XCTest

@testable import NotificationExtension

final class NotificationServiceTests: XCTestCase {
    // device_proto::fixtures::KEY  (0x00..=0x1f)
    static let key = Data((0x00...0x1f).map { UInt8($0) })
    // device_proto::fixtures::NONCE  (0xa0..=0xab)
    static let nonce = Data((0xa0...0xab).map { UInt8($0) })
    // device_proto::fixtures::CIPHERTEXT_HEX  (ciphertext || tag)
    static let ciphertextHex =
        "77890c36398aa78f9a2db175859892d9b17cb1d027134299968cb3d45103a469" +
        "02f436ecc7a796f9aea9919d6f34082a33ad245f272e1680e50d5ab76a47bcd8" +
        "968f87f6de8098"

    func testDecryptsThePinnedFixture() throws {
        let ct = try XCTUnwrap(Data(hexString: Self.ciphertextHex))
        let preview = NotificationService.open(
            key: SymmetricKey(data: Self.key),
            nonce: Self.nonce,
            ciphertextAndTag: ct
        )
        XCTAssertEqual(preview?.title, "Baybo")
        XCTAssertEqual(preview?.body, "The agent finished replying.")
    }

    func testWrongKeyFallsBackToNil() throws {
        let ct = try XCTUnwrap(Data(hexString: Self.ciphertextHex))
        let preview = NotificationService.open(
            key: SymmetricKey(data: Data(repeating: 0xff, count: 32)),
            nonce: Self.nonce,
            ciphertextAndTag: ct
        )
        XCTAssertNil(preview, "a bad key must yield nil (placeholder is kept)")
    }
}

private extension Data {
    /// Decode an even-length hex string, or nil on malformed input.
    init?(hexString: String) {
        let chars = Array(hexString)
        guard chars.count % 2 == 0 else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(chars.count / 2)
        var i = 0
        while i < chars.count {
            guard let b = UInt8(String(chars[i ... i + 1]), radix: 16) else { return nil }
            bytes.append(b)
            i += 2
        }
        self.init(bytes)
    }
}
