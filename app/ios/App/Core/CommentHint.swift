import Foundation

/// What sending this comment will do, said before it is sent.
///
/// **This is a third copy of a rule that lives in Rust.**
/// `crates/project/src/comments.rs::comment_delivery` decides what a comment
/// does besides being recorded, and the decision is not exposed over REST —
/// a composer has to say what sending will do while the text is still being
/// typed, so it cannot ask the server. `app/web`'s `timelineModel.commentHint`
/// is the second copy; this is the third.
///
/// Nothing enforces the correspondence — not a generated binding, not a
/// shared schema — except the golden vectors in
/// `app/web/src/pages/projects/commentHintVectors.json`, which this port and
/// the web's own suite both assert against. Widening what counts as live work
/// on the server, or adding a run state that reads as idle, is a change in
/// three places in one commit; two of them will compile and pass regardless.
enum CommentHint {
    /// `budgetModel.HELD_RUN_NOTE`. Never spell the reason out as "over its
    /// daily budget": an operator told that on a token-limited board goes and
    /// raises a dollar figure that was never what stopped it.
    static let heldRunNote = "the project is over one of its daily ceilings"

    /// What a plain comment does. `assigneeHandle` is already resolved —
    /// [`handle(forAgent:in:)`] falls back to the raw id exactly as the web
    /// does, so an agent that left the board still reads as itself.
    static func text(
        status: IssueStatus,
        assigneeHandle: String?,
        cancelled: Bool,
        blockedReason: String?,
        liveRunStatus: RunStatus?
    ) -> String {
        guard let assignee = assigneeHandle else {
            return "Records only — nobody is assigned to this issue yet."
        }
        if cancelled {
            return "Records only — this issue is cancelled."
        }
        if status == .backlog || status == .done {
            return "Records only — @\(assignee) is not working on this right now."
        }
        // A block takes precedence over any live run, matching the server: a
        // block parks the run everywhere the board acts on its own.
        if blockedReason != nil {
            return
                "Records only — a block has stopped this issue; @\(assignee) picks this up when it is lifted."
        }
        switch liveRunStatus {
        case .held:
            return "@\(assignee) will read this when the held run starts — \(heldRunNote)."
        case .queued:
            return "@\(assignee) will read this when the queued run starts."
        case .running:
            return "@\(assignee) is mid-run — this is picked up when that run finishes."
        default:
            return "Starts a run: @\(assignee) will read this now."
        }
    }

    /// What an `@mention` in the draft does, or `nil` when it does nothing the
    /// plain hint does not already say.
    ///
    /// Only an **unassigned** card's first mention staffs anybody: a mention
    /// on a card somebody is already on is a question, never a reassignment.
    /// And a mention on a blocked card is recorded and staffs nobody — the
    /// composer says so rather than promising a handover that will not happen.
    static func mention(
        assigneeHandle: String?,
        blockedReason: String?,
        draft: String,
        teamHandles: [String]
    ) -> String? {
        guard assigneeHandle == nil else { return nil }
        guard let handle = firstMention(in: draft), teamHandles.contains(handle) else {
            return nil
        }
        if blockedReason != nil {
            return
                "Records only — a block has stopped this issue; @\(handle) is not put on it until it is lifted."
        }
        return "Sending this puts @\(handle) on the issue."
    }

    /// The first `@handle` in the draft, by the server's own handle grammar.
    ///
    /// Mirrors `mentionModel.mentionHint`'s `/(^|[\s(])@([a-z0-9-]+)/`: a
    /// mention starts the text or follows whitespace or an open paren, so an
    /// email address is not one. Trailing hyphens are trimmed because a
    /// handle never ends in one and `@dev-` mid-sentence would otherwise miss.
    static func firstMention(in draft: String) -> String? {
        let scalars = Array(draft)
        var index = 0
        while index < scalars.count {
            guard scalars[index] == "@" else {
                index += 1
                continue
            }
            let precededCorrectly =
                index == 0 || scalars[index - 1].isWhitespace || scalars[index - 1] == "("
            guard precededCorrectly else {
                index += 1
                continue
            }
            var end = index + 1
            while end < scalars.count, isHandleCharacter(scalars[end]) {
                end += 1
            }
            let handle = String(scalars[(index + 1)..<end])
            guard !handle.isEmpty else {
                index += 1
                continue
            }
            let trimmed = String(handle.reversed().drop { $0 == "-" }.reversed())
            return trimmed.isEmpty ? nil : trimmed
        }
        return nil
    }

    private static func isHandleCharacter(_ c: Character) -> Bool {
        c.isASCII && (c.isLowercase && c.isLetter || c.isNumber || c == "-")
    }

    /// A teammate's `@handle`, falling back to the raw agent id — an id from
    /// another board, or one whose teammate has been removed, still resolves
    /// to something the operator can see rather than to nothing.
    static func handle(forAgent agentId: String, in team: [TeamMemberInfo]) -> String {
        team.first { $0.id == agentId }?.handle ?? agentId
    }
}
