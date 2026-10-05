import Network
import Testing

@testable import Baybo

@Suite struct PathMonitorTests {
    private typealias Interface = PathMonitor.Interface

    private let tunnel = Interface(name: "utun4", type: .other, isUsed: true)
    private let wifi = Interface(name: "en0", type: .wifi, isUsed: true)
    private let cellular = Interface(name: "pdp_ip0", type: .cellular, isUsed: true)
    private let wired = Interface(name: "en2", type: .wiredEthernet, isUsed: true)

    @Test func vpnOverWifiUsesWifiForCandidates() {
        #expect(PathMonitor.primaryInterface(in: [tunnel, wifi, cellular]) == wifi)
    }

    @Test func vpnOverCellularDoesNotSelectUnusedWifi() {
        let unusedWifi = Interface(name: "en0", type: .wifi, isUsed: false)
        #expect(PathMonitor.primaryInterface(in: [tunnel, unusedWifi, cellular]) == cellular)
    }

    @Test func vpnOverWiredUsesWiredForCandidates() {
        #expect(PathMonitor.primaryInterface(in: [tunnel, wired, wifi]) == wired)
    }

    @Test func physicalInterfacesKeepSystemPreference() {
        #expect(PathMonitor.primaryInterface(in: [cellular, wifi]) == cellular)
        #expect(PathMonitor.primaryInterface(in: [wifi, cellular]) == wifi)
    }

    @Test func tunnelWithoutAnActivePhysicalInterfaceStaysOther() {
        let unusedWifi = Interface(name: "en0", type: .wifi, isUsed: false)
        #expect(PathMonitor.primaryInterface(in: [tunnel, unusedWifi]) == tunnel)
        #expect(PathMonitor.primaryInterface(in: [unusedWifi, tunnel]) == tunnel)
        #expect(PathMonitor.primaryInterface(in: [unusedWifi]) == nil)
    }

    @Test func removingVpnKeepsTheSameUnderlyingInterface() {
        #expect(PathMonitor.primaryInterface(in: [tunnel, wifi])
            == PathMonitor.primaryInterface(in: [wifi]))
    }

    @Test func noInterfacesHasNoPrimary() {
        #expect(PathMonitor.primaryInterface(in: []) == nil)
    }
}
