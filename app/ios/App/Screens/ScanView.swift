import AVFoundation
import SwiftUI
import VisionKit

/// What the scan sheet can show, decided from the scanner's own availability
/// and the camera permission. Pure so the decision is testable without a
/// camera.
///
/// `denied` and `unavailable` are kept apart on purpose. A camera can be
/// refused for reasons that have nothing to do with permission — another app
/// holding it, a multitasking window the camera is withheld from (Split View
/// on a foldable's inner display) — and answering those with "permission is
/// off, open Settings" sends the user to a switch that is already on, while
/// the scanner never tries again.
enum ScanState: Equatable {
    case scanning
    /// The user (or a restriction) turned the camera off for Baybo: only
    /// Settings can change that.
    case denied
    /// The camera exists and may be used, but not right now. Retried.
    case unavailable
    /// No scanner on this hardware (the simulator, old devices).
    case unsupported

    static func resolve(
        isSupported: Bool, isAvailable: Bool, authorization: AVAuthorizationStatus
    ) -> ScanState {
        guard isSupported else { return .unsupported }
        switch authorization {
        case .denied, .restricted:
            return .denied
        // Not asked yet: the scanner raises the prompt itself when it starts,
        // and `isAvailable` cannot be trusted until the answer is in.
        case .notDetermined:
            return .scanning
        default:
            return isAvailable ? .scanning : .unavailable
        }
    }

    @MainActor static func current() -> ScanState {
        resolve(
            isSupported: DataScannerViewController.isSupported,
            isAvailable: DataScannerViewController.isAvailable,
            authorization: AVCaptureDevice.authorizationStatus(for: .video))
    }
}

/// The native QR scan sheet: a full-bleed `DataScannerViewController` under a
/// reticle overlay. QR symbology only.
struct ScanView: View {
    @EnvironmentObject private var store: AppStore
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var scenePhase
    @State private var state: ScanState
    /// Bumped to ask the scanner to start again — the representable compares it
    /// against the attempt it last ran, so a retry is an input, not a latch.
    @State private var attempt = 0

    /// How often an unavailable camera is tried again while the sheet waits.
    /// Nothing announces a camera being released (the scanner owns the capture
    /// session), so the sheet asks.
    private static let retryInterval: Duration = .seconds(2)

    init() {
        _state = State(initialValue: ScanState.current())
    }

    var body: some View {
        ZStack(alignment: .bottom) {
            switch state {
            case .scanning, .unavailable:
                // Stays mounted while unavailable, so a retry restarts the same
                // scanner instead of rebuilding it under the notice.
                QrScannerRepresentable(
                    attempt: attempt,
                    onScan: { payload in store.handleScan(payload: payload) },
                    onStarted: { state = .scanning },
                    onUnavailable: { state = Self.failedState() }
                )
                .ignoresSafeArea()

                // Thin rounded reticle, mirroring the web overlay's viewfinder.
                RoundedRectangle(cornerRadius: 24)
                    .strokeBorder(.white.opacity(0.9), lineWidth: 1.5)
                    .frame(width: 240, height: 240)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .allowsHitTesting(false)

                if state == .unavailable {
                    notice(Lang.shared.t("scan.cameraUnavailable"))
                }
            case .denied:
                notice(Lang.shared.t("scan.permissionOff")) {
                    if let url = URL(string: UIApplication.openSettingsURLString) {
                        UIApplication.shared.open(url)
                    }
                }
            case .unsupported:
                // Simulator / devices without the scanner: nothing to scan.
                Text(verbatim: Lang.shared.t("scan.notBaybo"))
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(Theme.paper)
            }

            Button {
                dismiss()
            } label: {
                Text(verbatim: Lang.shared.t("scan.cancel"))
                    .font(Theme.mono(14, weight: .bold))
                    .textCase(.uppercase)
                    .kerning(2)
                    .foregroundStyle(.white)
                    .padding(.horizontal, 30)
                    .padding(.vertical, 12)
                    .background(.black.opacity(0.55), in: Capsule())
            }
            .padding(.bottom, 34)
        }
        // Coming back from Settings or from the app that held the camera is the
        // likeliest moment for the answer to have changed.
        .onChange(of: scenePhase) { _, phase in
            guard phase == .active, state != .scanning else { return }
            retry()
        }
        .task(id: state) {
            guard state == .unavailable else { return }
            while !Task.isCancelled {
                do { try await Task.sleep(for: Self.retryInterval) } catch { return }
                retry()
            }
        }
    }

    /// Where a scanner that would not run lands. Permission is re-read at the
    /// moment of failure: a prompt answered "Don't Allow" fails the start that
    /// raised it, and that is a denial, not a busy camera.
    private static func failedState() -> ScanState {
        let next = ScanState.current()
        return next == .scanning ? .unavailable : next
    }

    private func retry() {
        let next = ScanState.current()
        switch next {
        case .scanning:
            attempt += 1
        case .denied, .unsupported:
            state = next
        case .unavailable:
            break
        }
    }

    /// A paper panel over the viewfinder, with the one action that can help.
    private func notice(_ text: String, settings: (() -> Void)? = nil) -> some View {
        VStack(spacing: 16) {
            Text(verbatim: text)
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
            if let settings {
                Button(action: settings) {
                    Text(verbatim: "Settings")
                }
                .buttonStyle(OutlinePillButtonStyle())
                .frame(width: 200)
            }
        }
        .padding(28)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.paper)
    }
}

/// DataScannerViewController wrapper: QR-only, one shot — the first recognized
/// payload stops the scanner and fires `onScan`.
private struct QrScannerRepresentable: UIViewControllerRepresentable {
    /// Each new value is one request to (re)start scanning.
    let attempt: Int
    let onScan: (String) -> Void
    let onStarted: () -> Void
    let onUnavailable: () -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .fast,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        let coordinator = context.coordinator
        coordinator.onUnavailable = onUnavailable
        guard coordinator.startedAttempt != attempt else { return }
        coordinator.startedAttempt = attempt
        // A camera the system handed back on its own is already running; the
        // retry only has to say so.
        if scanner.isScanning {
            DispatchQueue.main.async { onStarted() }
            return
        }
        do {
            try scanner.startScanning()
            DispatchQueue.main.async { onStarted() }
        } catch {
            // Out of the update pass: this writes the hosting view's state.
            DispatchQueue.main.async { onUnavailable() }
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onScan: onScan, onUnavailable: onUnavailable)
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onScan: (String) -> Void
        var onUnavailable: () -> Void
        /// The `attempt` the scanner was last started for; `nil` until the
        /// first update.
        var startedAttempt: Int?
        private var fired = false

        init(onScan: @escaping (String) -> Void, onUnavailable: @escaping () -> Void) {
            self.onScan = onScan
            self.onUnavailable = onUnavailable
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController, didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            guard !fired else { return }
            for item in addedItems {
                if case .barcode(let barcode) = item, let payload = barcode.payloadStringValue {
                    fired = true
                    dataScanner.stopScanning()
                    onScan(payload)
                    return
                }
            }
        }

        /// The camera went away mid-scan (another app took it, the window lost
        /// camera access) — or the permission prompt was refused.
        func dataScanner(
            _ dataScanner: DataScannerViewController,
            becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable
        ) {
            onUnavailable()
        }
    }
}
