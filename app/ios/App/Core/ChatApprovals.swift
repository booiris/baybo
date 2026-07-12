import SwiftUI

/// One tool call blocked on the user's decision — the native mirror of the
/// gateway's `ApprovalCard` / `approval_requested` frame.
///
/// `callId` identifies the PROMPT (a fresh id per prompt; one call can prompt
/// more than once) and is what `chatResolveApproval` answers with. `toolCallId`
/// identifies the TOOL CALL the prompt blocks — the id the work block's step
/// carries — so the transcript can badge the exact step that is waiting, and so
/// a `tool_completed` for that call retires a card the gate already timed out.
struct PendingApproval: Identifiable, Equatable {
    let callId: String
    let toolCallId: String?
    let tool: String
    /// The tool's own label for the call (Bash's `description`), when it wrote one.
    let description: String?
    /// Truncated JSON preview of the call's parameters.
    let paramsPreview: String
    /// Human lines for the resources the call wants ("run: rm -rf x").
    let accesses: [String]

    var id: String { callId }

    /// Both shapes — the live `approval_requested` frame and a `subscribe_state`
    /// bundle's card — carry the same fields (the frame just adds `session_id`),
    /// so one reader serves both. `nil` for a shape we can't render.
    /// `@MainActor` for `Lang` (the access lines are localized as they are read;
    /// every caller is the main-actor frame path anyway).
    @MainActor
    static func from(frame: [String: Any]) -> PendingApproval? {
        guard let callId = frame["call_id"] as? String, !callId.isEmpty,
            let tool = frame["tool"] as? String
        else { return nil }
        return PendingApproval(
            callId: callId,
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

/// The pending-approval queue, derived from the frame stream. Extracted from
/// `ChatStore` because it is the app's densest logic per line and has ZERO
/// dependencies once separated — no FFI, no bridge, no filesystem.
///
/// Four inputs, matching the four ways a prompt appears or goes away:
/// * `approval_requested` — a new prompt. DEDUPED: the gate's waker re-fires on
///   the newest queue entry, so the same card legitimately arrives twice;
/// * `approval_resolved` — someone answered (this device, or another client).
///   It is broadcast to EVERY session's sink, so it is matched by PROMPT id and
///   never by session;
/// * `subscribe_state.pending_approvals` — the authoritative set on
///   (re)subscribe, which REPLACES the queue (appending would resurrect a card
///   the user already answered);
/// * `tool_completed` — the blocked call finished. A gate that TIMES OUT denies
///   server-side and broadcasts NO `approval_resolved`, so this is the only
///   signal that retires that card.
struct ApprovalQueue {
    private(set) var cards: [PendingApproval] = []

    /// Fold one frame in; `true` when `cards` actually changed. The caller runs
    /// this on EVERY frame (streamed deltas included), so an unchanged fold must
    /// not republish.
    @MainActor
    mutating func apply(_ frame: [String: Any]) -> Bool {
        switch frame["kind"] as? String {
        case "approval_requested":
            guard let card = PendingApproval.from(frame: frame),
                !cards.contains(where: { $0.callId == card.callId })
            else { return false }
            cards.append(card)
            return true
        case "approval_resolved":
            guard let callId = frame["call_id"] as? String else { return false }
            return resolve(callId: callId)
        case "subscribe_state":
            let authoritative = (frame["pending_approvals"] as? [[String: Any]] ?? [])
                .compactMap(PendingApproval.from(frame:))
            guard authoritative != cards else { return false }
            cards = authoritative
            return true
        case "tool_completed":
            // `call_id` here is the BLOCKED TOOL CALL's id, not the prompt's —
            // the same JSON key carries a different id on this frame than it
            // does on `approval_resolved`, so this arm matches `toolCallId`.
            // Retiring by `callId` instead would silently retire the wrong card
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
    /// answers, and the `approval_resolved` arm above.
    mutating func resolve(callId: String) -> Bool {
        guard cards.contains(where: { $0.callId == callId }) else { return false }
        cards.removeAll { $0.callId == callId }
        return true
    }
}
