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

    /// The machine outlives the frame — an upload holds it — so leaving must
    /// retire it, or a re-push builds a second one over the same draft key and
    /// the zombie's terminal write puts a sent draft back on disk.
    @Test func leavingTheCardRetiresItsStagingMachine() async {
        let dir = TempSupportDir()
        let card = store(dir, client: FakeBayboClient())
        let machine = card.staging
        card.staging.text = "half a comment"
        card.staging.flushDraft()

        card.leaveCard()
        machine.text = "written by a ghost"
        machine.flushDraft()

        #expect(
            DraftStore.read(key: .card(project: "p1", number: 41), in: dir.url)?.text
                == "half a comment",
            "a retired machine may not touch the draft again")
        #expect(card.staging !== machine, "and the next visit gets a fresh one")
    }
}
