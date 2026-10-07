import Foundation

@MainActor
protocol ConnectionDiagnosticsClient {
    func setConnectionDiagnostics(enabled: Bool)
    func drainConnectionDiagnostics() -> [ConnectionLogEntry]
}

extension BayboClient: ConnectionDiagnosticsClient {}

@MainActor
final class ConnectionDiagnosticsStore: ObservableObject {
    static let maxEntries = Int(connectionDiagnosticsCapacity())
    @Published private(set) var enabled = false
    @Published private(set) var entries: [ConnectionLogEntry] = []
    private let client: any ConnectionDiagnosticsClient

    init(client: any ConnectionDiagnosticsClient) {
        self.client = client
    }

    func setEnabled(_ enabled: Bool) {
        guard self.enabled != enabled else { return }
        if !enabled { poll() }
        self.enabled = enabled
        client.setConnectionDiagnostics(enabled: enabled)
        if enabled { poll() }
    }

    func poll() {
        guard enabled else { return }
        let incoming = client.drainConnectionDiagnostics()
        guard !incoming.isEmpty else { return }
        entries = Array((entries + incoming).suffix(Self.maxEntries))
    }

    func clear() {
        _ = client.drainConnectionDiagnostics()
        entries.removeAll()
    }

    func stop() {
        setEnabled(false)
        entries.removeAll()
    }

    var text: String { entries.map(Self.line).joined(separator: "\n") }

    static func line(_ entry: ConnectionLogEntry) -> String {
        "\(timestamp(entry)) [\(stage(entry.stage))] \(entry.message)"
    }

    static func timestamp(_ entry: ConnectionLogEntry) -> String {
        time.string(from: Date(timeIntervalSince1970: Double(entry.timestampMs) / 1000))
    }

    private static let time: DateFormatter = {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.dateFormat = "HH:mm:ss.SSS"
        return formatter
    }()

    static func stage(_ stage: ConnectionLogStage) -> String {
        switch stage {
        case .lifecycle: return "app"
        case .network: return "network"
        case .relay: return "relay"
        case .probe: return "probe"
        case .rendezvous: return "rendezvous"
        case .quic: return "quic"
        case .chat: return "chat"
        }
    }
}
