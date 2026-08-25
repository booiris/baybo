import Foundation

/// A photo pick's temp spool, owned by everything holding it: the file is
/// unlinked when the LAST reference goes.
///
/// That ownership is the pick's whole lifetime. The tiles die with the
/// composer — the back button on a strip that was never sent — and an upload
/// still streaming one keeps its own file alive to the end instead of reading
/// a path the ✕ has just unlinked. Deliberately unlike the digest-keyed
/// `baybo-preview` / `baybo-deck-share` caches, which are bounded by the number
/// of distinct blobs and hit again on re-open: this name is a UUID, so anything
/// left behind is dead weight nothing can ever reach — up to 100 MiB of it per
/// pick, ten picks to a strip.
final class SpoolFile: Sendable {
    let url: URL

    init(url: URL) {
        self.url = url
    }

    deinit {
        try? FileManager.default.removeItem(at: url)
    }
}
