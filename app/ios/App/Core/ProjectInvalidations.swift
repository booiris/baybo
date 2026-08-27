import Foundation

@MainActor
final class ProjectInvalidations {
    static let shared = ProjectInvalidations()

    struct Change {
        let projectId: String
        let scope: String
        /// The card, when the change names one.
        let issueNumber: Int64?
    }

    private var observers: [UUID: (Change) -> Void] = [:]

    func observe(_ handler: @escaping (Change) -> Void) -> Token {
        let id = UUID()
        observers[id] = handler
        return Token(id: id, owner: self)
    }

    func publish(projectId: String, scope: String, issueNumber: Int64?) {
        // Consumers treat every scope as dirty; scope only narrows optional work.
        let change = Change(projectId: projectId, scope: scope, issueNumber: issueNumber)
        for handler in observers.values { handler(change) }
    }

    func publishStale() {
        publish(projectId: "", scope: "stale", issueNumber: nil)
    }

    fileprivate func remove(_ id: UUID) {
        observers[id] = nil
    }

    /// Observation lifetime is RAII: releasing the token removes the callback.
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
