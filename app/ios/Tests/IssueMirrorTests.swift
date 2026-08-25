import Foundation
import Testing

@testable import Baybo

/// A card's local cache: it paints before the network answers, and it is
/// careful about what it is allowed to arm while doing so.
@MainActor
struct IssueMirrorTests {
    private func issue(
        _ number: Int64, title: String = "the dial loop",
        attachments: [IssueAttachmentInfo] = [], blocked: String? = nil
    ) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: title, description: "why",
            attachments: attachments, status: .inProgress, priority: .high, assignee: "a-dev",
            position: 3, pinned: false, branch: "b", blockedReason: blocked, parent: nil,
            filedFrom: nil, stage: 0, subIssues: SubIssueProgress(done: 1, total: 2), unread: 2,
            lastRunFailed: false, approvalPending: true, openedByAgent: false,
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
}
