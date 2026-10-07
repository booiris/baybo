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
                    connectionRow
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

    private var connectionRow: some View {
        actionRow(
            icon: "antenna.radiowaves.left.and.right",
            title: lang.t("settings.connection"),
            value: ConnectionLabels.carrierLabel(connection.status?.carrier ?? .relay)
        ) {
            Haptics.tap()
            appStore.chatPath.append(.connection)
        }
        .accessibilityIdentifier("settings.connection")
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

    private var divider: some View {
        Rectangle()
            .fill(Theme.line)
            .frame(height: 1)
    }
}
