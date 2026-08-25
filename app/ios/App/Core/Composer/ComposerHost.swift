import Foundation

/// What a composer's staging machine needs from the surface it belongs to.
///
/// `ComposerStaging` used to hold a `weak var store: ChatStore?`, and that one
/// field was the whole reason a project card could not have an attachment
/// strip: the machine is 800 lines of picks, spools, uploads and drafts, none
/// of which is about a chat, hanging off a type that is entirely about one.
/// Two things were actually wanted from it — a place to put a line of text,
/// and a name to file the draft under — so those are what the seam carries.
///
/// **Deliberately no `send`.** Sending is where the two surfaces genuinely
/// differ: a chat mints a message id, paints an optimistic bubble, writes an
/// outbox row and sends through a connection gate; a card posts one REST
/// comment and then, conditionally, lifts a block. Putting a `send` verb here
/// would collapse those into one body behind a branch, and the first feature
/// that needed the card's comment-then-unblock ordering would have to grow it
/// an `if`. The machine's door out is `claimSend()`, which answers WHAT to
/// send and never sends it.
@MainActor
protocol ComposerHost: AnyObject {
    /// Where this surface's draft is filed. Read once, at init — a late upload
    /// writing the draft back can find the host already gone.
    var draftKey: DraftKey { get }

    /// A line of text under the composer's own rows, `nil` for none.
    ///
    /// Read as well as written, and that is load-bearing: the strip retracts
    /// its own line by checking the slot still says what it put there. A model
    /// failure raised in between owns the line from that moment, and a ✕ on a
    /// tile must not clear somebody else's sentence.
    var notice: String? { get set }
}

/// Which drafts root a key lives in.
///
/// Two roots, not one namespace, and the reason is a live trap:
/// `AppStore.unsentDraftSessionId` enumerates every directory under the chat
/// root and treats an unlisted, outbox-free one as the abandoned new chat that
/// the compose button should resume. A card's comment draft filed beside them
/// would open as a conversation — and `SessionIndex`'s own sweeps would delete
/// it under chat rules. `DraftStore.sessionIds` therefore scans `.chat` and
/// only `.chat`.
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
