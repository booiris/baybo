import Foundation

/// What to call an agent, given the board's roster.
///
/// A card's DTOs carry profile IDs and nothing else, and three surfaces have
/// to print a name from one — the board's rows, the card page's payload, the
/// run log in a card's ⋯ — so the fallback belongs in one place rather than in
/// each of them.
///
/// This file was `CommentHint.swift` until 2026-08-26 and held the iOS port of
/// `comments::comment_delivery` beside this: the rule deciding what a comment
/// does besides being recorded, said in the composer while the text was still
/// being typed. The card dock no longer says it, so the port and the golden
/// vectors that held it to `app/web`'s copy are gone with it. The rule still
/// has two implementations — the gateway's and the web dashboard's — and if a
/// hint ever comes back here it needs a third and a gate to hold it.
enum AgentHandles {
    /// A teammate's `@handle`, falling back to the raw agent id — an id from
    /// another board, or one whose teammate has been removed, still resolves
    /// to something the operator can see rather than to nothing.
    static func handle(forAgent agentId: String, in team: [TeamMemberInfo]) -> String {
        team.first { $0.id == agentId }?.handle ?? agentId
    }
}
