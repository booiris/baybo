import Foundation
import Testing

@testable import Baybo

@Suite @MainActor
struct TranscriptMediaTests {
    private static let blobId = "sha256:avatar.tok"

    @Test func aCachedDisplayBlobNeverTouchesTheDownloadPath() async throws {
        let client = FakeBayboClient()
        let bytes = Data("cached avatar".utf8)
        client.cachedBlobs[Self.blobId] = bytes
        let sink = BlobSink()
        let media = TranscriptMedia(client: client)
        media.attach(sink)

        media.requestBlob(id: 7, blobId: Self.blobId)

        #expect(await waitUntil { sink.replies.count == 1 })
        let reply = try #require(sink.replies.first)
        #expect(reply.dataBase64 == bytes.base64EncodedString())
        #expect(client.blobCachedReadCalls == [Self.blobId])
        #expect(client.blobDownloadCalls.isEmpty)
    }

    @Test func aCacheMissFallsBackToTheDownloadPath() async throws {
        let client = FakeBayboClient()
        let bytes = Data("remote avatar".utf8)
        client.downloadableBlobs[Self.blobId] = bytes
        let sink = BlobSink()
        let media = TranscriptMedia(client: client)
        media.attach(sink)

        media.requestBlob(id: 8, blobId: Self.blobId)

        #expect(await waitUntil { sink.replies.count == 1 })
        let reply = try #require(sink.replies.first)
        #expect(reply.dataBase64 == bytes.base64EncodedString())
        #expect(client.blobCachedReadCalls == [Self.blobId])
        #expect(client.blobDownloadCalls == [Self.blobId])
    }
}

@MainActor
private final class BlobSink: WebMediaSink {
    struct Reply {
        let id: Int
        let dataBase64: String?
        let mimeType: String
        let error: String?
    }

    private(set) var replies: [Reply] = []

    func blobResult(id: Int, dataBase64: String?, mimeType: String, error: String?) {
        replies.append(
            Reply(id: id, dataBase64: dataBase64, mimeType: mimeType, error: error))
    }

    func fileState(
        blobId: String, state: String, loaded: UInt64?, total: UInt64?, error: String?
    ) {}

    func audioState(blobId: String, state: String, position: Double, duration: Double) {}

    func videoPoster(
        id: Int, dataBase64: String?, width: Int, height: Int, durationMs: Int, error: String?
    ) {}
}
