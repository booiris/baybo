import SwiftUI

/// The Settings section of the home shell: the account/app controls that used
/// to hang off the chat header. Language toggle, app version, and log out —
/// laid out flat and monochrome, logout pinned above the menu bar.
struct SettingsScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @State private var confirmLogout = false

    /// Clearance for the overlaid header; the native tab bar's bottom inset is
    /// handled by the system, so the bottom is just a breathing gap.
    private static let topInset: CGFloat = 58
    private static let bottomInset: CGFloat = 24

    private static let version =
        (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String) ?? "—"

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
        .confirmationDialog(
            Text(verbatim: lang.t("connected.logoutConfirm")),
            isPresented: $confirmLogout, titleVisibility: .visible
        ) {
            Button(role: .destructive) {
                Haptics.tap()
                Task { await appStore.logout() }
            } label: {
                Text(verbatim: lang.t("connected.logout"))
            }
        }
    }

    private var logoutButton: some View {
        Button {
            guard !appStore.busy else { return }
            Haptics.tap()
            confirmLogout = true
        } label: {
            Text(verbatim: lang.t("connected.logout"))
        }
        .buttonStyle(OutlinePillButtonStyle(color: Theme.err))
        .disabled(appStore.busy)
    }

    private func actionRow(
        icon: String, title: String, value: String, action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            rowContent(icon: icon, title: title, value: value, chevron: true)
        }
        .buttonStyle(.plain)
    }

    private func infoRow(icon: String, title: String, value: String) -> some View {
        rowContent(icon: icon, title: title, value: value, chevron: false)
    }

    private func rowContent(icon: String, title: String, value: String, chevron: Bool)
        -> some View
    {
        HStack(spacing: 14) {
            Image(systemName: icon)
                .font(.system(size: 17, weight: .regular))
                .foregroundStyle(Theme.ink)
                .frame(width: 24)
            Text(verbatim: title)
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
            Spacer()
            Text(verbatim: value)
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
            if chevron {
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Theme.line)
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
