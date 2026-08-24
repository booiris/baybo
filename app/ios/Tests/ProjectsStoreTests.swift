import Foundation
import Testing

@testable import Baybo

/// `ProjectsStore` with an injected fake: the mirror's cold paint, the REPLACE
/// refresh, and a write's rollback.
@MainActor
struct ProjectsStoreTests {
    private func project(_ id: String, name: String, archived: Bool = false) -> ProjectInfo {
        ProjectInfo(
            id: id, name: name, description: "", workdir: "/tmp/\(id)",
            dailyBudgetMicros: 5_000_000, dailyBudgetTokens: nil, maxParallelIssueRuns: 3,
            agentsMayMerge: false, archivedAtMs: archived ? 1 : nil, createdAtMs: 0,
            updatedAtMs: 0)
    }

    private func issue(_ number: Int64, title: String, unread: Int64 = 0) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: title, description: "", attachments: [],
            status: .todo, priority: .high, assignee: "a-dev", position: number, pinned: false,
            branch: nil, blockedReason: nil, parent: nil, filedFrom: nil, stage: 0,
            subIssues: SubIssueProgress(done: 1, total: 3), unread: unread, lastRunFailed: false,
            approvalPending: true, openedByAgent: false, cancelledAtMs: nil, createdAtMs: 0,
            updatedAtMs: 0)
    }

    private func member(_ id: String, _ handle: String, lead: Bool = false) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: handle, name: handle, description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: lead,
            hiredBy: nil, createdAtMs: 0)
    }

    /// A cold start paints the mirror before the network answers — the whole
    /// reason boards are mirrored at all.
    @Test func aSecondStorePaintsTheMirrorBeforeAnyFetch() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        fake.stubTeam = [member("a-dev", "dev-1")]

        let first = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await first.refreshRoot()
        await first.refreshBoard("p1")

        let second = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        #expect(second.projects.map(\.name) == ["rglide"])
        #expect(second.boards["p1"]?.issues.map(\.title) == ["the parser"])
        #expect(second.boards["p1"]?.team.first?.handle == "dev-1")
        // Enum words survive the round trip rather than degrading to unknown.
        #expect(second.boards["p1"]?.issues.first?.status == .todo)
        #expect(second.boards["p1"]?.issues.first?.priority == .high)
        #expect(second.boards["p1"]?.issues.first?.approvalPending == true)
        #expect(second.boards["p1"]?.issues.first?.subIssues?.total == 3)
    }

    /// A run's cost is NEVER mirrored: the active-run poll does not price runs,
    /// and a mirror that wrote `0` would report free work as fact.
    @Test func theMirrorRefusesToInventARunsCost() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubRuns = [
            IssueRunInfo(
                number: 12, attempt: 2, agentId: "a-dev", status: .running, trigger: .retry,
                sessionId: "s1", error: nil, createdAtMs: 1, startedAtMs: 2, settledAtMs: nil,
                costMicros: nil, inputTokens: nil, outputTokens: nil)
        ]
        let first = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await first.refreshRoot()
        await first.refreshBoard("p1")

        let second = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        let run = second.boards["p1"]?.runs.first
        #expect(run?.status == .running)
        #expect(run?.trigger == .retry)
        #expect(run?.costMicros == nil)
    }

    /// Boards with nothing waiting are ABSENT from `/attention`, not zeroed —
    /// so the map is rebuilt, or a board that just went quiet keeps yesterday's
    /// count forever.
    @Test func attentionIsReplacedNotMerged() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubAttention = [
            ProjectAttention(
                projectId: "p1", name: "rglide", approvals: 1, failed: 0, unread: 2)
        ]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        #expect(store.attention["p1"]?.approvals == 1)

        fake.stubAttention = []
        await store.refreshRoot()
        #expect(store.attention["p1"] == nil)
    }

    /// A refresh REPLACES a board. There is no local state worth protecting
    /// here, and a merge would only invent ways for the two to disagree.
    @Test func aRefreshReplacesTheBoardWholesale() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(1, title: "first"), issue(2, title: "second")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("p1")
        #expect(store.boards["p1"]?.issues.count == 2)

        fake.stubIssues = [issue(2, title: "second, renamed")]
        await store.refreshBoard("p1")
        #expect(store.boards["p1"]?.issues.map(\.title) == ["second, renamed"])
    }

    /// A failed write restores the board exactly as it was — the snapshot,
    /// never the inverse of the optimistic edit — and surfaces the server's own
    /// sentence, which is the only part the operator can act on.
    @Test func aFailedWriteRollsBackToTheSnapshotAndKeepsTheServersWords() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        await store.refreshBoard("p1")

        let refused = await store.write(
            board: "p1",
            apply: { $0.issues.removeAll() },
            call: { _ in
                throw BayboError.Other(
                    message: "this issue is blocked — lift the block before running it again")
            }
        )
        #expect(!refused)
        #expect(store.boards["p1"]?.issues.map(\.title) == ["the parser"])
        #expect(
            store.writeError == "this issue is blocked — lift the block before running it again")
    }

    /// Offline disables writes rather than queueing them: a board moves while
    /// the phone is away, so replaying a write authored against a board that
    /// has since changed is worse than not having sent it.
    @Test func offlineRefusesTheWriteInsteadOfQueueingIt() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.failProjects = true
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        #expect(store.isOffline)

        var called = false
        let sent = await store.write(board: "p1", call: { _ in called = true })
        #expect(!sent)
        #expect(!called)
        #expect(store.writeError?.contains("Offline") == true)
    }

    /// Logout takes the boards with it — they belong to the departing gateway,
    /// and a mirror that outlived one is somebody else's account on screen.
    @Test func removingTheMirrorTakesEveryBoardWithIt() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(1, title: "one")]
        let first = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await first.refreshRoot()
        await first.refreshBoard("p1")

        ProjectsStore.removeMirror(in: dir.url)

        let second = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        #expect(second.projects.isEmpty)
        #expect(second.boards.isEmpty)
    }

    /// A project id reaches the filesystem, so it may not name a path — `..`
    /// would put a mirror wherever the app can write.
    @Test func aProjectIdCannotEscapeTheSupportDirectory() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssues = [issue(1, title: "one")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("../../escape")
        // The board is held in memory, but nothing was written outside.
        let written = (try? FileManager.default.contentsOfDirectory(atPath: dir.url.path)) ?? []
        #expect(!written.contains { $0.contains("escape") })
    }
}
