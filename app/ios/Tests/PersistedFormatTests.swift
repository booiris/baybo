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
}
