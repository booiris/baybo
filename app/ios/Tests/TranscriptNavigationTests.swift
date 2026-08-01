import Foundation
import Testing

@testable import Baybo

@Suite @MainActor
struct TranscriptNavigationTests {
    @Test
    func onlyTheTranscriptIndexCanNavigateTheMainFrame() throws {
        let index = try #require(
            URL(string: "baybo-transcript://localhost/index.html"))
        let preview = try #require(
            URL(
                string:
                    "baybo-transcript://localhost/html-preview/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.beef"
            ))

        #expect(TranscriptSchemeHandler.permitsNavigation(to: index, isMainFrame: true))
        #expect(!TranscriptSchemeHandler.permitsNavigation(to: preview, isMainFrame: true))
    }

    @Test
    func onlyHtmlPreviewRoutesCanNavigateASubframe() throws {
        let preview = try #require(
            URL(
                string:
                    "baybo-transcript://localhost/html-preview/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.beef"
            ))
        let bundleAsset = try #require(
            URL(string: "baybo-transcript://localhost/assets/main.js"))
        let wrongHost = try #require(
            URL(
                string:
                    "baybo-transcript://elsewhere/html-preview/sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.beef"
            ))
        let external = try #require(URL(string: "https://example.com/"))

        #expect(TranscriptSchemeHandler.permitsNavigation(to: preview, isMainFrame: false))
        #expect(!TranscriptSchemeHandler.permitsNavigation(to: bundleAsset, isMainFrame: false))
        #expect(!TranscriptSchemeHandler.permitsNavigation(to: wrongHost, isMainFrame: false))
        #expect(!TranscriptSchemeHandler.permitsNavigation(to: external, isMainFrame: false))
    }
}
