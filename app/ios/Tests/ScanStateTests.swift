import AVFoundation
import Testing

@testable import Baybo

/// What the scan sheet shows. The bug this pins: ANY failure to start the
/// scanner used to read as "camera permission is off", with a Settings button
/// for a switch that was already on — and the scanner never tried again. A
/// camera held by another app, or withheld from a multitasking window, is not a
/// permission problem.
@Suite
struct ScanStateTests {
    @Test func noScannerIsUnsupportedWhateverElseHolds() {
        #expect(
            ScanState.resolve(isSupported: false, isAvailable: true, authorization: .authorized)
                == .unsupported)
        #expect(
            ScanState.resolve(isSupported: false, isAvailable: false, authorization: .denied)
                == .unsupported)
    }

    @Test(arguments: [AVAuthorizationStatus.denied, .restricted])
    func aRefusedPermissionIsDenied(status: AVAuthorizationStatus) {
        #expect(
            ScanState.resolve(isSupported: true, isAvailable: false, authorization: status)
                == .denied)
    }

    /// Authorized but not available right now: busy, not denied.
    @Test func anAuthorizedButBusyCameraIsUnavailable() {
        #expect(
            ScanState.resolve(isSupported: true, isAvailable: false, authorization: .authorized)
                == .unavailable)
    }

    @Test func anAuthorizedFreeCameraScans() {
        #expect(
            ScanState.resolve(isSupported: true, isAvailable: true, authorization: .authorized)
                == .scanning)
    }

    /// Never asked: start the scanner, which raises the prompt — whatever
    /// `isAvailable` claims before the answer is in.
    @Test func anUnaskedPermissionStartsTheScanner() {
        #expect(
            ScanState.resolve(isSupported: true, isAvailable: false, authorization: .notDetermined)
                == .scanning)
    }
}
