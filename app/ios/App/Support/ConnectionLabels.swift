import Foundation

@MainActor
enum ConnectionLabels {
    private static var lang: Lang { Lang.shared }

    static func carrierLabel(_ carrier: CarrierLabel) -> String {
        switch carrier {
        case .relay: return lang.t("settings.connectionRelay")
        case .lan: return lang.t("settings.connectionLan")
        case .ipv6: return lang.t("settings.connectionIpv6")
        case .ipv4: return lang.t("settings.connectionIpv4")
        case .ipv4Punched: return lang.t("settings.connectionIpv4Punched")
        }
    }

    static func networkLabel(_ kind: NetworkInterfaceKind) -> String {
        switch kind {
        case .wifi: return lang.t("settings.networkWifi")
        case .wired: return lang.t("settings.networkWired")
        case .cellular: return lang.t("settings.networkCellular")
        case .loopback, .other: return lang.t("settings.networkOther")
        }
    }

    static func tierLabel(_ tier: CarrierLabel) -> String {
        switch tier {
        case .relay: return lang.t("settings.connectionRelay")
        case .lan: return lang.t("settings.tierLan")
        case .ipv6: return lang.t("settings.tierIpv6")
        case .ipv4: return lang.t("settings.tierIpv4")
        case .ipv4Punched: return lang.t("settings.tierIpv4Punched")
        }
    }

    static func outcomeLabel(_ outcome: TierOutcome) -> String {
        switch outcome {
        case .ok: return lang.t("settings.outcomeOk")
        case .failed: return lang.t("settings.outcomeFailed")
        case .timeout: return lang.t("settings.outcomeTimeout")
        case .notOffered: return lang.t("settings.outcomeNotOffered")
        case .denied: return lang.t("settings.outcomeDenied")
        case .skipped: return lang.t("settings.outcomeSkipped")
        }
    }

}
