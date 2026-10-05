import SwiftUI

/// The Settings section of the home shell: the account/app controls that used
/// to hang off the chat header. Language, legal/support links, a relay
/// binding's connection, version, and log out — laid out flat and monochrome,
/// logout pinned above the menu bar.
struct SettingsScreen: View {
    @Environment(\.openURL) private var openURL
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var connection = ConnectionStore.shared

    /// Clearance for the overlaid header; the native tab bar's bottom inset is
    /// handled by the system, so the bottom is just a breathing gap.
    private static let topInset: CGFloat = 58
    private static let bottomInset: CGFloat = 24
    private static let iconWidth: CGFloat = 24
    private static let iconGap: CGFloat = 14

    private static let version =
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "—"
    private static let privacyPolicyURL =
        "https://github.com/booiris/privacy-page/blob/master/baybo/README.md"
    private static let supportURL = "https://github.com/booiris/baybo/issues"

    var body: some View {
        VStack(spacing: 0) {
            VStack(spacing: 0) {
                actionRow(
                    icon: "globe",
                    title: lang.t("settings.language"),
                    value: lang.current.label
                ) {
                    Haptics.tap()
                    lang.toggle()
                }
                divider
                actionRow(
                    icon: "hand.raised",
                    title: lang.t("settings.privacyPolicy")
                ) {
                    Haptics.tap()
                    openExternalURL(Self.privacyPolicyURL)
                }
                divider
                actionRow(
                    icon: "questionmark.circle",
                    title: lang.t("settings.support")
                ) {
                    Haptics.tap()
                    openExternalURL(Self.supportURL)
                }
                divider
                // Carriers belong to a paired (relay) binding; a typed-URL
                // direct login never has one to show.
                if !appStore.directBound {
                    connectionRows
                    divider
                }
                infoRow(
                    icon: "info.circle",
                    title: lang.t("settings.version"),
                    value: Self.version
                )
            }
            .padding(.horizontal, 24)

            Spacer(minLength: 40)
        }
        .padding(.top, Self.topInset)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            VStack(spacing: 0) {
                logoutButton
                    .padding(.horizontal, 24)
            }
            .padding(.top, 16)
            .padding(.bottom, Self.bottomInset)
            .background(Theme.paper)
        }
    }

    /// What the legs ride on, and — once a probe has run — how it went. Reads
    /// "Relay" until the core's first report lands.
    @ViewBuilder private var connectionRows: some View {
        infoRow(
            icon: "antenna.radiowaves.left.and.right",
            title: lang.t("settings.connection"),
            value: carrierLabel(connection.status?.carrier ?? .relay)
        )
        if let probe = connection.status?.lastProbe {
            divider
            // Minute ticks keep "2 min. ago" true while the screen stays up.
            // The tick's own date is the START of its minute, which can precede
            // a probe that just finished, so the age is measured from the clock.
            TimelineView(.everyMinute) { _ in
                infoRow(
                    icon: "clock.arrow.circlepath",
                    title: lang.t("settings.lastProbe"),
                    value: probeLabel(probe, now: Date()),
                    detail: tiersLabel(probe.tiers)
                )
            }
        }
    }

    // No busy gate: presenting the confirm is pure UI (`AppStore.logout()`
    // guards its own re-entry), and a `.disabled` pill here has no disabled
    // look — it just reads as a dead button.
    private var logoutButton: some View {
        Button {
            Haptics.tap()
            withAnimation(ConfirmDialog.enterMotion) {
                appStore.confirmLogout = true
            }
        } label: {
            Text(verbatim: lang.t("connected.logout"))
        }
        .buttonStyle(OutlinePillButtonStyle(color: Theme.err))
    }

    private func actionRow(
        icon: String, title: String, value: String? = nil, action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            rowContent(icon: icon, title: title, value: value, chevron: true)
        }
        .buttonStyle(.plain)
    }

    private func openExternalURL(_ value: String) {
        guard let url = URL(string: value) else { return }
        openURL(url)
    }

    private func infoRow(icon: String, title: String, value: String, detail: String? = nil)
        -> some View
    {
        rowContent(icon: icon, title: title, value: value, detail: detail, chevron: false)
    }

    /// `detail` is a second, smaller line under the title, for a value too
    /// long to share the row with it.
    private func rowContent(
        icon: String, title: String, value: String?, detail: String? = nil, chevron: Bool
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: Self.iconGap) {
                Image(systemName: icon)
                    .font(.system(size: 17, weight: .regular))
                    .foregroundStyle(Theme.ink)
                    .frame(width: Self.iconWidth)
                Text(verbatim: title)
                    .font(Theme.mono(15))
                    .foregroundStyle(Theme.ink)
                Spacer()
                if let value {
                    Text(verbatim: value)
                        .font(Theme.mono(14))
                        .foregroundStyle(Theme.inkSoft)
                }
                if chevron {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(Theme.line)
                }
            }
            if let detail {
                Text(verbatim: detail)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
                    .padding(.leading, Self.iconWidth + Self.iconGap)
            }
        }
        .padding(.vertical, 16)
        .contentShape(Rectangle())
    }

    // MARK: - Connection labels

    private func carrierLabel(_ carrier: CarrierLabel) -> String {
        switch carrier {
        case .relay: return lang.t("settings.connectionRelay")
        case .lan: return lang.t("settings.connectionLan")
        case .ipv6: return lang.t("settings.connectionIpv6")
        case .ipv4: return lang.t("settings.connectionIpv4")
        case .ipv4Punched: return lang.t("settings.connectionIpv4Punched")
        }
    }

    /// "2 min. ago · Wi-Fi": when the probe finished, and on which network.
    /// Relative like the cron list's next-run column, in the chrome language.
    private func probeLabel(_ probe: ProbeReport, now: Date) -> String {
        let finished = Date(timeIntervalSince1970: TimeInterval(probe.finishedAtMs) / 1000)
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = Locale(identifier: lang.current.lproj)
        formatter.unitsStyle = .abbreviated
        let when = formatter.localizedString(for: finished, relativeTo: now)
        return "\(when) · \(networkLabel(probe.network))"
    }

    private func networkLabel(_ kind: NetworkInterfaceKind) -> String {
        switch kind {
        case .wifi: return lang.t("settings.networkWifi")
        case .wired: return lang.t("settings.networkWired")
        case .cellular: return lang.t("settings.networkCellular")
        case .loopback, .other: return lang.t("settings.networkOther")
        }
    }

    /// "LAN ok · IPv6 not offered · IPv4 failed · Punched timeout", in the
    /// core's tier order.
    private func tiersLabel(_ tiers: [TierReport]) -> String {
        tiers
            .map { "\(tierLabel($0.tier)) \(outcomeLabel($0.outcome))" }
            .joined(separator: " · ")
    }

    private func tierLabel(_ tier: CarrierLabel) -> String {
        switch tier {
        case .relay: return lang.t("settings.connectionRelay")
        case .lan: return lang.t("settings.tierLan")
        case .ipv6: return lang.t("settings.tierIpv6")
        case .ipv4: return lang.t("settings.tierIpv4")
        case .ipv4Punched: return lang.t("settings.tierIpv4Punched")
        }
    }

    private func outcomeLabel(_ outcome: TierOutcome) -> String {
        switch outcome {
        case .ok: return lang.t("settings.outcomeOk")
        case .failed: return lang.t("settings.outcomeFailed")
        case .timeout: return lang.t("settings.outcomeTimeout")
        case .notOffered: return lang.t("settings.outcomeNotOffered")
        case .denied: return lang.t("settings.outcomeDenied")
        case .skipped: return lang.t("settings.outcomeSkipped")
        }
    }

    private var divider: some View {
        Rectangle()
            .fill(Theme.line)
            .frame(height: 1)
    }
}
