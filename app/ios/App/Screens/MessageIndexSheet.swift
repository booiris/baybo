import SwiftUI

/// The chat header's message index: every message the USER sent in this
/// conversation, each glossed with the agent's answer, oldest at the top.
/// Tapping a row parks the transcript on that message.
///
/// A `.sheet`, not an overlay on the screen's ZStack: the composer dock is a
/// `.safeAreaInset(edge: .bottom)` applied TO that stack, so no sibling can
/// ever cover it — an overlay would need a hand-rolled drag-dismiss plus a
/// `.disabled()` on the composer, which walks straight into the CJK
/// marked-text scar.
///
/// The list is a window over the transcript rows the webview currently holds,
/// which is why the count can only ever be a lower bound and why "load earlier"
/// exists at all.
struct MessageIndexSheet: View {
    @ObservedObject var bridge: TranscriptBridge
    /// The picked row. The screen stashes it and closes the sheet; the jump
    /// itself runs from this content's `.onDisappear` (see `ChatScreen`).
    let onPick: (String) -> Void

    /// This sheet renders VISIBLE localized prose, so it observes the language.
    /// (`ChatHeaderView` and `ModelMenuPanel` skip this and go stale on a live
    /// toggle — a known bug, not a pattern to copy.)
    @ObservedObject private var lang = Lang.shared
    /// Whether the opening scroll has settled on the real answer — see
    /// `landOnReadersPosition`.
    @State private var landed = false

    fileprivate static let rowHInset: CGFloat = 24
    /// Stable handle for the UI smoke: every row's label is localized prose fed
    /// by the transcript, so there is nothing else to match a row by.
    static let rowIdentifier = "message-index-row"

    /// The count beside the title. A windowed set makes a bare number a lie, so
    /// while older messages remain unloaded it reads as a floor.
    static func countLabel(_ count: Int, hasMore: Bool) -> String {
        hasMore ? "\(count)+" : "\(count)"
    }

    var body: some View {
        VStack(spacing: 0) {
            grabber
            titleRow
            Rectangle()
                .fill(Theme.line)
                .frame(height: 1)
            content
        }
    }

    /// Hand-rolled rather than `.presentationDragIndicator(.visible)`, which
    /// sits inside the content inset and pushes the title down. Ink at low
    /// alpha, never `Theme.line` — #E4E4E4 on white paper is invisible.
    private var grabber: some View {
        Capsule()
            .fill(Theme.inkSoft.opacity(0.35))
            .frame(width: 36, height: 4)
            .padding(.vertical, 9)
    }

    private var titleRow: some View {
        HStack(spacing: 12) {
            Text(verbatim: lang.t("chat.messageIndex"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
            Spacer(minLength: 0)
            Text(
                verbatim: Self.countLabel(
                    bridge.outline.count, hasMore: bridge.outlineHasMoreOlder)
            )
            .font(Theme.mono(11))
            .foregroundStyle(Theme.inkSoft)
        }
        .padding(.horizontal, Self.rowHInset)
        .frame(height: 40)
    }

    /// `hasMoreOlder` keeps the list even with nothing to show yet: the button's
    /// own gate lets it through on unloaded pages alone (a window whose rows are
    /// all one long turn's), and the empty state would then strand the reader on
    /// a blank sheet with the "load earlier" row hidden behind it.
    @ViewBuilder
    private var content: some View {
        if bridge.outline.isEmpty && !bridge.outlineHasMoreOlder {
            emptyState
        } else {
            entryList
        }
    }

    private var entryList: some View {
        ScrollViewReader { proxy in
            List {
                if bridge.outlineHasMoreOlder {
                    loadOlderRow
                }
                ForEach(MessageOutline.buckets(bridge.outline)) { bucket in
                    let label = MessageOutline.dayLabel(
                        bucket.dayKey, lproj: lang.current.lproj)
                    if !label.isEmpty {
                        dayHeader(label)
                    }
                    ForEach(bucket.entries) { entry in
                        Button {
                            onPick(entry.id)
                        } label: {
                            MessageIndexRow(
                                entry: entry, here: entry.id == bridge.outlineHereId)
                        }
                        .buttonStyle(.plain)
                        .id(entry.id)
                        .accessibilityIdentifier(Self.rowIdentifier)
                        .listRowBackground(Theme.paper)
                        .listRowSeparatorTint(Theme.line)
                        .listRowInsets(Self.rowInsets)
                    }
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
            // `outlineHereId` is a round trip through the web-content process,
            // cleared as the request goes out, so it can land before OR after
            // this list first appears — react to both, and land only once so a
            // late answer can't yank the list out from under a reader who has
            // already started scrolling it.
            .onAppear { landOnReadersPosition(proxy) }
            .onChange(of: bridge.outlineHereId) { _, _ in landOnReadersPosition(proxy) }
        }
    }

    /// Open on what the reader is looking at, falling back to the newest entry
    /// (the transcript's own resting position) until the scan answers.
    private func landOnReadersPosition(_ proxy: ScrollViewProxy) {
        guard !landed else { return }
        guard let target = bridge.outlineHereId ?? bridge.outline.last?.id else { return }
        // Only the real answer settles it; the fallback still scrolls, so an
        // answer that never arrives leaves the sheet somewhere sensible.
        if bridge.outlineHereId != nil { landed = true }
        proxy.scrollTo(target, anchor: .center)
    }

    /// Earlier is UP, mirroring the transcript's own scroll-up paging.
    private var loadOlderRow: some View {
        Button {
            bridge.outlineLoadOlder()
        } label: {
            HStack(spacing: 6) {
                Image(systemName: "chevron.up")
                    .font(.system(size: 10, weight: .semibold))
                Text(verbatim: lang.t("chat.loadOlder"))
                    .font(Theme.mono(12))
            }
            .foregroundStyle(Theme.inkSoft)
            .frame(maxWidth: .infinity, minHeight: 44)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        // Dimmed rather than spun: the rows below it stay readable, and a
        // spinner over a list that is about to grow upward reads as a stall.
        .opacity(bridge.outlineLoadingOlder ? 0.4 : 1)
        .disabled(bridge.outlineLoadingOlder)
        .listRowBackground(Theme.paper)
        .listRowSeparator(.hidden)
        .listRowInsets(Self.rowInsets)
    }

    /// An ordinary row, NOT a `Section` header — `.plain` pins those to the top
    /// of the scroller, and a day label sliding over the rows it belongs to
    /// reads as a stuck cell.
    private func dayHeader(_ label: String) -> some View {
        Text(verbatim: label)
            .font(Theme.mono(11))
            .foregroundStyle(Theme.inkSoft)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.top, 14)
            .padding(.bottom, 6)
            .listRowBackground(Theme.paper)
            .listRowSeparator(.hidden)
            .listRowInsets(Self.rowInsets)
    }

    /// The normal state of a new conversation, now that the header button is
    /// permanent — it used to be reachable only if the last entry vanished
    /// under an open sheet. The sheet stays open either way: a panel that
    /// closes itself under your finger reads as a crash.
    ///
    /// A DECODE FAILURE says so instead. Both leave the list empty, but only
    /// one of them is the reader's own doing, and telling someone with a full
    /// thread that they have never sent a message is the worse lie.
    private var emptyState: some View {
        VStack {
            Spacer()
            Text(verbatim: lang.t(bridge.outlineFailed ? "chat.indexUnavailable" : "chat.indexEmpty"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private static let rowInsets = EdgeInsets(
        top: 0, leading: rowHInset, bottom: 0, trailing: rowHInset)
}

/// One sent message: the prompt over the agent's one-line gloss, with the time
/// (already formatted web-side) in a trailing column.
///
/// Hand-rolled rather than `ChatRowBody`, which looks the same but is a
/// different thing: it clamps its date against conversation recency and
/// hardcodes an unread badge, neither of which means anything here
/// (`CronJobRowView` is the sanctioned precedent for the split).
private struct MessageIndexRow: View {
    let entry: OutlineEntry
    /// The message the transcript is currently parked on.
    let here: Bool

    @ObservedObject private var lang = Lang.shared

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            VStack(alignment: .leading, spacing: 4) {
                prompt
                if !entry.gloss.isEmpty {
                    gloss
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            metaColumn
        }
        .frame(minHeight: 56)
        .padding(.vertical, 11)
        .background(alignment: .leading) { hereMark }
        .contentShape(Rectangle())
        .accessibilityLabel(Text(verbatim: promptText))
        .accessibilityValue(Text(verbatim: entry.at))
    }

    @ViewBuilder
    private var prompt: some View {
        if isAttachmentOnly {
            HStack(spacing: 6) {
                Image(systemName: "paperclip")
                    .font(.system(size: 12))
                Text(verbatim: promptText)
            }
            .font(Theme.sys(15, weight: .medium))
            .foregroundStyle(Theme.ink)
        } else {
            Text(verbatim: promptText)
                .font(Theme.sys(15, weight: .medium))
                .foregroundStyle(Theme.ink)
                .lineLimit(2)
                .multilineTextAlignment(.leading)
        }
    }

    private var gloss: some View {
        HStack(alignment: .firstTextBaseline, spacing: 5) {
            Image(systemName: "arrow.turn.down.right")
                .font(.system(size: 10))
            Text(verbatim: entry.gloss)
                .font(Theme.sys(13))
                .lineLimit(1)
                .truncationMode(.tail)
        }
        .foregroundStyle(Theme.inkSoft)
        .padding(.top, 4)
    }

    /// `fixedSize` because `at` widens to a dated form on any earlier day —
    /// exactly the old messages this sheet exists to reach — and the prompt
    /// must never run under it.
    private var metaColumn: some View {
        VStack(alignment: .trailing, spacing: 6) {
            if entry.state == .sending {
                // A queued send has no server clock, so the glyph REPLACES the
                // time rather than sitting beside an empty column.
                Image(systemName: "clock")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.inkSoft)
            } else if !entry.at.isEmpty {
                Text(verbatim: entry.at)
                    .font(Theme.sys(11))
                    .foregroundStyle(Theme.inkSoft)
            }
            if entry.state == .failed {
                Image(systemName: "exclamationmark.circle")
                    .font(.system(size: 12))
                    .foregroundStyle(Theme.err)
            }
        }
        .fixedSize(horizontal: true, vertical: false)
    }

    /// Both marks bleed back through the row gutter to the screen edge — a wash
    /// inset by 24pt reads as a floating band rather than a highlighted row.
    @ViewBuilder
    private var hereMark: some View {
        if here {
            ZStack(alignment: .leading) {
                Theme.ink.opacity(0.045)
                Rectangle()
                    .fill(Theme.ink)
                    .frame(width: 2)
            }
            .padding(.horizontal, -MessageIndexSheet.rowHInset)
            .allowsHitTesting(false)
        }
    }

    private var isAttachmentOnly: Bool {
        entry.text.isEmpty && entry.attachments > 0
    }

    /// Through `previewText` rather than a third truncation rule: the same
    /// 120-scalar clip the chat list and the gateway both apply.
    private var promptText: String {
        isAttachmentOnly
            ? String(format: lang.t("chat.indexAttachments"), entry.attachments)
            : SessionIndex.previewText(entry.text)
    }
}
