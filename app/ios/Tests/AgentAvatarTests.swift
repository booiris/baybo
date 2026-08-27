import Foundation
import Testing

@testable import Baybo

@MainActor
struct AgentAvatarTests {
    private func member(_ id: String, avatar: String?) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: id, name: id, description: "", avatarBlobId: avatar,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    @Test func anAgentWithoutAPictureAsksForNothing() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        avatars.load(team: [member("a", avatar: nil), member("b", avatar: "")])
        #expect(avatars.image(for: nil) == nil)
        #expect(avatars.image(for: "") == nil)
        #expect(avatars.images.isEmpty)
    }

    @Test func twoAgentsSharingOneImageShareOneEntry() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        let shared = "\(AgentAvatars.demoPrefix)112233"
        avatars.load(team: [member("a", avatar: shared), member("b", avatar: shared)])
        #expect(avatars.images.count == 1)
        #expect(avatars.image(for: shared) != nil)
    }

    @Test func logoutTakesTheFaces() {
        let avatars = AgentAvatars(clientProvider: { FakeBayboClient() })
        avatars.load(blobId: "\(AgentAvatars.demoPrefix)445566")
        #expect(!avatars.images.isEmpty)
        avatars.reset()
        #expect(avatars.images.isEmpty)
    }

    @Test func theBoardResolvesAnAgentsPicture() {
        let board = ProjectsStore.Board(
            issues: [], runs: [],
            team: [member("a-dev", avatar: "blob-1"), member("a-doc", avatar: nil)])
        #expect(board.avatarBlobId(forAgent: "a-dev") == "blob-1")
        #expect(board.avatarBlobId(forAgent: "a-doc") == nil)
        #expect(board.avatarBlobId(forAgent: "a-gone") == nil)
    }
}
