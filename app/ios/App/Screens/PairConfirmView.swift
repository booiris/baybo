import SwiftUI

/// The pairing confirm screen — the web `.screen.screen-center` layout: title,
/// muted hint, the code in a bordered rounded box (1.9rem, 0.24em tracking),
/// then a ROW of two filled base buttons (Cancel | Pair), status below, and the
/// language switcher fixed top-right. A gateway-side abort dismisses this
/// screen via `AppStore.pairAborted`.
struct PairConfirmView: View {
    @EnvironmentObject private var store: AppStore
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        VStack(spacing: 12) {
            // h1: 1.4rem bold.
            Text(verbatim: lang.t("pair.confirmTitle"))
                .font(Theme.mono(22, weight: .bold))
                .foregroundStyle(Theme.ink)

            Text(verbatim: lang.t("pair.confirmHint"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(5)
                .multilineTextAlignment(.center)

            // .confirm-code: bordered rounded box, wide-tracked digits.
            Text(verbatim: store.challenge?.confirmCode ?? "")
                .font(Theme.mono(30))
                .kerning(7)
                .padding(.leading, 7)
                .foregroundStyle(Theme.ink)
                .padding(16)
                .frame(maxWidth: .infinity)
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.radius)
                        .strokeBorder(Theme.line, lineWidth: 1)
                )
                .padding(.vertical, 8)
                // The one place selectable text is wanted (tap-to-copy).
                .textSelection(.enabled)

            // .row: two side-by-side base buttons, both filled.
            HStack(spacing: 12) {
                Button {
                    store.confirmPair(accepted: false)
                } label: {
                    Text(verbatim: lang.t("pair.cancel"))
                }
                .buttonStyle(FilledPillButtonStyle())
                .disabled(store.busy)

                Button {
                    Haptics.tap()
                    store.confirmPair(accepted: true)
                } label: {
                    Text(verbatim: lang.t("pair.pair"))
                }
                .buttonStyle(FilledPillButtonStyle())
                .disabled(store.busy)
            }

            if let status = store.status {
                Text(verbatim: status)
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .multilineTextAlignment(.center)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(.horizontal, 20)
        .overlay(alignment: .topTrailing) {
            LangSwitcher()
                .padding(.top, 12)
                .padding(.trailing, 16)
        }
    }
}
