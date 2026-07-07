import SwiftUI

/// The unbound entry screen — the web `.screen.landing` structure: one hero
/// group (`flex: 1`, centered) holding the wordmark + rule and either the menu
/// CTAs or the direct-login form, a version footer pinned below it, and the
/// language switcher fixed top-right. The CTAs live INSIDE the centered hero,
/// not at the screen bottom.
struct LandingView: View {
    @EnvironmentObject private var store: AppStore
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        // A scroll container, like the web page it replaces — and the layout
        // IGNORES the keyboard entirely. Moving focus between the URL and
        // password fields swaps keyboards (they are different input views), so
        // UIKit fires a hide+show pair; any keyboard-driven inset rides both
        // animations and the page dips down then up. The whole form sits above
        // the portrait keyboard's top edge anyway (the web page only panned
        // when something was actually covered — here nothing is), so the right
        // behavior is: don't move at all.
        GeometryReader { geo in
            ScrollView(showsIndicators: false) {
                content
                    .frame(minHeight: geo.size.height)
                    .tapToDismissKeyboard()
            }
            .scrollDismissesKeyboard(.interactively)
        }
        .ignoresSafeArea(.keyboard)
        .overlay(alignment: .topTrailing) {
            LangSwitcher()
                .padding(.top, 12)
                .padding(.trailing, 16)
        }
    }

    private var content: some View {
        VStack(spacing: 12) {
            // .landing-hero: flex 1, centered column, 1rem gap.
            VStack(spacing: 16) {
                // .wordmark: 1.7rem regular, 0.3em tracking (+ lead-in for
                // optical centering), uppercase.
                Text(verbatim: "Baybo")
                    .font(Theme.mono(27))
                    .textCase(.uppercase)
                    .kerning(8)
                    .padding(.leading, 8)
                    .foregroundStyle(Theme.ink)
                // .rule: a 2rem hairline.
                Rectangle()
                    .fill(Theme.ink)
                    .frame(width: 32, height: 1)

                switch store.landingView {
                case .menu:
                    menu
                case .direct:
                    DirectLoginForm()
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            // .landing-foot
            Text(verbatim: appVersion)
                .font(Theme.mono(11))
                .kerning(0.9)
                .foregroundStyle(Theme.inkSoft)
                .padding(.top, 16)
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 20)
    }

    @ViewBuilder
    private var menu: some View {
        Text(verbatim: lang.t("landing.subtitle"))
            .font(Theme.mono(16))
            .foregroundStyle(Theme.inkSoft)
            .lineSpacing(6)
            .multilineTextAlignment(.center)
            .frame(maxWidth: 288)

        VStack(spacing: 16) {
            Button {
                Haptics.tap()
                store.status = nil
                store.scanPresented = true
            } label: {
                Text(verbatim: lang.t("landing.scan"))
            }
            .buttonStyle(InkPillButtonStyle())
            .disabled(store.busy)

            Button {
                Haptics.tap()
                store.status = nil
                store.landingView = .direct
            } label: {
                Text(verbatim: lang.t("landing.direct"))
            }
            .buttonStyle(OutlinePillButtonStyle())
            .disabled(store.busy)
        }
        .frame(maxWidth: 272) // .cta max-width 17rem
        .padding(.top, 12)

        if let status = store.status {
            Text(verbatim: status)
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
        }
    }

    private var appVersion: String {
        let version =
            Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return "v\(version ?? "0")"
    }
}
