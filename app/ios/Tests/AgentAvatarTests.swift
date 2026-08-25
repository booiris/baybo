import Foundation
import Testing

@testable import Baybo

/// The uploaded picture an agent wears.
@MainActor
struct AgentAvatarTests {
    private func member(_ id: String, avatar: String?) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: id, name: id, description: "", avatarBlobId: avatar,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    /// Nothing to draw is not a fetch. An agent without a picture must not
    /// cost a round trip on every board that draws it.
    @Test func anAgentWithoutAPictureAsksForNothing() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        avatars.load(team: [member("a", avatar: nil), member("b", avatar: "")])
        #expect(avatars.image(for: nil) == nil)
        #expect(avatars.image(for: "") == nil)
        #expect(avatars.images.isEmpty)
    }

    /// The store is keyed by BLOB, not by agent: replacing an avatar mints a
    /// new blob, so a stale picture cannot survive under the agent's key — and
    /// two agents sharing one image cost one fetch, not two.
    @Test func twoAgentsSharingOneImageShareOneEntry() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        let shared = "\(AgentAvatars.demoPrefix)112233"
        avatars.load(team: [member("a", avatar: shared), member("b", avatar: shared)])
        #expect(avatars.images.count == 1)
        #expect(avatars.image(for: shared) != nil)
    }

    /// Logout takes the faces: a blob id means nothing under the next gateway,
    /// and a cached picture would be a departed account's agent looking back
    /// from the new one's board.
    @Test func logoutTakesTheFaces() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        avatars.load(blobId: "\(AgentAvatars.demoPrefix)445566")
        #expect(!avatars.images.isEmpty)
        avatars.reset()
        #expect(avatars.images.isEmpty)
    }

    /// A board holds the roster; a row holds a handle. The lookup lives on the
    /// board so a row never hunts the team for a picture.
    @Test func theBoardResolvesAnAgentsPicture() {
        let board = ProjectsStore.Board(
            issues: [], runs: [],
            team: [member("a-dev", avatar: "blob-1"), member("a-doc", avatar: nil)])
        #expect(board.avatarBlobId(forAgent: "a-dev") == "blob-1")
        #expect(board.avatarBlobId(forAgent: "a-doc") == nil)
        // An agent that left the board still resolves to nothing rather than
        // throwing — the card outlives the teammate.
        #expect(board.avatarBlobId(forAgent: "a-gone") == nil)
    }
}
