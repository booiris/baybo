import Foundation
import Testing

@testable import Baybo

@Suite @MainActor
struct ServerCacheTests {
    @Test func serverKeysOwnDifferentDirectories() {
        let temp = TempSupportDir()
        let first = ServerCache.supportDirectory(
            for: "gateway-" + String(repeating: "1", count: 64),
            in: temp.url)
        let second = ServerCache.supportDirectory(
            for: "gateway-" + String(repeating: "2", count: 64),
            in: temp.url)

        #expect(first != second)
        #expect(first.lastPathComponent.hasPrefix("gateway-"))
        #expect(first.deletingLastPathComponent().lastPathComponent == "servers")
    }

    @Test func unloadKeepsRowsDraftsAndTranscriptsForARebind() {
        let temp = TempSupportDir()
        let index = temp.makeIndex()
        index.recordUserSend(sessionId: "s-1", text: "hello")
        index.beginArchive("s-1", archived: true)
        DraftStore.write(
            Draft(text: "unfinished", attachments: []), key: .chat("s-1"), in: temp.url)
        TranscriptStore.write(sessionId: "s-1", stateJson: "{}", in: temp.url)

        index.unload()
        #expect(index.rows.isEmpty)
        #expect(DraftStore.read(key: .chat("s-1"), in: temp.url)?.text == "unfinished")
        #expect(TranscriptStore.read(sessionId: "s-1", in: temp.url) == "{}")

        index.activate(supportDirectory: temp.url)
        #expect(index.rows.map(\.id) == ["s-1"])
        #expect(index.rows.first?.archived == true)
        #expect(index.pendingMutation(for: "s-1") == .archived(true))
    }
}
