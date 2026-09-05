import PhotosUI
import SwiftUI

/// The Deck tab (docs/modules/deck.md): a native empty state over the warm,
/// full-bleed card-grid webview, under the shared wordmark header, with an
/// edit-mode Done pill and native destructive confirmation.
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
    /// The photo the `deck.pickBlob` picker returned — cleared the moment the
    /// choice is claimed (`photoSelection`), so the picker opens clean and the
    /// same photo picked twice is delivered twice.
    @State private var pickedItem: PhotosPickerItem?

    var body: some View {
        ZStack(alignment: .top) {
            // Full-bleed like the chat list: the grid's own top padding
            // (deck.css, env(safe-area-inset-top) + header height) sets
            // the resting offset, and scrolled content ghosts under the
            // header's paper veil instead of hitting a hard edge.
            DeckWebView(host: host)
                .ignoresSafeArea()
            if deck.isEmpty {
                emptyState
            }
            // The header (wordmark + ☰ menu) STAYS on a maximized card — the
            // maximized card fills the area below it and its content scrolls
            // under the header's paper veil, same as the grid. Only the edit
            // pill is suppressed while maximized (you can't reorder/resize a
            // card that's expanded); see `header`.
            header
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
        // `deck.pickBlob`: a card asked for a blob. One picker at a time
        // (DeckStore's `activePick`), and the card's `accept` elects which —
        // so the two presentations bind to the same mode, never to each other.
        .photosPicker(
            isPresented: Binding(
                get: { deck.pickerMode == .photos },
                set: { if !$0 { deck.photosPickerDismissed() } }
            ),
            selection: photoSelection, matching: .images
        )
        .fileImporter(
            isPresented: Binding(
                get: { deck.pickerMode.isFiles },
                set: { if !$0 { deck.filePickerDismissed() } }
            ),
            allowedContentTypes: deck.pickerMode.fileTypes,
            allowsMultipleSelection: false,
            onCompletion: { result in
                guard let pick = deck.consumePick() else { return }
                deck.finishFilePick(
                    id: pick.id, cardId: pick.cardId, url: (try? result.get())?.first)
            },
            // Unlike PhotosPicker, the file browser says so — no inference.
            onCancellation: { deck.filePickCancelled() }
        )
        // `deck.shareBlob`: a materialized blob awaiting the system share sheet.
        .sheet(item: $deck.shareItem) { item in
            ShareSheet(url: item.url)
        }
    }

    private var emptyState: some View {
        VStack {
            Spacer()
            CreationPrompt(
                symbol: AppStore.HomeTab.deck.icon,
                title: lang.t("home.tab.deck"),
                message: lang.t("deck.empty"),
                actionTitle: emptyActionTitle,
                actionIdentifier: "deck-empty-new-card",
                actionInFlight: deck.setupSessionId != nil
            ) {
                Haptics.tap()
                appStore.startCardDraft(prompt: lang.t("deck.quickSetupPrompt"))
            }
            Spacer()
        }
        .padding(.bottom, 80)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.paper)
    }

    private var emptyActionTitle: String {
        if deck.setupSessionId != nil {
            return lang.t("deck.createCardInflight")
        }
        return lang.t("deck.quickSetup")
    }

    /// The photo picker's selection, taken through a binding SETTER rather than
    /// watched with `onChange`. `PhotosPicker` writes the selection and clears
    /// `isPresented` on one synchronous dismissal path, and only a setter runs
    /// on it: an `onChange` lands in the next update pass, which the dismissal's
    /// deferred cancel inference is not guaranteed to lose to — that race drops
    /// the chosen photo and rejects the card's promise as cancelled. Claiming
    /// the pick here makes it an ordering instead of a race.
    private var photoSelection: Binding<PhotosPickerItem?> {
        Binding(
            get: { pickedItem },
            set: { item in
                pickedItem = nil
                guard let item, let pick = deck.consumePick() else { return }
                Task {
                    let mime = item.supportedContentTypes.first?.preferredMIMEType ?? "image/jpeg"
                    let data = (try? await item.loadTransferable(type: Data.self)) ?? nil
                    deck.finishPick(id: pick.id, cardId: pick.cardId, data: data, mime: mime)
                }
            }
        )
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
                    .glassSurface(interactive: true, in: .circle)
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
            .glassSurface(interactive: true, in: .circle)
            .padding(.trailing, 20)
            .accessibilityLabel(Text(verbatim: lang.t("deck.editDone")))
            .transition(.opacity)
        } else if !deck.isEmpty {
            Button {
                deck.setEditMode(!deck.editMode)
            } label: {
                Text(verbatim: lang.t(deck.editMode ? "deck.editDone" : "deck.edit"))
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.ink)
                    .padding(.horizontal, 14)
                    .frame(height: 34)
            }
            .glassSurface(interactive: true, in: .capsule)
            .padding(.trailing, 20)
            .transition(.opacity)
        }
    }
}
