import SwiftUI
import UIKit

#if DEBUG

    enum AppStoreScreenshotData {
        static let launchArgument = "-baybo-appstore-data"
        static let boardLaunchArgument = "-baybo-appstore-board"
        static let boardProjectName = "rglide"
        static let avatarDirectoryName = "appstore-avatars"

        static var requested: Bool {
            ProcessInfo.processInfo.arguments.contains(launchArgument)
        }

        static var launchesBoard: Bool {
            requested && ProcessInfo.processInfo.arguments.contains(boardLaunchArgument)
        }

        static func avatar(for blobId: String) -> Image? {
            guard requested, blobId.hasPrefix("sha256:") else { return nil }
            let digest = blobId.dropFirst("sha256:".count).split(separator: ".").first
            guard let digest else { return nil }
            let url = ServerCache.rootDirectory()
                .appendingPathComponent(avatarDirectoryName, isDirectory: true)
                .appendingPathComponent(String(digest))
            guard let image = UIImage(contentsOfFile: url.path) else { return nil }
            return Image(uiImage: image)
        }
    }

#endif

@MainActor
/// App-wide avatar cache keyed by blob id: repeated drawings share one fetch,
/// while replacing an avatar naturally changes the key.
final class AgentAvatars: ObservableObject {
    static let shared = AgentAvatars()

    /// Decoded and ready to draw. `@Published` so a face that arrives after
    /// its row painted replaces the monogram in place.
    @Published private(set) var images: [String: Image] = [:]

    /// In flight, so a board with the same agent on twelve cards asks once.
    private var loading: Set<String> = []
    /// Remember broken blobs so a repaint does not retry them once per face.
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
            if let screenshotAvatar = AppStoreScreenshotData.avatar(for: blobId) {
                images[blobId] = screenshotAvatar
                return
            }
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
                // A full roster can arrive together; decode off the main actor.
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
                self.failed.insert(blobId)
                NSLog("baybo: agent avatar %@: %@", blobId, bayboErrorText(error))
            }
        }
    }

    #if DEBUG
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
