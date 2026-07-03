import SwiftUI

/// The unpaired entry screen, following the redesigned Tauri landing: wordmark
/// over a hairline rule, subtitle, primary scan CTA, secondary direct-login CTA,
/// status line.
struct LandingView: View {
    @EnvironmentObject private var store: AppStore

    var body: some View {
        VStack(spacing: 0) {
            Spacer()

            VStack(spacing: 14) {
                Text(verbatim: "Baybo")
                    .font(Theme.mono(34, weight: .bold))
                    .textCase(.uppercase)
                    .kerning(10)
                    // Wide tracking adds a trailing gap; nudge to stay optically
                    // centered (the CSS text-indent trick, in SwiftUI form).
                    .padding(.leading, 10)
                    .foregroundStyle(Theme.ink)
                Rectangle()
                    .fill(Theme.ink)
                    .frame(width: 56, height: 1)
                Text("landing.subtitle")
                    .font(Theme.mono(14))
                    .foregroundStyle(Theme.inkSoft)
            }

            Spacer()

            // The web `.cta` column: width-capped at 17rem and centered, not
            // edge-to-edge.
            VStack(spacing: 12) {
                Button {
                    Haptics.tap()
                    store.status = nil
                    store.scanPresented = true
                } label: {
                    Text("landing.scan")
                }
                .buttonStyle(InkPillButtonStyle())

                Button {
                    store.status = nil
                    store.landingView = .direct
                } label: {
                    Text("landing.direct")
                }
                .buttonStyle(OutlinePillButtonStyle())
            }
            .frame(maxWidth: 272)

            statusLine
                .padding(.top, 18)

            Text(verbatim: appVersion)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft.opacity(0.6))
                .padding(.top, 26)
        }
        .padding(.horizontal, 28)
        .padding(.bottom, 18)
    }

    @ViewBuilder
    private var statusLine: some View {
        if let status = store.status {
            Text(verbatim: status)
                .font(Theme.mono(12))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
        } else if store.busy {
            ProgressView().tint(Theme.inkSoft)
        }
    }

    private var appVersion: String {
        let version =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return "v\(version ?? "0")"
    }
}
