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
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject var deck: DeckStore
    let host: DeckHost
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        ZStack(alignment: .top) {
            // Full-bleed like the chat list: the grid's own top padding
            // (deck.css, env(safe-area-inset-top) + header height) sets
            // the resting offset, and scrolled content ghosts under the
            // header's paper veil instead of hitting a hard edge.
            DeckWebView(host: host)
                .ignoresSafeArea()
            // The header (wordmark + ☰ menu) STAYS on a maximized card — the
            // maximized card fills the area below it and its content scrolls
            // under the header's paper veil, same as the grid. Only the edit
            // pill is suppressed while maximized (you can't reorder/resize a
            // card that's expanded); see `header`.
            header
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

    private var header: some View {
        HomeHeaderView()
            .overlay(alignment: .leading) {
                    // The Chats header's ☰ grammar: one glass circle, menu
                    // entries for the section's secondary surfaces. Deck has
                    // one — the recycle bin.
                    Menu {
                        Button {
                            appStore.openDeckRecycle()
                        } label: {
                            Label(
                                lang.t("deck.menuRecycle"),
                                systemImage: "arrow.uturn.backward")
                        }
                    } label: {
                        Image(systemName: "line.3.horizontal")
                            .font(.system(size: 16, weight: .medium))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 45, height: 45)
                    }
                    .glassEffect(.regular.interactive(), in: .circle)
                    .accessibilityLabel(Text(verbatim: lang.t("list.menu")))
                    .padding(.leading, 20)
                }
                .overlay(alignment: .trailing) {
                    // The header pill is the ONLY way in and out of edit
                    // mode (reorder/resize/remove) — no long-press entry;
                    // holds inside a card belong to the card. Hidden while a
                    // card is maximized (editing is meaningless then; the ✕ in
                    // the card's corner is the way back).
                    Button {
                        deck.setEditMode(!deck.editMode)
                    } label: {
                        Text(verbatim: lang.t(deck.editMode ? "deck.editDone" : "deck.edit"))
                            .font(Theme.mono(13))
                            .foregroundStyle(Theme.ink)
                            .padding(.horizontal, 14)
                            .frame(height: 34)
                    }
                    .glassEffect(.regular.interactive(), in: .capsule)
                    .padding(.trailing, 20)
                    .opacity(deck.maximized ? 0 : 1)
                    .allowsHitTesting(!deck.maximized)
                    .animation(.easeInOut(duration: 0.2), value: deck.maximized)
                }
    }
}
