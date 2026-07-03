import SwiftUI

/// The pairing confirm screen: the Bluetooth-style code the user compares
/// against the operator's terminal, with Cancel / Pair. A gateway-side abort
/// (`PairAbortListener`) dismisses this screen via `AppStore.pairAborted`.
struct PairConfirmView: View {
    @EnvironmentObject private var store: AppStore

    var body: some View {
        VStack(spacing: 0) {
            Spacer()

            VStack(spacing: 22) {
                Text("pair.confirmTitle")
                    .font(Theme.mono(18, weight: .bold))
                    .foregroundStyle(Theme.ink)

                Text(verbatim: store.challenge?.confirmCode ?? "")
                    .font(Theme.mono(44, weight: .bold))
                    .kerning(8)
                    .padding(.leading, 8)
                    .foregroundStyle(Theme.ink)
                    // The one place selectable text is wanted (tap-to-copy).
                    .textSelection(.enabled)

                Text("pair.confirmHint")
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.inkSoft)
                    .multilineTextAlignment(.center)
            }

            Spacer()

            if let status = store.status {
                Text(verbatim: status)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
                    .padding(.bottom, 12)
            }

            VStack(spacing: 12) {
                Button {
                    Haptics.tap()
                    store.confirmPair(accepted: true)
                } label: {
                    Text("pair.pair")
                }
                .buttonStyle(InkPillButtonStyle())
                .disabled(store.busy)

                Button {
                    store.confirmPair(accepted: false)
                } label: {
                    Text("pair.cancel")
                }
                .buttonStyle(OutlinePillButtonStyle())
                .disabled(store.busy)
            }
        }
        .padding(.horizontal, 28)
        .padding(.bottom, 18)
    }
}
