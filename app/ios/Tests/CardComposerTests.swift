import Foundation
import Testing
import UIKit

@testable import Baybo

/// The card's composer: its draft, and what a comment carries.
///
/// The strip, the spool and the uploads are `ComposerStagingTests`' — one
/// machine, tested once. What is card-specific is where its draft is filed,
/// what the comment wire takes, and whether a failed post is survivable.
@MainActor
struct CardComposerTests {
    /// The smallest thing that decodes as an image, drawn the way
    /// `ComposerStagingTests` draws its own.
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
        return IssueStore(projectId: "p1", number: 41, client: client, supportDirectory: dir.url)
    }

    /// **The trap this exists for.** `AppStore.unsentDraftSessionId` enumerates
    /// the chat drafts root and treats any unlisted, outbox-free directory as
    /// the abandoned new chat the compose button should resume. A card's
    /// comment draft filed beside them would open as a conversation.
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

    /// Logout takes both roots: a card draft left behind keeps a departing
    /// gateway's comment text, and its hard-linked bytes, for whoever binds
    /// next.
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

    /// The wire takes a blob id and a NAME. The gateway reads mime and size off
    /// the blob itself, but nothing there knows what the user picked the file
    /// as — drop the filename and every file card on the page prints an
    /// inferred one.
    @Test func aCommentCarriesItsPicksByIdAndName() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let card = store(dir, client: fake)

        let landed = await card.comment(
            "here you go",
            attachments: [
                IssueAttachmentInput(blobId: "sha256:a.tok", filename: "trace.txt"),
                IssueAttachmentInput(blobId: "sha256:b.tok", filename: nil),
            ])

        #expect(landed)
        #expect(fake.comments.count == 1)
        #expect(fake.comments.first?.text == "here you go")
        #expect(fake.comments.first?.attachments.map(\.blobId) == ["sha256:a.tok", "sha256:b.tok"])
        #expect(fake.comments.first?.attachments.first?.filename == "trace.txt")
    }

    /// A comment that did not land says so. There is no outbox on this
    /// surface, so the dock keeps its text and its tiles on a `false` — the
    /// picks are uploaded blobs, and discarding them strands files the
    /// operator cannot get back.
    @Test func aRefusedCommentReportsItRatherThanVanishing() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.failComments = true
        let card = store(dir, client: fake)

        let landed = await card.comment("this will not land")

        #expect(!landed)
        #expect(fake.comments.isEmpty)
        #expect(card.writeError != nil, "and the failure is reported, not swallowed")
    }

    /// The send gate is the machine's, so the card cannot ship a comment MINUS
    /// a pick that is still uploading — the silent failure the gate exists for.
    @Test func aPickStillUploadingHoldsTheSendAndSaysSo() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.stubIssueDetail = issue(41)
        // The upload is held open, so the pick stays mid-flight.
        fake.holdBlobUploads()
        let card = IssueStore(
            projectId: "p1", number: 41, client: fake, supportDirectory: dir.url,
            pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        card.staging.text = "with a file"
        card.staging.stagePasteboard()

        #expect(card.staging.claimSend() == nil)
        #expect(card.notice != nil, "the tile holding it up says why")
    }

    /// A face the page drew becomes a blob FIRST, and only then the agent's.
    ///
    /// The gateway stats the blob when the avatar is set and refuses a
    /// dangling reference, so the reverse order is not a slower way to do
    /// this — it is a 400.
    @Test func aGeneratedFaceIsStoredAsABlobBeforeTheAgentPointsAtIt() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        let card = store(dir, client: fake)
        let png = Data([0x89, 0x50, 0x4E, 0x47]).base64EncodedString()

        card.storeGeneratedFace(agentId: "a-dev", pngBase64: png)
        #expect(await waitUntil { !fake.avatarsSet.isEmpty })

        let upload = fake.blobUploadCalls.first
        #expect(upload?.mimeType == "image/png", "a native row cannot draw an SVG")
        #expect(fake.avatarsSet.first?.agentId == "a-dev")
        #expect(fake.avatarsSet.first?.blobId != nil)
    }

    /// Nobody asked for it, so a refusal costs nothing: the agent keeps the
    /// monogram it already had, and no banner appears over a card the
    /// operator did not touch.
    @Test func aRefusedFaceIsSilent() async {
        let dir = TempSupportDir()
        let fake = FakeBayboClient()
        fake.failProjects = true
        let card = store(dir, client: fake)

        card.storeGeneratedFace(
            agentId: "a-dev", pngBase64: Data([0x89]).base64EncodedString())
        #expect(await waitUntil { !fake.blobUploadCalls.isEmpty })
        // The PUT is what refuses; give its task a turn to land in.
        _ = await waitUntil { fake.avatarsSet.count == 1 }

        #expect(fake.avatarsSet.isEmpty, "a refused face sets nothing")
        #expect(card.writeError == nil, "and raises no banner over an untouched card")
    }

    /// The machine outlives the frame — an upload holds it — so the STORE'S
    /// DEATH must retire it, or a re-push builds a second one over the same
    /// draft key and the zombie's terminal write puts a sent draft back on
    /// disk.
    ///
    /// Driven by dropping the store rather than by calling the rule, because
    /// which moment fires it is the whole of what went wrong: it used to hang
    /// off `ProjectIssueScreen`'s `.onDisappear`, which SwiftUI fires when a
    /// push merely COVERS the card — so tapping a sub-issue retired a machine
    /// the reader was coming straight back to. A test that calls `leaveCard()`
    /// itself passes either way.
    @Test func theCardsDeathRetiresItsStagingMachine() async {
        let dir = TempSupportDir()
        var card: IssueStore? = store(dir, client: FakeBayboClient())
        // Deliberately the ONLY strong reference to the store, so `card = nil`
        // below really is the pop.
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

    /// The renderer deliberately outlives a card now; the STORE must not.
    /// `IssueBridge.store` is weak so a warm slot cannot keep every card it has
    /// rendered, along with its invalidation observer and staging machine.
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
