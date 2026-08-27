import SwiftUI
import UIKit

/// The uploaded picture for each agent that has one.
///
/// **One store for the whole app, not a fetch per drawing.** The same teammate
/// is drawn on every card it owns, in the board's face strip, in the assignee
/// picker, in the filter sheet and again on its own profile — and a per-view
/// fetch pulls the same bytes once per drawing. `app/web`'s `useTeamPortraits`
/// carries the same comment for the same reason.
///
/// Keyed by BLOB id, not agent id: replacing an agent's avatar mints a new
/// blob, so a stale picture cannot survive under the agent's key — and two
/// agents sharing one uploaded image cost one fetch rather than two.
@MainActor
final class AgentAvatars: ObservableObject {
    static let shared = AgentAvatars()

    /// Decoded and ready to draw. `@Published` so a face that arrives after
    /// its row painted replaces the monogram in place.
    @Published private(set) var images: [String: Image] = [:]

    /// In flight, so a board with the same agent on twelve cards asks once.
    private var loading: Set<String> = []
    /// Blobs that answered with something unusable. Kept so a broken avatar
    /// costs one round trip rather than one per repaint — a failed fetch that
    /// retried on every draw would hammer the leg for a picture that is not
    /// coming.
    private var failed: Set<String> = []

    private let clientProvider: () -> any BayboClientProtocol
    private lazy var client: any BayboClientProtocol = clientProvider()

    init(clientProvider: @escaping () -> any BayboClientProtocol = { Baybo.client }) {
        self.clientProvider = clientProvider
    }

    func image(for blobId: String?) -> Image? {
        guard let blobId, !blobId.isEmpty else { return nil }
        return images[blobId]
    }

    /// Fetch every avatar this roster names, once.
    ///
    /// Called from wherever a team arrives rather than from each face: a face
    /// knows its own blob id and nothing about the others, so a face-driven
    /// fetch is a fetch per face by construction.
    func load(team: [TeamMemberInfo]) {
        for member in team {
            load(blobId: member.avatarBlobId)
        }
    }

    func load(blobId: String?) {
        guard let blobId, !blobId.isEmpty else { return }
        guard images[blobId] == nil, !loading.contains(blobId), !failed.contains(blobId) else {
            return
        }
        #if DEBUG
            // `-baybo-demo-projects` has no gateway to fetch from, and an
            // avatar is the one thing on this screen that cannot be faked from
            // a record alone — so the demo draws its own.
            if let drawn = Self.demoAvatar(for: blobId) {
                images[blobId] = drawn
                return
            }
        #endif
        loading.insert(blobId)
        Task { [weak self] in
            guard let self else { return }
            defer { self.loading.remove(blobId) }
            do {
                let bytes = try await self.client.blobDownloadBytes(blobId: blobId, progress: nil)
                // Decoded off the main actor: an avatar is small, but a board
                // arriving with a dozen of them would decode a dozen images in
                // one hop and drop the frame the list is painting in.
                let decoded = await Task.detached(priority: .userInitiated) {
                    UIImage(data: bytes)
                }.value
                guard let decoded else {
                    // Bytes that are not an image are not going to become one.
                    self.failed.insert(blobId)
                    return
                }
                self.images[blobId] = Image(uiImage: decoded)
            } catch {
                // One unreachable blob costs that agent its upload, not the
                // board its faces — it falls back to the monogram like an
                // agent that never had one.
                self.failed.insert(blobId)
                NSLog("baybo: agent avatar %@: %@", blobId, bayboErrorText(error))
            }
        }
    }

    #if DEBUG
        /// Demo blob ids are `demo-avatar-<hex>`; the hex is the fill. A flat
        /// disc rather than a fake robot: what the screenshot has to prove is
        /// that the PICTURE path replaced the monogram, and a solid colour
        /// proves it more clearly than a drawing would.
        static let demoPrefix = "demo-avatar-"

        private static func demoAvatar(for blobId: String) -> Image? {
            guard blobId.hasPrefix(demoPrefix) else { return nil }
            let hex = String(blobId.dropFirst(demoPrefix.count))
            guard hex.count == 6, let value = Int(hex, radix: 16) else { return nil }
            let color = UIColor(
                red: CGFloat((value >> 16) & 0xFF) / 255,
                green: CGFloat((value >> 8) & 0xFF) / 255,
                blue: CGFloat(value & 0xFF) / 255,
                alpha: 1)
            let side = CGSize(width: 64, height: 64)
            let drawn = UIGraphicsImageRenderer(size: side).image { context in
                color.setFill()
                context.fill(CGRect(origin: .zero, size: side))
            }
            return Image(uiImage: drawn)
        }
    #endif

    /// Logout: the pictures belong to the departing gateway's agents, and a
    /// blob id means nothing under the next one.
    func reset() {
        images.removeAll()
        loading.removeAll()
        failed.removeAll()
    }
}
