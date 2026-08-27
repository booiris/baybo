import Foundation
import UIKit

/// Shared spool-and-upload path for generated and picked faces. Use the generic
/// device blob route: deck-owned blobs may be reclaimed by a deck purge.
enum AgentFaceUpload {
    static func put(
        _ data: Data, mime: String = "image/png", client: any BayboClientProtocol
    ) async throws -> String {
        let spool = FileManager.default.temporaryDirectory
            .appendingPathComponent("baybo-face-\(UUID().uuidString)")
        try data.write(to: spool, options: .atomic)
        defer { try? FileManager.default.removeItem(at: spool) }
        return try await client.blobUploadFile(
            path: spool.path, mimeType: mime, progress: nil)
    }
}
