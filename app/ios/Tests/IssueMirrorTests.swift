import Foundation
import Testing

@testable import Baybo

@MainActor
struct IssueMirrorTests {
    private func issue(
        _ number: Int64, title: String = "the dial loop",
        attachments: [IssueAttachmentInfo] = [], blocked: String? = nil,
        approvalPending: Bool = true
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: title, description: "why",
            attachments: attachments, status: .inProgress, priority: .high, assignee: "a-dev",
            position: 3, pinned: false, branch: "b", blockedReason: blocked, parent: nil,
            filedFrom: nil, stage: 0, subIssues: SubIssueProgress(done: 1, total: 2), unread: 2,
            lastRunFailed: false, approvalPending: approvalPending, openedByAgent: false,
            cancelledAtMs: nil, createdAtMs: 1, updatedAtMs: 2)
    }

    private func run(settled: Int64?) -> IssueRunInfo {
        IssueRunInfo(
            number: 41, attempt: 1, agentId: "a-dev", status: .running, trigger: .promoted,
            sessionId: "s1", error: nil, createdAtMs: 1, startedAtMs: 2, settledAtMs: settled,
            costMicros: nil, inputTokens: nil, outputTokens: nil)
    }

    private func member() -> TeamMemberInfo {
        TeamMemberInfo(
            id: "a-dev", handle: "dev-1", name: "dev-1", description: "", avatarBlobId: nil,
            framework: "baybo", llm: nil, model: nil, reasoningEffort: nil, lead: false,
            hiredBy: nil, createdAtMs: 0)
    }

    private func seeded(_ dir: TempSupportDir) async -> IssueStore {
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.stubIssueEventsJson = """
            {"items":[{"id":"e1","number":41,"actor":{"kind":"agent","id":"a-dev","handle":"dev-1"},
             "created_at_ms":1,"body":{"kind":"approval_requested","call_id":"c1","tool":"exec","summary":"cargo test"}}]}
            """
        fake.stubRunLog = IssueRunLog(
            runs: [run(settled: nil)], totalCostMicros: 0, totalInputTokens: 0,
            totalOutputTokens: 0)
        fake.stubTeam = [member()]
        fake.stubIssues = [issue(41)]
        let first = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await first.refresh()
        return first
    }

    @Test func aSecondStorePaintsTheCardBeforeAnyFetch() async {
        let dir = TempSupportDir()
        _ = await seeded(dir)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.issue?.title == "the dial loop")
        #expect(second.events.count == 1)
        #expect(second.team.first?.handle == "dev-1")
        #expect(second.isFromMirror)
    }

    @Test func aBoardSeedPaintsAFirstEverCardWithoutArmingLiveControls() {
        let dir = TempSupportDir()
        let store = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(),
            supportDirectory: dir.url,
            seed: IssueStore.Seed(
                issue: issue(41), runs: [run(settled: nil)], team: [member()],
                children: [issue(42, title: "child")]))

        #expect(store.issue?.title == "the dial loop")
        #expect(store.runs.count == 1)
        #expect(store.team.first?.handle == "dev-1")
        #expect(store.children.first?.title == "child")
        #expect(store.isFromMirror)
        #expect(store.liveRun == nil, "Stop must wait for the card's live response")
        #expect(store.pendingApprovals.isEmpty)
    }

    @Test func aBoardSeedDoesNotRefetchTheTeam() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.stubIssueEventsJson = "{\"items\":[]}"
        fake.stubRunLog = IssueRunLog(
            runs: [], totalCostMicros: 0, totalInputTokens: 0, totalOutputTokens: 0)
        fake.stubIssues = [issue(41)]
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url,
            seed: IssueStore.Seed(
                issue: issue(41), runs: [], team: [member()], children: []))

        await store.refresh()

        #expect(fake.projectTeamCalls == 0)
        #expect(store.team.first?.handle == "dev-1")
    }

    @Test func theLiveTimelineCanMarkReadBeforeTheOtherDetailPlanesFinish() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.issueDetailStallMs = 500
        fake.stubIssueEventsJson = """
            {"items":[{"id":"e1","number":41,"created_at_ms":1,
             "body":{"kind":"comment","text":"new"}}]}
            """
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url,
            seed: IssueStore.Seed(
                issue: issue(41), runs: [], team: [member()], children: []))

        let refresh = Task { await store.refresh() }
        #expect(await waitUntil { store.events.count == 1 })
        store.markRendered()

        #expect(await waitUntil { fake.issueReads == [41] })
        #expect(store.issue?.unread == 0)
        await refresh.value
    }

    @Test func aMirroredCardOffersNoApprovalUntilTheNetworkConfirmsIt() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        #expect(first.pendingApprovals.count == 1)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.events.count == 1, "the timeline still paints")
        #expect(second.pendingApprovals.isEmpty, "but nothing may be answered from disk")
    }

    @Test func theCardPageIsHandedTheTeamsOwnMonograms() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.stubTeam = [
            TeamMemberInfo(
                id: "a-dev", handle: "dev-1", name: "dev-1", description: "",
                avatarBlobId: "blob-7", framework: "baybo", llm: nil, model: nil,
                reasoningEffort: nil, lead: false, hiredBy: nil, createdAtMs: 0),
            TeamMemberInfo(
                id: "a-docs", handle: "docs-1", name: "docs-1", description: "",
                avatarBlobId: nil, framework: "baybo", llm: nil, model: nil,
                reasoningEffort: nil, lead: false, hiredBy: nil, createdAtMs: 0),
        ]
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await store.refresh()

        #expect(
            store.people["a-dev"] == IssuePerson(handle: "dev-1", avatar: "blob-7", monogram: "DE1"))
        #expect(
            store.people["a-docs"] == IssuePerson(handle: "docs-1", avatar: nil, monogram: "DO1"))
    }

    @Test func aMirroredCardOpensAtTheTopRatherThanAtAStaleBoundary() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.stubIssueEventsJson = """
            {"items":[{"id":"e1","number":41,"created_at_ms":1,"body":{"kind":"comment","text":"hi"}}],
             "first_unread":"e1"}
            """
        let first = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await first.refresh()
        #expect(first.firstUnread == "e1", "live, the card lands where the reading stopped")

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.events.count == 1, "the timeline still paints")
        #expect(second.firstUnread == nil, "but the boundary waits for the network")
    }

    @Test func aMirroredCardShowsNoLiveRunUntilTheNetworkConfirmsIt() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        #expect(first.liveRun != nil)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.runs.count == 1, "the run list still paints")
        #expect(second.liveRun == nil, "but Stop stays away until the network says so")
    }

    @Test func aRefreshThatFailedLeavesTheCardMarkedAsMirrored() async {
        let dir = TempSupportDir()
        _ = await seeded(dir)

        let offline = FakeBayboClient()
        offline.failProjects = true
        let second = IssueStore(
            projectId: "p1", number: 41, client: offline, supportDirectory: dir.url)
        #expect(second.isFromMirror)
        await second.refresh()
        #expect(second.isFromMirror, "a failed fetch confirms nothing")
        #expect(second.pendingApprovals.isEmpty)
        #expect(second.issue != nil, "and the cached content stays on screen")
    }

    @Test func oneSuccessfulPlaneDoesNotArmFailedMirroredControlPlanes() async {
        let dir = TempSupportDir()
        _ = await seeded(dir)

        let partial = FakeBayboClient()
        partial.stubIssueDetail = issue(41, title: "live card only")
        let store = IssueStore(
            projectId: "p1", number: 41, client: partial, supportDirectory: dir.url)
        await store.refresh()

        #expect(!store.isFromMirror, "the card itself did answer live")
        #expect(store.runs.count == 1, "cached content stays visible")
        #expect(store.events.count == 1)
        #expect(store.liveRun == nil, "but the failed run plane cannot arm Stop")
        #expect(store.pendingApprovals.isEmpty, "nor can the failed timeline arm Approve")
    }

    @Test func historicalPromptsStayHiddenWhenTheLiveCardHasNoApproval() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41, approvalPending: false)
        fake.stubIssueEventsJson = """
            {"items":[{"id":"e1","number":41,"actor":{"kind":"agent","id":"a-dev","handle":"dev-1"},
             "created_at_ms":1,"body":{"kind":"approval_requested","call_id":"dead","tool":"exec","summary":"old"}}]}
            """
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await store.refresh()

        #expect(store.events.count == 1, "history still renders")
        #expect(store.pendingApprovals.isEmpty, "live gate state says nobody is listening")
    }

    @Test func aGoneApprovalIsRetiredSoTheNextLivePromptIsReachable() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.stubIssueEventsJson = """
            {"items":[
              {"id":"e1","number":41,"actor":{"kind":"agent","id":"a-dev","handle":"dev-1"},"created_at_ms":1,
               "body":{"kind":"approval_requested","call_id":"gone","tool":"exec","summary":"old"}},
              {"id":"e2","number":41,"actor":{"kind":"agent","id":"a-dev","handle":"dev-1"},"created_at_ms":2,
               "body":{"kind":"approval_requested","call_id":"live","tool":"exec","summary":"new"}}
            ]}
            """
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await store.refresh()
        #expect(store.pendingApprovals.map(\.callId) == ["gone", "live"])

        fake.approvalResolveError = BayboError.Other(message: "404 approval call not found")
        store.resolveApproval(callId: "gone", decision: .deny)

        #expect(store.pendingApprovals.map(\.callId) == ["live"])
        #expect(await waitUntil { fake.approvalsResolved.count == 1 })
        #expect(await waitUntil { fake.issueDetailCalls >= 2 }, "a gone call triggers reconciliation")
        #expect(store.writeError == nil, "a dead historical call is retired, not raised as a write failure")
    }

    @Test func anOlderRefreshCannotOverwriteANewerOne() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41, title: "old")
        fake.issueDetailStallMs = 150
        let store = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)

        let older = Task { await store.refresh() }
        #expect(await waitUntil { fake.issueDetailCalls == 1 })
        fake.stubIssueDetail = issue(41, title: "new")
        fake.issueDetailStallMs = 0
        await store.refresh()
        await older.value

        #expect(store.issue?.title == "new")
    }

    @Test func attachmentsSurviveTheRoundTrip() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(
            41,
            attachments: [
                IssueAttachmentInfo(
                    blobId: "b1", mimeType: "image/png", size: 12, filename: "a.png")
            ])
        let first = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url)
        await first.refresh()

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.issue?.attachments.first?.blobId == "b1")
        #expect(second.issue?.attachments.first?.filename == "a.png")
    }

    @Test func logoutTakesEveryCachedCard() async {
        let dir = TempSupportDir()
        _ = await seeded(dir)

        ProjectsStore.removeMirror(in: dir.url)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.issue == nil)
        #expect(!second.isFromMirror)
    }

    @Test func aProjectIdCannotEscapeTheSupportDirectory() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        let store = IssueStore(
            projectId: "../../escape", number: 41, client: fake, supportDirectory: dir.url)
        await store.refresh()
        let written = (try? FileManager.default.contentsOfDirectory(atPath: dir.url.path)) ?? []
        #expect(!written.contains { $0.contains("escape") })
    }

    @Test func resyncLeavesNothingBehindOnDiskOrInMemory() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        #expect(first.issue != nil)

        let offline = FakeBayboClient()
        offline.failProjects = true
        let store = IssueStore(
            projectId: "p1", number: 41, client: offline, supportDirectory: dir.url)
        #expect(store.issue != nil, "it starts from the mirror")
        store.resync()

        #expect(store.issue == nil)
        #expect(store.events.isEmpty)
        #expect(store.runs.isEmpty)
        #expect(!store.isFromMirror, "nothing is mirrored, because nothing is here")
        #expect(store.pendingApprovals.isEmpty)

        let third = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(third.issue == nil, "the mirror is gone from disk too")
    }

    @Test func resyncLeavesTheBoardsOwnMirrorAlone() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubProjects = [
            ProjectInfo(
                id: "p1", name: "rglide", description: "", workdir: "/tmp/p1",
                dailyBudgetMicros: nil, dailyBudgetTokens: nil, maxParallelIssueRuns: 3,
                agentsMayMerge: false, archivedAtMs: nil, createdAtMs: 0, updatedAtMs: 0)
        ]
        fake.stubIssues = [issue(41)]
        let board = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        await board.refreshRoot()
        await board.refreshBoard("p1")

        let card = await seeded(dir)
        card.resync()

        let reopened = ProjectsStore(supportDirectory: dir.url, clientProvider: { fake })
        #expect(reopened.projects.map(\.name) == ["rglide"])
        #expect(reopened.boards["p1"]?.issues.count == 1)
    }

    @Test func resyncReArmsTheReadStamp() async {
        let dir = TempSupportDir()
        let store = await seeded(dir)
        store.markRendered()
        store.resync()
        store.markRendered()
        #expect(store.issue == nil)
    }
}
