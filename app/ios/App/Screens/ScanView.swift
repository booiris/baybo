import SwiftUI
import VisionKit

/// The native QR scan sheet, replacing the Tauri windowed-scanner hack (camera
/// drawn behind a transparent webview). QR symbology only — matching the old
/// `scan({ windowed: true, formats: [QRCode] })` restriction.
struct ScanView: View {
    @EnvironmentObject private var store: AppStore
    @Environment(\.dismiss) private var dismiss
    @State private var deniedCamera = false

    var body: some View {
        ZStack(alignment: .bottom) {
            if deniedCamera {
                VStack(spacing: 16) {
                    Text(verbatim: Lang.shared.t("scan.permissionOff"))
                        .font(Theme.mono(13))
                        .foregroundStyle(Theme.inkSoft)
                        .multilineTextAlignment(.center)
                    Button {
                        if let url = URL(string: UIApplication.openSettingsURLString) {
                            UIApplication.shared.open(url)
                        }
                    } label: {
                        Text(verbatim: "Settings")
                    }
                    .buttonStyle(OutlinePillButtonStyle())
                    .frame(width: 200)
                }
                .padding(28)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.paper)
            } else if DataScannerViewController.isSupported {
                QrScannerRepresentable(
                    onScan: { payload in store.handleScan(payload: payload) },
                    onUnavailable: { deniedCamera = true }
                )
                .ignoresSafeArea()

                // Thin rounded reticle, mirroring the web overlay's viewfinder.
                RoundedRectangle(cornerRadius: 24)
                    .strokeBorder(.white.opacity(0.9), lineWidth: 1.5)
                    .frame(width: 240, height: 240)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .allowsHitTesting(false)
            } else {
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
    }
}

/// DataScannerViewController wrapper: QR-only, one shot — the first recognized
/// payload stops the scanner and fires `onScan`.
private struct QrScannerRepresentable: UIViewControllerRepresentable {
    let onScan: (String) -> Void
    let onUnavailable: () -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .fast,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        context.coordinator.scanner = scanner
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        guard !context.coordinator.started else { return }
        context.coordinator.started = true
        do {
            try scanner.startScanning()
        } catch {
            onUnavailable()
        }
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onScan: onScan)
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onScan: (String) -> Void
        var started = false
        private var fired = false
        weak var scanner: DataScannerViewController?

        init(onScan: @escaping (String) -> Void) {
            self.onScan = onScan
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
    }
}
