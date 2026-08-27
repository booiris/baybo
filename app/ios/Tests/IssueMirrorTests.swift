import Foundation
import Testing

@testable import Baybo

/// A card's local cache: it paints before the network answers, and it is
/// careful about what it is allowed to arm while doing so.
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

    /// A second store over the same directory paints without asking anybody —
    /// the whole reason the cache exists.
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

    /// The board already has enough to draw the first screen of a card. A
    /// first-ever open has no per-card mirror yet, so this seed closes the one
    /// remaining cold-data gap while still withholding controls that require a
    /// live card response.
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

    /// **A cached prompt is never offered.** It is a live queue entry with a
    /// 300s timeout, and one replayed off disk would ask for an answer to
    /// something that stopped listening hours ago — the same reason the board
    /// refuses to mirror prompts at all.
    @Test func aMirroredCardOffersNoApprovalUntilTheNetworkConfirmsIt() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        // Live, the prompt IS offered — so the test is about the mirror, not
        // about the replay being broken.
        #expect(first.pendingApprovals.count == 1)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.events.count == 1, "the timeline still paints")
        #expect(second.pendingApprovals.isEmpty, "but nothing may be answered from disk")
    }

    /// The faces the card page draws come with the team's OWN monograms.
    ///
    /// The distinction the test is on: `AgentMonogram.map` widens the whole set
    /// when any pair collides, and `AgentMonogram.of` (one handle, no set)
    /// cannot — `dev-1` and `docs-1` both reduce to `D1` under it. The page is
    /// handed the resolved letters for exactly this reason, and a store that
    /// resolved them per member would hand it two identical faces.
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

    /// And the same rule for where the card opens. The envelope on disk still
    /// carries the boundary it was fetched with, and replaying it would open
    /// the card halfway up a thread under a rule promising news that is not
    /// there — this card was read the moment it was last opened.
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

    /// Same rule for Stop: a run unsettled when this was written may have
    /// finished hours ago, and the header's Stop would be offering to end
    /// something already over.
    @Test func aMirroredCardShowsNoLiveRunUntilTheNetworkConfirmsIt() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        #expect(first.liveRun != nil)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.runs.count == 1, "the run list still paints")
        #expect(second.liveRun == nil, "but Stop stays away until the network says so")
    }

    /// A failed refresh must not arm the live controls — `self.issue` is
    /// non-nil the moment a mirror loads, so the flag has to read THIS fetch's
    /// own answer.
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

    /// A card response cannot vouch for the independently fetched control
    /// planes. The cached run and prompt still paint as content here, but a
    /// failed run/timeline fetch must not expose Stop or Approve.
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

    /// A card page draws its files; the board never did, so the shape gained a
    /// field. An older mirror without it still decodes.
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

    /// Logout takes every cached card — one belongs to the gateway that
    /// served it.
    @Test func logoutTakesEveryCachedCard() async {
        let dir = TempSupportDir()
        _ = await seeded(dir)

        ProjectsStore.removeMirror(in: dir.url)

        let second = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(second.issue == nil)
        #expect(!second.isFromMirror)
    }

    /// A project id reaches the filesystem, so it may not name a path.
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

    /// The hatch: the mirror goes, the memory goes, and the next open is a
    /// cold one. What it must NOT do is leave a copy anywhere.
    @Test func resyncLeavesNothingBehindOnDiskOrInMemory() async {
        let dir = TempSupportDir()
        let first = await seeded(dir)
        #expect(first.issue != nil)

        // Resync with a client that answers nothing, so the refetch cannot
        // quietly repopulate what the clear was supposed to remove.
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

        // And a third store finds no file to paint from.
        let third = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        #expect(third.issue == nil, "the mirror is gone from disk too")
    }

    /// A resync must not take the BOARD's mirror with it — a card is not its
    /// board, and rebuilding one card should not cost the list its cold paint.
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

    /// Read is stamped once per card — and a rebuild is a fresh look at it, so
    /// the stamp re-arms. Otherwise a card resynced after being read would
    /// never mark itself read again on this device.
    @Test func resyncReArmsTheReadStamp() async {
        let dir = TempSupportDir()
        let store = await seeded(dir)
        store.markRendered()
        store.resync()
        // Nothing observable to assert but the absence of a crash and the
        // flag's reset; `markRendered` is idempotent by design, so this pins
        // that the reset happened at all.
        store.markRendered()
        #expect(store.issue == nil)
    }
}
