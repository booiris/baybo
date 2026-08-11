import Foundation
import Testing
import UIKit

@testable import Baybo

/// What a tapped chat image opens as — the election between the viewer's two
/// media, and the fact that it happens at all.
///
/// It ran on `UIImage(data:)` alone, which is nil for an SVG on every iOS there
/// is (no public API decodes one — ImageIO does not even carry the type), so a
/// tap on an agent-drawn diagram fell out of `viewImage`'s `guard` and did
/// nothing whatsoever: no viewer, no error, no log. A vector now takes its own
/// case and its own engine (`ZoomableVectorView`), which is also why the
/// election cannot be "did UIImage work" alone.
@Suite @MainActor
struct ImageViewerContentTests {
    private static let sessionId = "s-1"
    private static let blobId = "sha256:diagram.tok"

    /// The smallest legal PNG — a 1x1 that `UIImage` really decodes, so the
    /// raster branch is chosen for a reason and not by default.
    private static let onePixelPng = Data(
        base64Encoded: """
            iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmM\
            IQAAAABJRU5ErkJggg==
            """)!

    /// Deliberately a bare `viewBox`, the spelling an agent writes most often
    /// and the one with no pixel size anywhere in the bytes.
    private static let svg = Data(
        #"""
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1200 400">
          <rect width="100%" height="100%" fill="#d9d9d9"/>
        </svg>
        """#.utf8)

    private let temp: TempSupportDir
    private let client = FakeBayboClient()
    private let store: ChatStore

    init() {
        let temp = TempSupportDir()
        self.temp = temp
        let index = temp.makeIndex()
        store = ChatStore(
            sessionId: Self.sessionId, client: client, index: index,
            outbox: temp.makeOutbox(sessionId: Self.sessionId))
    }

    // MARK: - The election

    @Test func rasterBytesDecodeToAnImage() throws {
        let content = try #require(
            ViewedImage.Content(bytes: Self.onePixelPng, mimeType: "image/png"))
        guard case .raster = content else {
            Issue.record("a PNG must open as a decoded image")
            return
        }
    }

    @Test func svgBytesTakeTheVectorPath() throws {
        let content = try #require(
            ViewedImage.Content(bytes: Self.svg, mimeType: "image/svg+xml"))
        guard case .vector(let data) = content else {
            Issue.record("an SVG must open as vector art, not a rasterised copy")
            return
        }
        #expect(data == Self.svg, "the viewer renders the ORIGINAL bytes")
    }

    /// A bare mime is what the gateway sends today; the parameterised spelling
    /// is legal and names the same type.
    @Test func aParameterisedSvgMimeIsStillAnSvg() {
        #expect(
            ViewedImage.Content(bytes: Self.svg, mimeType: "image/SVG+XML; charset=utf-8") != nil)
    }

    /// Nothing to show is not the same as something to show blankly: a full
    /// screen of black with a close button is worse than the tap doing nothing.
    @Test func bytesThatAreNeitherOpenNothing() {
        #expect(ViewedImage.Content(bytes: Data("not an image".utf8), mimeType: "text/plain") == nil)
    }

    // MARK: - The tap

    @Test func tappingAnSvgPresentsTheViewer() async {
        client.cachedBlobs[Self.blobId] = Self.svg

        store.viewImage(blobId: Self.blobId, filename: "diagram.svg", mimeType: "image/svg+xml")

        #expect(await waitUntil { store.viewedImage != nil })
        #expect(store.viewedImage?.id == Self.blobId)
    }

    @Test func tappingARasterImagePresentsTheViewer() async {
        client.cachedBlobs[Self.blobId] = Self.onePixelPng

        store.viewImage(blobId: Self.blobId, filename: "shot.png", mimeType: "image/png")

        #expect(await waitUntil { store.viewedImage != nil })
    }

    /// A blob that never arrives must leave the screen alone rather than
    /// presenting an empty cover.
    @Test func anUnreachableBlobPresentsNothing() async {
        store.viewImage(blobId: Self.blobId, filename: "diagram.svg", mimeType: "image/svg+xml")

        #expect(await waitUntil(timeout: .milliseconds(250)) { store.viewedImage != nil } == false)
    }
}
