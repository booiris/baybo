import Foundation

/// Where a `ProjectChanged` frame goes.
///
/// Three surfaces can be watching one board at once — the cards root, the
/// board, and an open card with its run sheet over it — and the frame is one
/// broadcast. A store that reached into the others to nudge them would make
/// each new surface an edit to every existing one; this is the seam instead:
/// the relay publishes, and whoever is on screen listens.
///
/// Deliberately not a Combine subject on `ProjectsStore`: a run sheet has no
/// business holding the boards store to learn that its own run moved, and the
/// board has no business knowing a run sheet exists.
@MainActor
final class ProjectInvalidations {
    static let shared = ProjectInvalidations()

    struct Change {
        let projectId: String
        /// `project` | `board` | `run` | `timeline`, or a word this build has
        /// never heard of — carried, never matched on for the decision to
        /// refetch. **Every scope means dirty.**
        let scope: String
        /// The card, when the change names one.
        let issueNumber: Int64?
    }

    private var observers: [UUID: (Change) -> Void] = [:]

    /// Subscribe until the returned token is dropped.
    ///
    /// A token rather than an explicit `remove`: every caller here is a screen
    /// whose lifetime IS the subscription's, and an unsubscribe somebody has to
    /// remember is one a `.onDisappear` racing a `deinit` eventually forgets.
    func observe(_ handler: @escaping (Change) -> Void) -> Token {
        let id = UUID()
        observers[id] = handler
        return Token(id: id, owner: self)
    }

    func publish(projectId: String, scope: String, issueNumber: Int64?) {
        let change = Change(projectId: projectId, scope: scope, issueNumber: issueNumber)
        for handler in observers.values { handler(change) }
    }

    /// The gateway dropped a broadcast, so everything on screen is suspect.
    /// Published as a `project`-scoped change with no board, which every
    /// observer reads as "refetch whatever you are showing".
    func publishStale() {
        publish(projectId: "", scope: "stale", issueNumber: nil)
    }

    fileprivate func remove(_ id: UUID) {
        observers[id] = nil
    }

    final class Token {
        private let id: UUID
        private weak var owner: ProjectInvalidations?

        fileprivate init(id: UUID, owner: ProjectInvalidations) {
            self.id = id
            self.owner = owner
        }

        deinit {
            let id = self.id
            let owner = self.owner
            MainActor.assumeIsolated { owner?.remove(id) }
        }
    }
}
