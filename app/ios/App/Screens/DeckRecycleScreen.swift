import SwiftUI

/// The Deck's recycle bin, pushed on the outer NavigationStack from the Deck
/// header's ☰ menu — the ArchivedScreen shape: back chevron + centered title
/// over the paper veil, rows in the list grammar. Soft-deleted cards keep
/// their bundle and last snapshot server-side, so a restore puts the card
/// back on the board already populated.
struct DeckRecycleScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    private var deck: DeckStore { appStore.deckStore }

    var body: some View {
        DeckRecycleContent(deck: appStore.deckStore, dismiss: { dismiss() })
    }
}

private struct DeckRecycleContent: View {
    @ObservedObject var deck: DeckStore
    let dismiss: () -> Void
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if deck.recycle.isEmpty {
                    emptyState
                } else {
                    cardList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            header
        }
        .background(Theme.paper)
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task {
            await deck.fetchRecycleNow()
        }
    }

    private var header: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(verbatim: lang.t("deck.recycleTitle"))
                    .font(Theme.mono(16))
                    .foregroundStyle(Theme.ink)

                HStack {
                    Button {
                        dismiss()
                    } label: {
                        Image(systemName: "chevron.left")
                            .font(.system(size: 18, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassSurface(interactive: true, in: .circle)
                    .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

                    Spacer()
                }
            }
            .padding(.horizontal, 24)
            .frame(height: ChatHeaderView.barHeight)
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    private var cardList: some View {
        List {
            ForEach(deck.recycle) { card in
                DeckRecycleRowView(
                    card: card,
                    langCode: lang.current.lproj,
                    onRestore: { deck.restore(cardId: card.cardId) }
                )
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(EdgeInsets(top: 0, leading: 24, bottom: 0, trailing: 24))
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, ChatHeaderView.barHeight + 8, for: .scrollContent)
    }

    private var emptyState: some View {
        Text(verbatim: lang.t("deck.recycleEmpty"))
            .font(Theme.sys(14))
            .foregroundStyle(Theme.inkSoft)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

/// One recycled card: title + when it was deleted, with the restore action as
/// the right column. The restore is a plain ink pill — the row's only action,
/// no swipe to discover.
struct DeckRecycleRowView: View {
    let card: DeckStore.RecycledCard
    let langCode: String
    let onRestore: () -> Void

    var body: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 5) {
                    Image(systemName: "rectangle.stack")
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(Theme.ink)
                        .accessibilityHidden(true)
                    Text(verbatim: card.title)
                        .font(Theme.sys(15, weight: .bold))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Text(verbatim: Self.deletedLabel(card, locale: langCode))
                    .font(Theme.sys(13))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Button(action: onRestore) {
                Text(verbatim: Lang.shared.t("deck.restore"))
                    .font(Theme.mono(13))
                    .foregroundStyle(Theme.ink)
                    .padding(.horizontal, 14)
                    .frame(height: 32)
                    .overlay(
                        Capsule().strokeBorder(Theme.ink, lineWidth: 1)
                    )
                    .contentShape(Capsule())
            }
            .buttonStyle(.plain)
        }
        .frame(minHeight: 52)
        .padding(.vertical, 15)
        .contentShape(Rectangle())
    }

    /// "Deleted <relative time>" — relative like the cron list's next-run
    /// column, in the chrome language. A row minted before `deleted_at`
    /// existed shows just the size class rather than a fake time.
    static func deletedLabel(_ card: DeckStore.RecycledCard, locale: String) -> String {
        guard let ms = card.deletedAtMs else { return card.size }
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = Locale(identifier: locale)
        formatter.unitsStyle = .abbreviated
        let rel = formatter.localizedString(for: date, relativeTo: Date())
        return Lang.shared.t("deck.deletedAt").replacingOccurrences(of: "{time}", with: rel)
    }
}
