import SwiftUI

/// One tool call blocked on the user's decision — the native mirror of the
/// gateway's `ApprovalCard` / `approval_requested` frame.
///
/// `askId` identifies the PROMPT (a fresh id per prompt; one call can prompt
/// more than once) and is what `chatResolveApproval` answers with. `toolCallId`
/// identifies the TOOL CALL the prompt blocks — the id the work block's step
/// carries — so the transcript can badge the exact step that is waiting, and so
/// a `tool_completed` for that call retires a card whose close the client missed.
struct PendingApproval: Identifiable, Equatable, PendingChatPrompt {
    static let raisedKind = "approval_requested"
    static let snapshotKey = "pending_approvals"

    let askId: String
    let toolCallId: String?
    let tool: String
    /// The tool's own label for the call (Bash's `description`), when it wrote one.
    let description: String?
    /// Truncated JSON preview of the call's parameters.
    let paramsPreview: String
    /// Human lines for the resources the call wants ("run: rm -rf x").
    let accesses: [String]

    var id: String { askId }

    /// Both shapes — the live `approval_requested` frame and a `subscribe_state`
    /// bundle's card — carry the same fields (the frame just adds `session_id`),
    /// so one reader serves both. `nil` for a shape we can't render.
    /// `@MainActor` for `Lang` (the access lines are localized as they are read;
    /// every caller is the main-actor frame path anyway).
    @MainActor
    static func from(frame: [String: Any]) -> PendingApproval? {
        guard let askId = frame["ask_id"] as? String, !askId.isEmpty,
            let tool = frame["tool"] as? String
        else { return nil }
        return PendingApproval(
            askId: askId,
            toolCallId: frame["tool_call_id"] as? String,
            tool: tool,
            description: frame["description"] as? String,
            paramsPreview: (frame["params_preview"] as? String) ?? "",
            accesses: (frame["accesses"] as? [[String: Any]] ?? []).map(formatAccess)
        )
    }

    /// One line per resource the call declared. Mirrors the web chat's
    /// `formatAccess`; an unknown kind (a wire type this build predates) falls
    /// back to the raw kind rather than being dropped — a resource the user
    /// can't see is a resource they can't judge.
    @MainActor
    private static func formatAccess(_ access: [String: Any]) -> String {
        let lang = Lang.shared
        switch (access["kind"] as? String) ?? "?" {
        case "read_file":
            return lang.t("chat.accessRead", (access["path"] as? String) ?? "")
        case "write_file":
            return lang.t("chat.accessWrite", (access["path"] as? String) ?? "")
        case "http":
            let host = (access["host"] as? String) ?? ""
            return host == "*" ? lang.t("chat.accessNetwork") : lang.t("chat.accessHttp", host)
        case "exec_command":
            return lang.t("chat.accessExec", (access["command"] as? String) ?? "")
        case "env":
            let vars = (access["vars"] as? [String]) ?? []
            return vars.isEmpty
                ? lang.t("chat.accessEnv")
                : lang.t("chat.accessEnvVars", vars.joined(separator: ", "))
        case let kind:
            return kind
        }
    }
}

protocol PendingChatPrompt: Equatable {
    static var raisedKind: String { get }
    static var snapshotKey: String { get }
    var askId: String { get }
    var toolCallId: String? { get }
    @MainActor static func from(frame: [String: Any]) -> Self?
}

typealias ApprovalQueue = PendingPromptQueue<PendingApproval>

/// Live requests append once per ask id. Subscribe snapshots replace the
/// pending set. A session-less close is matched by ask id, and a completed
/// tool call retires any prompt whose close the client missed.
struct PendingPromptQueue<Card: PendingChatPrompt> {
    private(set) var cards: [Card] = []

    /// Fold one frame in; `true` when `cards` actually changed. The caller runs
    /// this on EVERY frame (streamed deltas included), so an unchanged fold must
    /// not republish.
    @MainActor
    mutating func apply(_ frame: [String: Any]) -> Bool {
        switch frame["kind"] as? String {
        case Card.raisedKind:
            guard let card = Card.from(frame: frame),
                !cards.contains(where: { $0.askId == card.askId })
            else { return false }
            cards.append(card)
            return true
        case "ask_resolved":
            guard let askId = frame["ask_id"] as? String else { return false }
            return resolve(askId: askId)
        case "subscribe_state":
            let authoritative = (frame[Card.snapshotKey] as? [[String: Any]] ?? [])
                .compactMap(Card.from(frame:))
            guard authoritative != cards else { return false }
            cards = authoritative
            return true
        case "tool_completed":
            // `call_id` here is the BLOCKED TOOL CALL's id, not the prompt's —
            // the same JSON key carries a different id on this frame than it
            // does on `approval_requested`, so this arm matches `toolCallId`.
            // Retiring by `askId` instead would silently retire the wrong card
            // (or none) and still compile.
            guard let toolCallId = frame["call_id"] as? String,
                cards.contains(where: { $0.toolCallId == toolCallId })
            else { return false }
            cards.removeAll { $0.toolCallId == toolCallId }
            return true
        default:
            return false
        }
    }

    /// Retire a prompt by its own id — the optimistic dismiss when the user
    /// answers, and the `ask_resolved` arm above.
    mutating func resolve(askId: String) -> Bool {
        guard cards.contains(where: { $0.askId == askId }) else { return false }
        cards.removeAll { $0.askId == askId }
        return true
    }
}
