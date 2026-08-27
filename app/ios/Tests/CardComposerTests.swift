import Foundation
import Testing
import UIKit

@testable import Baybo

@MainActor
struct CardComposerTests {
    private static func smallPNG() -> Data {
        let format = UIGraphicsImageRendererFormat.default()
        format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8), format: format)
            .pngData { ctx in
                UIColor(white: 0.5, alpha: 1).setFill()
                ctx.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
            }
    }

    private func issue(_ number: Int64) -> IssueInfo {
        IssueInfo(
            number: number, projectId: "p1", title: "the dial loop", description: "why",
            attachments: [], status: .inProgress, priority: .high, assignee: "a-dev",
            position: 3, pinned: false, branch: nil, blockedReason: nil, parent: nil,
            filedFrom: nil, stage: 0, subIssues: nil, unread: 0, lastRunFailed: false,
            approvalPending: false, openedByAgent: false, cancelledAtMs: nil, createdAtMs: 1,
            updatedAtMs: 2)
    }

    private func store(_ dir: TempSupportDir, client: FakeBayboClient) -> IssueStore {
        client.stubIssueDetail = issue(41)
        client.stubIssueEventsJson = #"{"items":[]}"#
        client.stubRunLog = IssueRunLog(
            runs: [], totalCostMicros: 0, totalInputTokens: 0, totalOutputTokens: 0)
        client.stubIssues = [issue(41)]
        return IssueStore(projectId: "p1", number: 41, client: client, supportDirectory: dir.url)
    }

    @Test func aCardsDraftIsFiledAwayFromTheConversations() async {
        let dir = TempSupportDir()
        let card = store(dir, client: FakeBayboClient())
        card.staging.text = "half a comment"
        card.staging.flushDraft()

        #expect(
            DraftStore.read(key: .card(project: "p1", number: 41), in: dir.url)?.text
                == "half a comment")
        #expect(
            DraftStore.sessionIds(in: dir.url).isEmpty,
            "a card draft must be invisible to the new-chat resume")
        #expect(DraftStore.read(key: .chat("p1#41"), in: dir.url) == nil)
    }

    @Test func logoutTakesTheCardDraftsToo() async {
        let dir = TempSupportDir()
        let card = store(dir, client: FakeBayboClient())
        card.staging.text = "half a comment"
        card.staging.flushDraft()
        DraftStore.write(Draft(text: "a chat", attachments: []), key: .chat("s-1"), in: dir.url)

        DraftStore.deleteAll(in: dir.url)

        #expect(DraftStore.read(key: .card(project: "p1", number: 41), in: dir.url) == nil)
        #expect(DraftStore.read(key: .chat("s-1"), in: dir.url) == nil)
    }

    @Test func aCommentCarriesItsPicksByIdAndName() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.holdComments()
        let card = store(dir, client: fake)
        await card.refresh()

        let clientMsgId = card.sendComment(
            "here you go",
            attachments: [
                AttachmentRef(
                    kind: .file, blobId: "sha256:a.tok", mimeType: "text/plain", size: 12,
                    filename: "trace.txt"),
                AttachmentRef(
                    kind: .file, blobId: "sha256:b.tok", mimeType: "application/json",
                    size: 18, filename: nil),
            ])

        #expect(await waitUntil { fake.parkedComments == 1 })
        #expect(card.pendingComments.first?.clientMsgId == clientMsgId)
        #expect(card.pendingComments.first?.state == .sending)
        #expect(fake.comments.count == 1)
        #expect(fake.comments.first?.clientMsgId == clientMsgId)
        #expect(fake.comments.first?.text == "here you go")
        #expect(fake.comments.first?.attachments.map(\.blobId) == ["sha256:a.tok", "sha256:b.tok"])
        #expect(fake.comments.first?.attachments.first?.filename == "trace.txt")

        fake.releaseComments()
        #expect(await waitUntil { card.pendingComments.isEmpty })
    }

    @Test func aRefusedCommentReportsItRatherThanVanishing() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.failComments = true
        let card = store(dir, client: fake)
        await card.refresh()

        let clientMsgId = card.sendComment("this will not land")
        #expect(await waitUntil { card.pendingComments.first?.state == .failed })

        #expect(card.pendingComments.first?.clientMsgId == clientMsgId)
        #expect(fake.comments.count == 1)
        #expect(card.writeError != nil, "and the failure is reported, not swallowed")

        fake.failComments = false
        card.retryComment(clientMsgId)

        #expect(await waitUntil { card.pendingComments.isEmpty })
        #expect(fake.comments.count == 2)
        #expect(fake.comments.allSatisfy { $0.clientMsgId == clientMsgId })
    }

    @Test func anUnconfirmedCommentSurvivesAStoreRebuild() async {
        let dir = TempSupportDir()
        let first = IssueCommentOutbox(
            projectId: "p1", number: 41, supportDirectory: dir.url)
        first.begin(
            clientMsgId: "0c928886-cef8-4449-9e16-913c601f9988",
            text: "still owed",
            attachments: [
                AttachmentRef(
                    kind: .image, blobId: "sha256:image.tok", mimeType: "image/png", size: 7,
                    filename: "plot.png")
            ],
            unblockAfterSend: true)

        let rebuilt = IssueCommentOutbox(
            projectId: "p1", number: 41, supportDirectory: dir.url)

        #expect(rebuilt.entries().first?.text == "still owed")
        #expect(rebuilt.entries().first?.attachments.first?.mimeType == "image/png")
        #expect(rebuilt.entries().first?.unblockAfterSend == true)
        #expect(rebuilt.entries().first?.state == .sending)
    }

    @Test func aPickStillUploadingHoldsTheSendAndSaysSo() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        fake.holdBlobUploads()
        let card = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url,
            pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        card.staging.text = "with a file"
        card.staging.stagePasteboard()

        #expect(card.staging.claimSend() == nil)
        #expect(card.notice != nil, "the tile holding it up says why")
    }

    @Test func aGeneratedFaceIsStoredAsABlobBeforeTheAgentPointsAtIt() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let card = store(dir, client: fake)
        let png = Data([0x89, 0x50, 0x4E, 0x47]).base64EncodedString()

        card.storeGeneratedFace(agentId: "a-dev", pngBase64: png)
        #expect(await waitUntil { !fake.generatedAvatarsSet.isEmpty })

        let upload = fake.blobUploadCalls.first
        #expect(upload?.mimeType == "image/png", "a native row cannot draw an SVG")
        #expect(fake.generatedAvatarsSet.first?.agentId == "a-dev")
        #expect(fake.generatedAvatarsSet.first?.blobId.isEmpty == false)
        #expect(fake.avatarsSet.isEmpty, "generated defaults never use the unconditional door")
    }

    @Test func aRefusedFaceIsSilent() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.failProjects = true
        let card = store(dir, client: fake)

        card.storeGeneratedFace(
            agentId: "a-dev", pngBase64: Data([0x89]).base64EncodedString())
        #expect(await waitUntil { !fake.blobUploadCalls.isEmpty })
        _ = await waitUntil { !fake.blobUploadCalls.isEmpty }

        #expect(fake.generatedAvatarsSet.isEmpty, "a refused face sets nothing")
        #expect(card.writeError == nil, "and raises no banner over an untouched card")
    }

    @Test func theCardsDeathRetiresItsStagingMachine() async {
        let dir = TempSupportDir()
        var card: IssueStore? = store(dir, client: FakeBayboClient())
        let machine = card!.staging
        machine.text = "half a comment"
        machine.flushDraft()

        card = nil
        machine.text = "written by a ghost"
        machine.flushDraft()

        #expect(
            DraftStore.read(key: .card(project: "p1", number: 41), in: dir.url)?.text
                == "half a comment",
            "a retired machine may not touch the draft again")
    }

    @Test func droppingTheCardReleasesItsStoreButKeepsTheWarmHost() async {
        let dir = TempSupportDir()
        weak var weakStore: IssueStore?
        weak var weakHost: IssueHost?
        let pool = IssueHostPool()
        var lease: IssueHostPool.Lease?
        autoreleasepool {
            let card = store(dir, client: FakeBayboClient())
            weakStore = card
            lease = pool.open(id: UUID(), store: card)
            weakHost = lease?.host
            _ = card.staging
        }

        #expect(weakStore == nil, "the card's store outlived the card")
        #expect(weakHost != nil, "the app's warm renderer died with one card")
        if let lease { pool.close(lease) }
        lease = nil
        pool.teardown()
        #expect(weakHost == nil, "tearing down the binding left a renderer alive")
    }
}
