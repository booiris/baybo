import Foundation
import Testing

@testable import Baybo

private actor WriteFailureGate {
    private var continuation: CheckedContinuation<Void, Never>?
    private var released = false

    func wait() async {
        guard !released else { return }
        await withCheckedContinuation { continuation = $0 }
    }

    func release() {
        released = true
        continuation?.resume()
        continuation = nil
    }
}

@MainActor
struct ProjectsStoreTests {
    private func project(_ id: String, name: String, archived: Bool = false) -> ProjectInfo {
        ProjectInfo(
            id: id, name: name, description: "", workdir: "/tmp/\(id)",
            dailyBudgetMicros: 5_000_000, dailyBudgetTokens: nil, maxParallelIssueRuns: 3,
            agentsMayMerge: false, archivedAtMs: archived ? 1 : nil, createdAtMs: 0,
            updatedAtMs: 0)
    }

    private func issue(
        _ number: Int64, title: String, unread: Int64 = 0, cancelled: Bool = false
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: title, description: "", attachments: [],
            status: .todo, priority: .high, assignee: "a-dev", position: number, pinned: false,
            branch: nil, blockedReason: nil, parent: nil, filedFrom: nil, stage: 0,
            subIssues: SubIssueProgress(done: 1, total: 3), unread: unread, lastRunFailed: false,
            approvalPending: true, openedByAgent: false, cancelledAtMs: cancelled ? 1 : nil,
            createdAtMs: 0, updatedAtMs: 0)
    }

    private func member(_ id: String, _ handle: String, lead: Bool = false) -> TeamMemberInfo {
        TeamMemberInfo(
            id: id, handle: handle, name: handle, description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: lead,
            hiredBy: nil, createdAtMs: 0)
    }

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
        #expect(second.boards["p1"]?.issues.first?.status == .todo)
        #expect(second.boards["p1"]?.issues.first?.priority == .high)
        #expect(second.boards["p1"]?.issues.first?.approvalPending == true)
        #expect(second.boards["p1"]?.issues.first?.subIssues?.total == 3)
    }

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

    /// A failed optimistic write may roll back only the revision it created,
    /// never a newer live refresh that completed while the request was suspended.
    @Test func aFailedOlderWriteDoesNotRestoreOverANewerRefresh() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssues = [issue(12, title: "old")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("p1")
        let gate = WriteFailureGate()

        let write = Task {
            await store.write(
                board: "p1",
                apply: { $0.issues.removeAll() },
                call: { _ in
                    await gate.wait()
                    throw BayboError.Other(message: "refused")
                })
        }
        while store.boards["p1"]?.issues.isEmpty != true { await Task.yield() }

        fake.stubIssues = [issue(12, title: "new from server")]
        await store.refreshBoard("p1")
        await gate.release()

        #expect(await write.value == false)
        #expect(store.boards["p1"]?.issues.map(\.title) == ["new from server"])
    }

    @Test func settingThePriorityChangesThePriorityAndNothingElse() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        fake.stubIssueDetail = issue(12, title: "the parser")
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        await store.refreshBoard("p1")

        let sent = await store.setPriority(board: "p1", issue: 12, to: .low)

        #expect(sent)
        #expect(store.boards["p1"]?.issues.first?.priority == .low)
        let patch = fake.patches.last
        #expect(patch?.0 == 12)
        #expect(patch?.1.priority == .low)
        #expect(patch?.1.assignee == .keep)
        #expect(patch?.1.blockedReason == .keep)
        #expect(patch?.1.pinned == nil)
        #expect(patch?.1.title == nil)
        #expect(patch?.1.description == nil)
        #expect(patch?.1.attachments == nil)
        #expect(patch?.1.stage == nil)
        #expect(patch?.1.parent == nil)
        #expect(patch?.1.cancelled == nil)
    }

    @Test func cancellingAndReopeningACardUsesOneSparsePatchEachWay() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let card = issue(12, title: "the parser")
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [card]
        fake.stubIssueDetail = card
        fake.stubRuns = [
            IssueRunInfo(
                number: 12, attempt: 1, agentId: "a-dev", status: .running,
                trigger: .promoted, sessionId: "s1", error: nil, createdAtMs: 1,
                startedAtMs: 2, settledAtMs: nil, costMicros: nil, inputTokens: nil,
                outputTokens: nil)
        ]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        await store.refreshBoard("p1")
        store.seedPrompts(
            board: "p1",
            [
                12: [
                    IssueApprovalPrompt(
                        callId: "c1", tool: "exec", summary: nil, askedBy: "dev-1",
                        askedAtMs: 1)
                ]
            ])

        #expect(await store.setCancelled(board: "p1", issue: 12, true))
        #expect(store.boards["p1"]?.issues.first?.cancelledAtMs != nil)
        #expect(store.boards["p1"]?.runs.isEmpty == true)
        #expect(store.approvalPrompts["p1"]?[12] == nil)
        let cancel = fake.patches.last?.1
        #expect(cancel?.cancelled == true)
        #expect(cancel?.title == nil)
        #expect(cancel?.description == nil)
        #expect(cancel?.attachments == nil)
        #expect(cancel?.priority == nil)
        #expect(cancel?.assignee == .keep)
        #expect(cancel?.blockedReason == .keep)
        #expect(cancel?.parent == nil)
        #expect(cancel?.stage == nil)
        #expect(cancel?.pinned == nil)

        #expect(await store.setCancelled(board: "p1", issue: 12, false))
        #expect(store.boards["p1"]?.issues.first?.cancelledAtMs == nil)
        #expect(fake.patches.last?.1.cancelled == false)
    }

    @Test func aRefusedCancelRestoresTheCardRunAndPrompt() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        fake.stubRuns = [
            IssueRunInfo(
                number: 12, attempt: 1, agentId: "a-dev", status: .running,
                trigger: .promoted, sessionId: "s1", error: nil, createdAtMs: 1,
                startedAtMs: 2, settledAtMs: nil, costMicros: nil, inputTokens: nil,
                outputTokens: nil)
        ]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("p1")
        store.seedPrompts(
            board: "p1",
            [
                12: [
                    IssueApprovalPrompt(
                        callId: "c1", tool: "exec", summary: nil, askedBy: "dev-1",
                        askedAtMs: 1)
                ]
            ])

        #expect(!(await store.setCancelled(board: "p1", issue: 12, true)))
        #expect(store.boards["p1"]?.issues.first?.cancelledAtMs == nil)
        #expect(store.boards["p1"]?.runs.map(\.number) == [12])
        #expect(store.approvalPrompts["p1"]?[12]?.map(\.callId) == ["c1"])
    }

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

    @Test func aProjectIdCannotEscapeTheSupportDirectory() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssues = [issue(1, title: "one")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("../../escape")
        let written = (try? FileManager.default.contentsOfDirectory(atPath: dir.url.path)) ?? []
        #expect(!written.contains { $0.contains("escape") })
    }

    @Test func answeringAPromptRetiresItAtOnce() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshRoot()
        await store.refreshBoard("p1")
        store.seedPrompts(
            board: "p1",
            [
                12: [
                    IssueApprovalPrompt(
                        callId: "c1", tool: "exec", summary: nil, askedBy: "dev-1", askedAtMs: 1),
                    IssueApprovalPrompt(
                        callId: "c2", tool: "exec", summary: nil, askedBy: "dev-1", askedAtMs: 2),
                ]
            ])

        _ = await store.resolveApproval(
            board: "p1", issue: 12, callId: "c1", decision: .approve)
        #expect(store.approvalPrompts["p1"]?[12]?.map(\.callId) == ["c2"])

        _ = await store.resolveApproval(
            board: "p1", issue: 12, callId: "c2", decision: .deny)
        #expect(store.approvalPrompts["p1"]?[12] == nil)
        #expect(fake.approvalsResolved.map(\.1) == ["c1", "c2"])
        #expect(fake.approvalsResolved.map(\.2) == [.approve, .deny])
    }

    @Test func aRefusedAnswerPutsTheRowBack() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser")]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("p1")
        store.seedPrompts(
            board: "p1",
            [
                12: [
                    IssueApprovalPrompt(
                        callId: "c1", tool: "exec", summary: nil, askedBy: "dev-1", askedAtMs: 1)
                ]
            ])

        fake.failProjects = true
        let answered = await store.resolveApproval(
            board: "p1", issue: 12, callId: "c1", decision: .approve)
        #expect(!answered)
        #expect(
            store.approvalPrompts["p1"]?[12]?.map(\.callId) == ["c1"],
            "a prompt still waiting must come back")
    }

    @Test func aRetryClearsTheFailedFlagWithoutWaitingForTheRoundTrip() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [project("p1", name: "rglide")]
        fake.stubIssues = [issue(12, title: "the parser").with(lastRunFailed: true)]
        let store = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await store.refreshBoard("p1")
        #expect(store.boards["p1"]?.issues.first?.lastRunFailed == true)

        _ = await store.retryRun(board: "p1", issue: 12)
        #expect(store.boards["p1"]?.issues.first?.lastRunFailed == false)
    }
}
