import Foundation
import Testing

@testable import Baybo

/// The on-disk formats, pinned as LITERAL TEXT.
///
/// `rowsPersistAcrossAReload` and `entriesAndTheirAttachmentsSurviveARelaunch`
/// write and read through the same `Codable`, so they pass even if every key
/// were renamed — and these two files are not caches the app can rebuild. A
/// renamed `SessionRow` key blanks the chat list of an existing install; a
/// renamed `OutboxEntry` key drops the sends a user made just before the
/// upgrade, silently, on the one launch that was supposed to deliver them.
/// `SessionRow` already carries a hand-written `init(from:)` whose whole job is
/// decoding an OLDER file — this is what proves it still does.
@Suite @MainActor
struct PersistedFormatTests {
    private static let sessionId = "s-1"
    /// The paths are spelled out, not read from the app's own constants: the
    /// filename and directory layout are as much a part of the contract with the
    /// installed base as the keys are.
    private static let indexFile = "sessions.json"
    private static let outboxDirectory = "outbox"
    private static let draftsDirectory = "drafts"

    private let temp = TempSupportDir()

    private func write(_ json: String, to relativePath: String) throws {
        let url = temp.url.appendingPathComponent(relativePath)
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data(json.utf8).write(to: url)
    }

    private func read(_ relativePath: String) throws -> String {
        try String(contentsOf: temp.url.appendingPathComponent(relativePath), encoding: .utf8)
    }

    /// The PRE-TELEGRAM schema, exactly as it sits in an installed app's
    /// container today: the user's last message under `lastUserText`, and no
    /// `preview` / `userText` / `title` / `archived` / `unread` at all. It must
    /// upgrade in place — a blank list on the launch after an update is the whole
    /// reason `SessionRow.init(from:)` is hand-written.
    @Test func aLegacySessionsJsonUpgradesInPlace() throws {
        try write(
            """
            [{"id":"\(Self.sessionId)","createdAt":"2026-07-01T00:00:00Z",
              "lastActive":"2026-07-10T12:00:00Z",
              "lastUserText":"what is the answer","pinned":true}]
            """, to: Self.indexFile)

        let row = try #require(temp.makeIndex().rows.first)
        #expect(row.id == Self.sessionId)
        #expect(row.preview == "what is the answer", "the legacy key still feeds the preview")
        #expect(row.userText == "what is the answer")
        #expect(row.pinned == true)
        #expect(row.archived == false)
        #expect(row.unread == 0)
        #expect(row.title == nil)
    }

    /// The CURRENT schema, byte for byte. Rename any key in `SessionRow` and this
    /// is what goes red — the reload round-trip never would.
    @Test func aCurrentSessionsJsonDecodesEveryField() throws {
        try write(
            """
            [{"id":"\(Self.sessionId)","createdAt":"2026-07-01T00:00:00Z",
              "lastActive":"2026-07-10T12:00:00Z","title":"A title",
              "preview":"the answer is 42","userText":"what is the answer",
              "pinned":true,"archived":true,"unread":7}]
            """, to: Self.indexFile)

        let row = try #require(temp.makeIndex().rows.first)
        #expect(row.title == "A title")
        #expect(row.preview == "the answer is 42")
        #expect(row.userText == "what is the answer")
        #expect(row.pinned == true)
        #expect(row.archived == true)
        #expect(row.unread == 7)
        // 2026-07-10T12:00:00Z — the ISO-8601 string really is being parsed as a
        // date, not silently defaulted.
        #expect(row.lastActive == Date(timeIntervalSince1970: 1_783_684_800))
    }

    /// And it is WRITTEN in that shape — the keys spelled as the installed base
    /// expects, dates as ISO-8601 strings (switch the strategy to epoch seconds
    /// and every existing file stops decoding, while a round-trip test sails on).
    @Test func sessionsJsonIsWrittenInTheSameShapeItReads() throws {
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: Self.sessionId, text: "what is the answer")
        index.setPinnedFlag(Self.sessionId, pinned: true)

        let json = try read(Self.indexFile)
        for key in ["\"id\"", "\"createdAt\"", "\"lastActive\"", "\"preview\"", "\"userText\"",
                    "\"pinned\"", "\"archived\"", "\"unread\""]
        {
            #expect(json.contains(key), "sessions.json lost the \(key) key")
        }
        #expect(
            json.contains("\"lastActive\":\"20") && json.contains("Z\""),
            "dates must stay ISO-8601 strings")
    }

    /// An outbox file written by the PREVIOUS build, holding a send that never
    /// got its echo. The upgrade's job is to transmit it — which starts with
    /// decoding it.
    @Test func anOutboxFileOnDiskDecodesWithItsAttachments() throws {
        try write(
            """
            [{"platformMsgId":"msg-1","text":"look at this","state":"sending",
              "transmissions":1,"createdAt":1000000,"lastSentAt":1000000,
              "attachments":[{"kind":"image","blobId":"sha256:abc.tok",
                              "mimeType":"image/png","size":12,"filename":"shot.png"}]}]
            """, to: "\(Self.outboxDirectory)/\(Self.sessionId).json")

        let entry = try #require(temp.makeOutbox(sessionId: Self.sessionId).entries().first)
        #expect(entry.platformMsgId == "msg-1")
        #expect(entry.text == "look at this")
        #expect(entry.state == .sending)
        #expect(entry.transmissions == 1)
        #expect(entry.lastSentAt == 1_000_000)
        #expect(entry.attachments.first?.blobId == "sha256:abc.tok")
        #expect(entry.attachments.first?.filename == "shot.png")
    }

    @Test func theOutboxIsWrittenInTheSameShapeItReads() throws {
        let outbox = temp.makeOutbox(sessionId: Self.sessionId)
        outbox.beginSend(
            platformMsgId: "msg-1", text: "look at this",
            attachments: [
                OutboxAttachment(
                    kind: "image", blobId: "sha256:abc.tok", mimeType: "image/png", size: 12,
                    filename: "shot.png")
            ])

        let json = try read("\(Self.outboxDirectory)/\(Self.sessionId).json")
        for key in ["\"platformMsgId\"", "\"text\"", "\"state\"", "\"transmissions\"",
                    "\"createdAt\"", "\"lastSentAt\"", "\"attachments\"", "\"blobId\"",
                    "\"mimeType\"", "\"filename\""]
        {
            #expect(json.contains(key), "the outbox file lost the \(key) key")
        }
        #expect(json.contains("\"sending\""), "the state is the enum's raw string")
    }

    // MARK: - the composer's unsent draft

    /// A draft is the one durable file here that the app CANNOT rebuild from the
    /// gateway — the gateway has never heard of it — so a renamed key or a moved
    /// directory silently throws away a message the user was in the middle of
    /// writing. The layout is spelled out for the same reason the other two are.
    @Test func aDraftOnDiskDecodesWithItsAttachments() throws {
        try write(
            """
            {"text":"look at this",
             "attachments":[{"id":"11111111-2222-3333-4444-555555555555","isImage":true,
                             "mime":"image/png","filename":"shot.png","byteCount":12,
                             "blobId":"sha256:abc.tok"},
                            {"id":"22222222-3333-4444-5555-666666666666","isImage":false,
                             "mime":"application/pdf","filename":"review.pdf","byteCount":34,
                             "bookmark":"Ym9va21hcms="}]}
            """, to: "\(Self.draftsDirectory)/\(Self.sessionId)/draft.json")

        let draft = try #require(DraftStore.read(sessionId: Self.sessionId, in: temp.url))
        #expect(draft.text == "look at this")
        let attachment = try #require(draft.attachments.first)
        #expect(attachment.id == "11111111-2222-3333-4444-555555555555")
        #expect(attachment.isImage)
        #expect(attachment.mime == "image/png")
        #expect(attachment.filename == "shot.png")
        #expect(attachment.byteCount == 12)
        #expect(attachment.blobId == "sha256:abc.tok")
        #expect(attachment.bookmark == nil, "an absent optional stays absent")
        // The Files half. A bookmark is the only handle to the USER's own
        // document — nothing else in the record can re-open it — and it rides
        // as base64, which is what `Data` decodes from.
        let scoped = try #require(draft.attachments.last)
        #expect(scoped.blobId == nil)
        #expect(scoped.bookmark == Data("bookmark".utf8))
    }

    @Test func aDraftIsWrittenInTheSameShapeItReads() throws {
        DraftStore.write(
            Draft(
                text: "look at this",
                attachments: [
                    DraftAttachment(
                        id: "11111111-2222-3333-4444-555555555555", isImage: true,
                        mime: "image/png", filename: "shot.png", byteCount: 12,
                        blobId: "sha256:abc.tok", bookmark: nil),
                    DraftAttachment(
                        id: "22222222-3333-4444-5555-666666666666", isImage: false,
                        mime: "application/pdf", filename: "review.pdf", byteCount: 34,
                        blobId: nil, bookmark: Data("bookmark".utf8)),
                ]),
            sessionId: Self.sessionId, in: temp.url)

        let json = try read("\(Self.draftsDirectory)/\(Self.sessionId)/draft.json")
        for key in ["\"text\"", "\"attachments\"", "\"id\"", "\"isImage\"", "\"mime\"",
                    "\"filename\"", "\"byteCount\"", "\"blobId\"", "\"bookmark\""]
        {
            #expect(json.contains(key), "the draft file lost the \(key) key")
        }
        #expect(json.contains("\"Ym9va21hcms=\""), "a bookmark is base64, as Data reads it")
    }
}
