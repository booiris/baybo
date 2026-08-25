import Foundation

/// The card timeline, decoded far enough for the native side to answer three
/// questions: what is waiting on an answer, whether an agent is asking one,
/// and who said the last thing.
///
/// The entries arrive as raw gateway JSON because their real consumer is the
/// issue webview, which renders the gateway's own shape — see
/// `BayboClient.projectIssueEvents`. What is decoded here is the handful of
/// fields the native dock and the Waiting strip need; every other kind rides
/// through as its raw `kind`, so a gateway that grows one costs a row its
/// sentence and nothing else.
struct IssueEvent: Equatable {
    enum ActorKind: Equatable {
        case user
        case system
        case agent(id: String, handle: String)

        var isAgent: Bool { if case .agent = self { true } else { false } }
    }

    let id: String
    let actor: ActorKind
    /// The body's own `kind`, verbatim — `comment`, `blocked`, `run_settled`,
    /// or something this build has never heard of.
    let kind: String
    let createdAtMs: Int64
    /// `approval_requested` / `approval_resolved`: the prompt's id, which is
    /// what an answer names. Distinct from the blocked tool call's own id.
    let callId: String?
    let tool: String?
    let summary: String?
    let decision: String?
    /// `blocked`: why work stopped, which is the agent's question when an
    /// agent is the one who wrote it.
    let reason: String?
    let text: String?

    /// Decode the `{"items":[…]}` envelope `projectIssueEvents` answers with.
    ///
    /// Tolerant by construction: an entry missing the fields this build reads
    /// still decodes, and an unknown `kind` is carried rather than dropped —
    /// the timeline is rendered by the webview, and a native decoder that
    /// threw on a new kind would take the whole card's Activity with it.
    static func decodeList(_ json: String) throws -> [IssueEvent] {
        try decodeTimeline(json).events
    }

    /// The same envelope, read whole: the entries AND `first_unread`, the
    /// entry the operator has not seen yet.
    ///
    /// One pass rather than two calls over the same bytes — a card's timeline
    /// is the largest thing this app decodes on the main actor, and it is
    /// re-read on every frame its board sends.
    ///
    /// The id is the gateway's answer, never re-derived here: which entries
    /// count as unread is one rule with one home (`UNREAD_EVENT_PREDICATE`),
    /// and a second copy of it in this file would put the card page's rule
    /// somewhere the board's unread badge disagrees with.
    static func decodeTimeline(_ json: String) throws -> (
        events: [IssueEvent], firstUnread: String?
    ) {
        guard let data = json.data(using: .utf8) else { return ([], nil) }
        let root = try JSONSerialization.jsonObject(with: data)
        guard let object = root as? [String: Any],
            let items = object["items"] as? [[String: Any]]
        else { return ([], nil) }
        return (items.compactMap(IssueEvent.init(item:)), object["first_unread"] as? String)
    }

    init?(item: [String: Any]) {
        guard let id = item["id"] as? String else { return nil }
        let body = item["body"] as? [String: Any] ?? [:]
        guard let kind = body["kind"] as? String else { return nil }
        self.id = id
        self.kind = kind
        createdAtMs = (item["created_at_ms"] as? NSNumber)?.int64Value ?? 0
        let actor = item["actor"] as? [String: Any] ?? [:]
        switch actor["kind"] as? String {
        case "agent":
            let agentId = actor["id"] as? String ?? ""
            self.actor = .agent(id: agentId, handle: actor["handle"] as? String ?? agentId)
        case "system":
            self.actor = .system
        default:
            self.actor = .user
        }
        callId = body["call_id"] as? String
        tool = body["tool"] as? String
        summary = body["summary"] as? String
        decision = body["decision"] as? String
        reason = body["reason"] as? String
        text = body["text"] as? String
    }
}

/// A tool call parked on this card, waiting to be answered.
///
/// Distinct from the chat surface's `PendingApproval` on purpose, and not
/// merely to avoid the name: a chat prompt is derived from a subscribed
/// session's frame stream and answered over the WS, while a board prompt is
/// read off the card's timeline and answered by `call_id` over REST. The two
/// planes never meet — an issue session is excluded from every chat surface —
/// so one type spanning both would be a shape with two irreconcilable halves.
struct IssueApprovalPrompt: Equatable {
    let callId: String
    let tool: String
    let summary: String?
    /// Who asked. A coordination run's prompt is the lead's, not the card
    /// assignee's, so this is read off the entry rather than off the card.
    let askedBy: String?
    let askedAtMs: Int64
}

enum IssueTimeline {
    /// Prompts requested and not resolved, oldest first.
    ///
    /// A replay rather than a scan for the newest: a prompt is answered by
    /// `call_id`, one card can hold several across a run, and a resolution
    /// retires exactly one of them.
    ///
    /// **The live queue is the truth, not this.** A gateway restart drops
    /// every parked prompt without writing a resolution, and a prompt that
    /// times out self-denies the same way — so an entry surviving here can
    /// name a prompt nothing is waiting for. That is why the card's badge
    /// comes from `IssueDto.approval_pending` (which reads the queue) and
    /// this only supplies the `call_id` to answer with, and why a 404 on
    /// answering means "already closed" rather than a failure to retry.
    static func pendingApprovals(in events: [IssueEvent]) -> [IssueApprovalPrompt] {
        var open: [String: IssueApprovalPrompt] = [:]
        var order: [String] = []
        for event in events {
            guard let callId = event.callId else { continue }
            switch event.kind {
            case "approval_requested":
                if open[callId] == nil { order.append(callId) }
                open[callId] = IssueApprovalPrompt(
                    callId: callId,
                    tool: event.tool ?? "a tool",
                    summary: event.summary,
                    askedBy: {
                        if case let .agent(_, handle) = event.actor { return handle }
                        return nil
                    }(),
                    askedAtMs: event.createdAtMs
                )
            case "approval_resolved":
                open.removeValue(forKey: callId)
                order.removeAll { $0 == callId }
            default:
                continue
            }
        }
        return order.compactMap { open[$0] }
    }

    /// The agent's own question, when an agent is what stopped this card.
    ///
    /// The distinction matters and is the whole reason this exists: an
    /// operator's block is that operator saying stop, and nothing should
    /// invite them to answer themselves. An **agent-authored** block is a
    /// question nobody has come back to — the board's own driver deliberately
    /// asks nobody about a blocked card, so it is the one card nothing ever
    /// returns to on its own.
    ///
    /// Unlike an approval this never expires: it waits until somebody lifts
    /// the block, which is what makes it worth a row of its own next to
    /// prompts that self-deny in five minutes.
    static func agentQuestion(blockedReason: String?, events: [IssueEvent]) -> PendingQuestion? {
        guard let reason = blockedReason, !reason.isEmpty else { return nil }
        // The newest block is the one in force; an older one may have been
        // lifted and re-applied by somebody else entirely.
        guard let block = events.last(where: { $0.kind == "blocked" }),
            case let .agent(_, handle) = block.actor
        else { return nil }
        return PendingQuestion(askedBy: handle, question: reason, askedAtMs: block.createdAtMs)
    }

    struct PendingQuestion: Equatable {
        let askedBy: String
        let question: String
        let askedAtMs: Int64
    }

    /// Consecutive system entries fold into one row.
    ///
    /// A card's Activity is mostly machinery — moved, assigned, run started,
    /// run settled — and on a phone that buries the two things a person
    /// actually said. Comments, approvals and blocks are never folded: they
    /// are the reason anybody opened the card.
    static func fold(_ events: [IssueEvent]) -> [Fold] {
        var folded: [Fold] = []
        for event in events {
            if isAlwaysShown(event) {
                folded.append(.entry(event))
            } else if case let .system(runs)? = folded.last {
                folded[folded.count - 1] = .system(runs + [event])
            } else {
                folded.append(.system([event]))
            }
        }
        return folded
    }

    enum Fold: Equatable {
        case entry(IssueEvent)
        case system([IssueEvent])
    }

    private static func isAlwaysShown(_ event: IssueEvent) -> Bool {
        switch event.kind {
        case "comment", "approval_requested", "approval_resolved", "blocked": true
        default: false
        }
    }
}
