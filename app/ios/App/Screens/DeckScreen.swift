import SwiftUI

/// The Deck tab (docs/modules/deck.md): the full-bleed deck-shell webview
/// under the shared wordmark header, with an edit-mode Done pill and the
/// native delete confirm (destructive actions confirm natively; the shell
/// only reports intent).
struct DeckScreen: View {
    @EnvironmentObject private var store: AppStore
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        DeckContent(deck: store.deckStore, host: store.deckHost())
    }
}

private struct DeckContent: View {
    @ObservedObject var deck: DeckStore
    let host: DeckHost
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        ZStack(alignment: .top) {
            DeckWebView(host: host)
                .ignoresSafeArea(edges: .bottom)
                .padding(.top, 52)
            HomeHeaderView()
                .overlay(alignment: .trailing) {
                    if deck.editMode {
                        Button {
                            deck.setEditMode(false)
                        } label: {
                            Text(verbatim: lang.t("deck.editDone"))
                                .font(Theme.mono(13))
                                .foregroundStyle(Theme.ink)
                                .padding(.horizontal, 14)
                                .frame(height: 34)
                        }
                        .glassEffect(.regular.interactive(), in: .capsule)
                        .padding(.trailing, 20)
                    }
                }
        }
        .onChange(of: lang.code) { _, code in
            host.bridge.setLanguage(code)
        }
        .alert(
            Text(verbatim: lang.t("deck.deleteTitle")),
            isPresented: Binding(
                get: { deck.pendingDelete != nil },
                set: { if !$0 { deck.pendingDelete = nil } }
            )
        ) {
            Button(lang.t("deck.deleteConfirm"), role: .destructive) {
                deck.confirmPendingDelete()
            }
            Button(lang.t("deck.cancel"), role: .cancel) {
                deck.pendingDelete = nil
            }
        } message: {
            Text(verbatim: lang.t("deck.deleteBody"))
        }
    }
}
