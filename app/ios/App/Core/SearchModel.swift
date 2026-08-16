import Foundation

/// The search screen's querying half, apart from its rendering half.
///
/// Extracted so the rules that are easy to get wrong — the debounce, the
/// staleness guard, the composition gate — are testable without a UI host, the
/// same shape `MessageOutline` and `ApprovalQueue` already take. The screen
/// keeps the field and the cards; everything below decides what actually
/// reaches the gateway.
@MainActor
final class SearchModel: ObservableObject {
    enum Phase {
        case idle
        case loading
        case ok(groups: [ChatSearchGroup], truncated: Bool)
        case failed
    }

    /// Long enough that typing a word is one request, short enough to feel live.
    ///
    /// Longer than app/web's 200ms deliberately: that panel talks to localhost,
    /// while this one may cross a relay tunnel budgeted at 15s to first byte, so
    /// a keystroke costs materially more here.
    nonisolated static let defaultDebounce: Duration = .milliseconds(300)

    /// Below this a CJK query matches nearly everything and the result list is
    /// noise — the index makes every Han codepoint its own token, so one
    /// character is a legitimate but useless query. Matches app/web's
    /// `MIN_QUERY_LEN`.
    nonisolated static let minQueryLength = 2

    @Published private(set) var phase: Phase = .idle

    private let client: any BayboClientProtocol
    private let debounce: Duration
    /// Whether an input method has an open composition. Injected so a test can
    /// state it; in the app it is the responder-chain probe.
    private let isComposing: () -> Bool

    private var task: Task<Void, Never>?
    /// Monotonic request id. Cancelling the task is not enough on its own: a
    /// request already awaiting its answer cannot be un-sent, and on the relay
    /// leg answers genuinely can land out of order — so the LAST issued query is
    /// the only one allowed to write `phase`.
    private var sequence = 0

    init(
        client: any BayboClientProtocol = Baybo.client,
        debounce: Duration = SearchModel.defaultDebounce,
        isComposing: @escaping () -> Bool = { FocusedTextInput.isComposing }
    ) {
        self.client = client
        self.debounce = debounce
        self.isComposing = isComposing
    }

    deinit { task?.cancel() }

    func cancel() {
        task?.cancel()
        task = nil
    }

    /// The field changed. Decides whether this is worth a round trip, and when.
    func update(query: String) {
        task?.cancel()
        sequence += 1
        let seq = sequence
        let trimmed = query.trimmingCharacters(in: .whitespacesAndNewlines)

        guard trimmed.count >= Self.minQueryLength else {
            phase = .idle
            return
        }
        task = Task { [weak self] in
            try? await Task.sleep(for: self?.debounce ?? Self.defaultDebounce)
            if Task.isCancelled { return }
            await self?.fire(trimmed, seq: seq)
        }
    }

    /// The debounce has elapsed: decide whether this query is worth a round trip
    /// and, if so, take it.
    private func fire(_ text: String, seq: Int) async {
        // The composition is checked HERE, after the debounce — NOT when the
        // binding changed.
        //
        // A CJK keyboard puts the uncommitted pinyin in the binding, and
        // searching that costs a tunnel round trip and flashes "no matches"
        // against a query nobody typed. But checking at the moment of the change
        // threw away the one change that mattered: tapping a candidate updates
        // the binding to 数据 while UIKit has STILL not cleared
        // `markedTextRange`, so the commit itself read as "composing", was
        // skipped, and nothing retried afterwards. Reported from a device as
        // two letters plus a candidate searching nothing while three letters or
        // an English keyboard worked.
        //
        // By the time the debounce is up the commit has landed and the flag is
        // clear, so the same check now admits exactly what it should.
        guard !isComposing() else { return }

        #if DEBUG
            // `-baybo-demo-search`: canned results over the `-baybo-open-home`
            // demo rows, so the search surface is drivable headlessly. There is
            // no gateway in that mode, and a screen whose every state is an
            // error page cannot smoke-test a result card or a jump.
            if let demo = Self.demoResults(for: text) {
                phase = .ok(groups: demo.groups, truncated: demo.truncated)
                return
            }
        #endif

        // Only the first query blanks the view. After that the previous results
        // stay up while the next answer is in flight — on a relay leg the wait is
        // long enough that clearing would read as "your results vanished".
        if case .ok = phase {} else { phase = .loading }
        await run(text, seq: seq)
    }

    #if DEBUG
        /// Canned hits over the `-baybo-open-home` demo conversations. Two
        /// groups, so grouping and the per-excerpt tap targets are both real;
        /// the ordinals are the ones `-baybo-demo-frames` writes into
        /// `debug-session`, so a jump has somewhere to land.
        static func demoResults(for query: String) -> ChatSearchResults? {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-search") else {
                return nil
            }
            func hit(_ ordinal: Int64, _ role: String, _ text: String) -> ChatSearchHit {
                ChatSearchHit(
                    ordinal: ordinal, role: role, text: text,
                    createdAt: "2026-08-12T09:00:00Z", supersededBy: nil)
            }
            return ChatSearchResults(
                groups: [
                    ChatSearchGroup(
                        sessionId: "demo-1", sessionTitle: nil,
                        hits: [
                            hit(2, "user", "Demo conversation number 1 mentions \(query) here"),
                            hit(3, "assistant", "The reply also covers \(query) in passing"),
                        ],
                        totalHits: 5),
                    ChatSearchGroup(
                        sessionId: "demo-2", sessionTitle: nil,
                        hits: [hit(1, "user", "A second conversation about \(query)")],
                        totalHits: 1),
                ],
                truncated: false)
        }
    #endif

    private func run(_ text: String, seq: Int) async {
        do {
            let results = try await client.chatSearch(query: text)
            guard seq == sequence else { return }
            phase = .ok(groups: results.groups, truncated: results.truncated)
        } catch {
            // A superseded query's failure is not this query's failure.
            guard seq == sequence else { return }
            phase = .failed
        }
    }
}
