import Foundation

struct IssueEvent: Equatable {
    enum ActorKind: Equatable {
        case user
        case system
        case agent(id: String, handle: String)

        var isAgent: Bool { if case .agent = self { true } else { false } }
    }

    let id: String
    /// Client idempotency key on an operator comment. Native uses it only to
    /// retire the matching optimistic outbox row.
    let clientMsgId: String?
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

    static func decodeList(_ json: String) throws -> [IssueEvent] {
        try decodeTimeline(json).events
    }

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
        clientMsgId = item["client_msg_id"] as? String
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

struct IssuePerson: Equatable {
    let handle: String
    /// The blob the page fetches over the bridge. Most agents have none.
    let avatar: String?
    let monogram: String
}

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
    /// Replays history into candidate prompts. Callers must still gate results
    /// with live `approval_pending` state before offering an action.
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

}
