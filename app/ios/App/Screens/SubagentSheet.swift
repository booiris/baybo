import SwiftUI

/// The chat header's subagent list: every child this conversation delegated to,
/// oldest at the top like the transcript itself. Tapping a row opens that
/// child's read-only transcript.
///
/// A `.sheet` for the same reason `MessageIndexSheet` is one — and the child
/// browser it opens is a sheet too, deliberately NOT a `fullScreenCover`: a
/// cover fires the parent `ChatScreen`'s `onDisappear`, which unhooks the
/// parent transcript for as long as it is up. Reading a subagent that ran for
/// half an hour would overflow the parent's offscreen frame buffer and force a
/// full re-sync on the way back.
struct SubagentSheet: View {
    let sessionId: String
    var client: any BayboClientProtocol = Baybo.client
    /// The picked child. The screen stashes it and closes this sheet; the
    /// child browser opens from `.onDisappear`, which is deterministic against
    /// the dismissal where a guessed delay is not (`ChatScreen` makes the same
    /// move for the message index).
    let onPick: (ChatSubagentSummary) -> Void

    @ObservedObject private var lang = Lang.shared
    @State private var items: [ChatSubagentSummary] = []
    @State private var hasMoreOlder = false
    @State private var loadingOlder = false
    @State private var loaded = false
    /// The last read failed. Kept apart from "empty" for the same reason the
    /// message index keeps `outlineFailed` apart: an OLDER GATEWAY 404s these
    /// routes, and answering that with "this chat has no subagents" is a lie
    /// about the conversation rather than a fact about the connection.
    @State private var failed = false
    @State private var poll: Task<Void, Never>?
    /// Re-read while the sheet is open so a running child's status and clock
    /// stay honest, and so a child spawned WHILE the sheet is up appears.
    private static let pollInterval = Duration.seconds(3)

    static let rowIdentifier = "subagent-row"

    var body: some View {
        VStack(spacing: 0) {
            grabber
            titleRow
            Rectangle()
                .fill(Theme.line)
                .frame(height: 1)
            content
        }
        .task { await load() }
        .onAppear { startPolling() }
        .onDisappear { poll?.cancel() }
    }

    /// Hand-rolled rather than `.presentationDragIndicator(.visible)`, which
    /// sits inside the content inset and pushes the title down.
    private var grabber: some View {
        Capsule()
            .fill(Theme.inkSoft.opacity(0.35))
            .frame(width: 36, height: 4)
            .padding(.vertical, 9)
    }

    private var titleRow: some View {
        HStack(spacing: 12) {
            Text(verbatim: lang.t("chat.subagents"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
            Spacer(minLength: 0)
            Text(verbatim: hasMoreOlder ? "\(items.count)+" : "\(items.count)")
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
        }
        .padding(.horizontal, 24)
        .frame(height: 40)
    }

    @ViewBuilder
    private var content: some View {
        if items.isEmpty {
            emptyState
        } else {
            entryList
        }
    }

    /// Three states, and they must not be confused: still loading, genuinely
    /// empty (a conversation that never delegated, or whose only spawn scrolled
    /// out of the window that lit the header entry), and unreachable.
    private var emptyState: some View {
        VStack {
            Spacer()
            Text(verbatim: lang.t(emptyStateKey))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    private var entryList: some View {
        List {
            if hasMoreOlder {
                loadOlderRow
            }
            ForEach(items, id: \.sessionId) { item in
                Button { onPick(item) } label: { row(item) }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier(Self.rowIdentifier)
                    .listRowBackground(Theme.paper)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 6, leading: 24, bottom: 6, trailing: 24))
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .background(Theme.paper)
    }

    private func row(_ item: ChatSubagentSummary) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(
                verbatim: SubagentList.title(
                    task: item.task, subagentType: item.subagentType,
                    sessionId: item.sessionId)
            )
            .font(Theme.mono(14))
            .foregroundStyle(Theme.ink)
            .lineLimit(2)
            .multilineTextAlignment(.leading)

            HStack(spacing: 8) {
                let subtitle = SubagentList.subtitle(
                    subagentType: item.subagentType, backend: item.backend)
                if !subtitle.isEmpty {
                    Text(verbatim: subtitle)
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.inkSoft)
                }
                Text(verbatim: lang.t(SubagentList.statusKey(item.status)))
                    .font(Theme.mono(11))
                    .foregroundStyle(item.status == .failed ? Theme.err : Theme.inkSoft)
                if let elapsed = SubagentList.elapsed(
                    startedAt: SubagentList.date(item.startedAt),
                    endedAt: SubagentList.date(item.endedAt))
                {
                    Text(verbatim: SubagentList.durationLabel(elapsed))
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.inkSoft)
                }
                Spacer(minLength: 0)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
    }

    /// The same affordance the message index offers, for the same reason: the
    /// fan-out limiter bounds CONCURRENT breadth, not the cumulative count, so
    /// a long conversation's oldest children live below the first page.
    private var loadOlderRow: some View {
        Button {
            Task { await loadOlder() }
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
        // Dimmed rather than spun: the rows below stay readable, and a spinner
        // over a list about to grow upward reads as a stall.
        .opacity(loadingOlder ? 0.4 : 1)
        .disabled(loadingOlder)
        .listRowBackground(Theme.paper)
        .listRowSeparator(.hidden)
        .listRowInsets(EdgeInsets(top: 6, leading: 24, bottom: 6, trailing: 24))
    }

    private var emptyStateKey: String {
        if failed { return "subagent.unavailable" }
        return loaded ? "subagent.empty" : "subagent.loading"
    }

    private func startPolling() {
        guard poll == nil else { return }
        poll = Task {
            while !Task.isCancelled {
                try? await Task.sleep(for: Self.pollInterval)
                if Task.isCancelled { return }
                await load()
            }
        }
    }

    /// Refresh the NEWEST page and fold it into what is already on screen.
    ///
    /// Not an assignment: this runs every three seconds, and a reader who has
    /// paged back would watch their older rows vanish under them. Merging by
    /// session id also lets a child spawned while the sheet is open appear
    /// without disturbing anything above it.
    private func load() async {
        do {
            let list = try await client.chatListSubagents(sessionId: sessionId, before: nil)
            merge(list.items)
            // Only the newest page can answer this, and only while nothing
            // older has been pulled in yet — once it has, the flag belongs to
            // the oldest page fetched, which `loadOlder` owns.
            if items.count <= list.items.count { hasMoreOlder = list.hasMoreOlder }
            failed = false
        } catch {
            // Keep whatever is on screen: a transient failure mid-poll must not
            // blank a list the reader is already looking at. It only changes
            // what an EMPTY sheet says.
            NSLog("baybo: subagent list: %@", bayboErrorText(error))
            failed = true
        }
        loaded = true
    }

    private func loadOlder() async {
        guard !loadingOlder, let oldest = items.first else { return }
        loadingOlder = true
        defer { loadingOlder = false }
        do {
            let list = try await client.chatListSubagents(
                sessionId: sessionId,
                before: SubagentCursor(
                    createdAt: oldest.createdAt, sessionId: oldest.sessionId))
            merge(list.items)
            hasMoreOlder = list.hasMoreOlder
        } catch {
            NSLog("baybo: subagent page: %@", bayboErrorText(error))
        }
    }

    /// Union by session id, ascending by `createdAt` with the id as tie-break —
    /// the gateway's own order, and one turn's fan-out really does mint
    /// siblings inside the same microsecond.
    private func merge(_ incoming: [ChatSubagentSummary]) {
        var byId: [String: ChatSubagentSummary] = [:]
        for item in items { byId[item.sessionId] = item }
        // Incoming wins: it carries the fresher status and clock.
        for item in incoming { byId[item.sessionId] = item }
        items = byId.values.sorted {
            ($0.createdAt, $0.sessionId) < ($1.createdAt, $1.sessionId)
        }
    }
}
