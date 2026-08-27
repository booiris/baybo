import Foundation

/// The staging seam intentionally exposes only draft identity and notices;
/// chat sends and issue comments have different ordering and durability rules.
@MainActor
protocol ComposerHost: AnyObject {
    /// Where this surface's draft is filed. Read once, at init — a late upload
    /// writing the draft back can find the host already gone.
    var draftKey: DraftKey { get }

    var notice: String? { get set }
}

/// Card drafts live outside the chat root so chat resume/pruning can never
/// mistake a card comment for an abandoned conversation.
enum DraftScope: String {
    case chat = "drafts"
    case card = "card-drafts"
}

/// One draft's address on disk: which root, and which conversation or card.
struct DraftKey: Hashable {
    let scope: DraftScope
    /// The id within the scope. A session id for a chat; `project#number` for
    /// a card. Sanitised into a directory name by `DraftStore`.
    let id: String

    static func chat(_ sessionId: String) -> DraftKey {
        DraftKey(scope: .chat, id: sessionId)
    }

    static func card(project: String, number: Int64) -> DraftKey {
        DraftKey(scope: .card, id: "\(project)#\(number)")
    }
}
