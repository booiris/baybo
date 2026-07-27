// PushKeyStoreTests.swift — the NSE's push-key lookup classifies absence,
// read failure, and corruption as DIFFERENT answers.
//
// The Swift half of the same rule the Rust core pins in
// `keychain::classify_read`'s tests. Neither side may report a failed keychain
// read as "no such item": on the app side that drives a mint-and-persist branch
// that rotates the device identity, and here it is the difference between "this
// device was never push-registered" and "the phone has not been unlocked since
// it rebooted" — two states a user reports identically as "previews stopped
// working", and this process has no file logger to tell them apart with.
//
// `PushKeyStore.swift` is compiled straight into this bundle (project.yml), for
// the reason spelled out in `NotificationServiceTests`.

import CryptoKit
import Foundation
import Testing

@Suite struct PushKeyStoreTests {
    static let thirtyTwoBytes = Data((0x00...0x1f).map { UInt8($0) })

    @Test func aStoredThirtyTwoByteKeyIsReturned() throws {
        let result = PushKeyStore.classify(status: errSecSuccess, item: Self.thirtyTwoBytes)
        let key = try result.get()
        #expect(key.withUnsafeBytes { Data($0) } == Self.thirtyTwoBytes)
    }

    @Test func itemNotFoundIsAbsenceNotFailure() {
        #expect(PushKeyStore.classify(status: errSecItemNotFound, item: nil) == .failure(.absent))
    }

    /// The regression this split exists for. `errSecInteractionNotAllowed` is
    /// what a locked keychain answers, and the NSE runs on the lock screen — so
    /// this is the COMMON failure here, not an exotic one. Reporting it as
    /// `.absent` would say the device has no key when it has a perfectly good
    /// one it simply cannot read yet.
    @Test func aReadFailureIsNeverReportedAsAbsence() {
        let statuses: [OSStatus] = [
            -25308,  // errSecInteractionNotAllowed — locked keychain before first unlock
            -34018,  // errSecMissingEntitlement — access group didn't apply
            -25291,  // errSecNotAvailable
            1,  // anything unrecognised
        ]
        for status in statuses {
            #expect(
                PushKeyStore.classify(status: status, item: nil) == .failure(.readFailed(status)),
                "OSStatus \(status) must classify as a read failure"
            )
        }
    }

    @Test func aWrongLengthKeyIsMalformedNotAbsent() {
        let short = Data(repeating: 7, count: 16)
        #expect(
            PushKeyStore.classify(status: errSecSuccess, item: short)
                == .failure(.malformed(byteCount: 16))
        )
    }

    @Test func successWithNoDataIsItsOwnAnswer() {
        #expect(PushKeyStore.classify(status: errSecSuccess, item: nil) == .failure(.noData))
    }
}
