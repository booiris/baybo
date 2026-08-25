import Foundation
import UIKit

/// Putting a picture on an agent, from bytes in memory.
///
/// Two callers, one path: the profile sheet's picker, and the card page
/// uploading the generated face an agent was born without. Both hold DATA and
/// the ffi's upload takes a PATH — deliberately, so an encoded image never
/// crosses the FFI boundary — so both need the same spool-and-upload dance,
/// and doing it twice is how the two would come to disagree about the mime.
///
/// **Never `deck_blob_upload_file`.** That one stamps the blob `deck:<card>`,
/// and a deck purge would reclaim the bytes an agent's face points at. The
/// generic upload stamps the device, which nothing sweeps.
enum AgentFaceUpload {
    /// Store `data` as a blob and answer its id.
    ///
    /// PNG unless told otherwise: a native `UIImage` has no SVG decoder, so an
    /// `image/svg+xml` avatar passes the gateway's `image/*` check and then
    /// draws as nothing on every board row.
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
