import Foundation

/// Reference-counted temp ownership: the last holder unlinks the spool, so an
/// upload can outlive a removed tile without reading a deleted path.
final class SpoolFile: Sendable {
    let url: URL

    init(url: URL) {
        self.url = url
    }

    deinit {
        try? FileManager.default.removeItem(at: url)
    }
}
