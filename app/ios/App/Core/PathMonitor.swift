import Foundation
import Network

/// Hands every `NWPathMonitor` delivery to the core's `network_changed`, which
/// owns everything decided about it: whether it is a duplicate, whether a
/// primary-interface change retires the direct carrier, and when the settled
/// path is probed (`docs/modules/mobile/direct-carriers.md` § Connection
/// policy on P). This side only translates `NWPath` into `NetworkPath`.
///
/// The core call is synchronous on purpose: a primary-interface change retires
/// the carrier and aborts any probe before it returns, so no leg dials a
/// carrier on the network the phone just left. It is cheap, so it runs on the
/// monitor's own serial queue — in delivery order, and never on the main actor.
///
/// Started once at launch and never stopped: the carrier needs every change,
/// foreground or not, and a stopped `NWPathMonitor` cannot be restarted.
enum PathMonitor {
    /// A `static let` so `start()` is idempotent: Swift runs the initializer
    /// exactly once, thread-safely, and `NWPathMonitor` delivers the current
    /// path as soon as it starts.
    private static let monitor: NWPathMonitor = {
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { path in
            Baybo.client.networkChanged(path: PathMonitor.networkPath(from: path))
        }
        monitor.start(queue: DispatchQueue(label: "baybo.path-monitor"))
        return monitor
    }()

    static func start() {
        _ = monitor
    }

    /// The primary interface is the path's first available one — the one
    /// `NWPath` itself prefers.
    static func networkPath(from path: NWPath) -> NetworkPath {
        let primary = path.availableInterfaces.first
        return NetworkPath(
            satisfied: path.status == .satisfied,
            interfaceKind: primary.map { interfaceKind($0.type) } ?? .other,
            interfaceName: primary?.name ?? "",
            gateways: path.gateways.compactMap(gatewayHost),
            supportsIpv4: path.supportsIPv4,
            supportsIpv6: path.supportsIPv6,
            availableInterfaces: path.availableInterfaces.map(\.name),
            isExpensive: path.isExpensive,
            isConstrained: path.isConstrained)
    }

    private static func interfaceKind(_ type: NWInterface.InterfaceType) -> NetworkInterfaceKind {
        switch type {
        case .wifi: return .wifi
        case .wiredEthernet: return .wired
        case .cellular: return .cellular
        case .loopback: return .loopback
        default: return .other
        }
    }

    /// The core only compares gateways (and hashes them into the failure
    /// cache's network key), so any stable rendering of the host will do; the
    /// port is dropped because it says nothing about which network this is.
    private static func gatewayHost(_ endpoint: NWEndpoint) -> String? {
        guard case let .hostPort(host, _) = endpoint else { return nil }
        return host.debugDescription
    }
}
