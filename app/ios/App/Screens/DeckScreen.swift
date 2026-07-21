import PhotosUI
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
    /// The photo the `deck.pickBlob` picker returned (nil until chosen).
    @State private var pickedItem: PhotosPickerItem?

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
        // `deck.pickBlob`: a card asked for a photo. One picker at a time
        // (DeckStore's `activePick`); selection uploads and resolves the card's
        // promise, dismissal-with-no-choice cancels it.
        .photosPicker(isPresented: $deck.pickerActive, selection: $pickedItem, matching: .images)
        .onChange(of: pickedItem) { _, item in
            guard let item, let pick = deck.consumePick() else {
                pickedItem = nil
                return
            }
            pickedItem = nil
            Task {
                let mime = item.supportedContentTypes.first?.preferredMIMEType ?? "image/jpeg"
                let data = (try? await item.loadTransferable(type: Data.self)) ?? nil
                deck.finishPick(id: pick.id, cardId: pick.cardId, data: data, mime: mime)
            }
        }
        .onChange(of: deck.pickerActive) { _, active in
            if !active { deck.pickerDismissed() }
        }
        // `deck.shareBlob`: a materialized blob awaiting the system share sheet.
        .sheet(item: $deck.shareItem) { item in
            ShareSheet(url: item.url)
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
                .overlay(alignment: .trailing) { trailingControl }
    }

    /// The header's top-right control. While a card is maximized it is the ✕
    /// that collapses it (the true top-right corner, above the header veil the
    /// webview can't paint over); otherwise it is the edit pill — the only way
    /// in and out of edit mode (reorder/resize/remove), no long-press entry.
    @ViewBuilder private var trailingControl: some View {
        if deck.maximized {
            Button {
                deck.requestRestore()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 15, weight: .medium))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 45, height: 45)
            }
            .glassEffect(.regular.interactive(), in: .circle)
            .padding(.trailing, 20)
            .accessibilityLabel(Text(verbatim: lang.t("deck.editDone")))
            .transition(.opacity)
        } else {
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
            .transition(.opacity)
        }
    }
}
