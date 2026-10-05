import Foundation

/// What a relay binding's legs ride on — the relay, or a direct carrier — and
/// how the last probe for one went, as the core last reported it
/// (`docs/modules/mobile/direct-carriers.md` § Observability). The Settings
/// screen's Connection row and its details read it; the chat screen
/// deliberately shows no carrier at all.
///
/// In memory only, like the core's carrier state. `CarrierEventsRelay` is its
/// one writer, and the core reports every change through it, including the
/// reset that pairing and forgetting perform — so this never needs clearing
/// on a binding change of its own.
@MainActor
final class ConnectionStore: ObservableObject {
    static let shared = ConnectionStore()

    /// `nil` only until the core's first report, which `setCarrierSink`
    /// delivers at once.
    @Published private(set) var status: CarrierStatus?

    func apply(_ status: CarrierStatus) {
        self.status = status
    }
}

/// The core's carrier sink. Registered once at launch (`AppStore` →
/// `setCarrierSink`); hops each report to the main actor. NOT named
/// `CarrierSinkImpl` — UniFFI generates a class by that exact name for the
/// `with_foreign` trait.
final class CarrierEventsRelay: CarrierSink {
    func onCarrier(status: CarrierStatus) {
        #if DEBUG
        if ProcessInfo.processInfo.arguments.contains("-baybo-demo-connection") { return }
        #endif
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                ConnectionStore.shared.apply(status)
            }
        }
    }
}
