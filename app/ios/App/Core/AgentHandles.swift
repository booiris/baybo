import Foundation

enum AgentHandles {
    static func handle(forAgent agentId: String, in team: [TeamMemberInfo]) -> String {
        team.first { $0.id == agentId }?.handle ?? agentId
    }
}
