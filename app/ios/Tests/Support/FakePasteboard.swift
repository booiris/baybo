import Foundation

@testable import Baybo

/// A clipboard the test owns, standing in for the process-global
/// `UIPasteboard.general` — which swift-testing's parallel suites cannot share
/// without one suite's paste surfacing as another's logic bug (the same
/// contamination `TempSupportDir` exists to prevent).
///
/// Items are modelled the way the real board is READ, and keeping those two
/// halves apart is the point: `declaresImage` is all the non-notifying presence
/// probe can ever know, and the bytes arrive separately. That lets a test pin
/// the shape the production reader has to survive — a flavour that is declared
/// and does not decode.
///
/// State sits behind a lock, not an actor: `image(at:)` is deliberately called
/// off the main actor (a real pull can raise the system "Allow Paste?" alert,
/// which blocks the reading thread until it is answered).
final class FakePasteboard: PasteboardReading, @unchecked Sendable {
    struct Item {
        /// Whether the item declares an image flavour.
        let declaresImage: Bool
        /// What pulling the item's bytes produces.
        let pulled: PastedImage?

        static func image(_ data: Data, mime: String? = "image/png") -> Item {
            Item(declaresImage: true, pulled: PastedImage(data: data, mime: mime))
        }

        /// Declared as an image, decodes as nothing — the board's nastiest real
        /// shape, and the only way to reach the "couldn't read that" notice.
        static let undecodable = Item(declaresImage: true, pulled: nil)
        /// Anything that isn't an image: a copied string, a URL.
        static let text = Item(declaresImage: false, pulled: nil)
    }

    private let lock = NSLock()
    private var items: [Item]
    private var reads: [Int] = []

    init(_ items: [Item] = []) {
        self.items = items
    }

    /// Which items the reader was actually asked for bytes from, in order. What
    /// proves the pull is per-item and sequential rather than one bulk read.
    var imageReads: [Int] {
        lock.withLock { reads }
    }

    /// Swap the board's contents mid-test — the race the Paste row has to
    /// survive is the user copying something else (or nothing) between opening
    /// the panel and tapping the row.
    func replace(with items: [Item]) {
        lock.withLock { self.items = items }
    }

    func imageItemIndices() -> [Int] {
        lock.withLock {
            items.enumerated().compactMap { index, item in item.declaresImage ? index : nil }
        }
    }

    func image(at index: Int) -> PastedImage? {
        lock.withLock {
            reads.append(index)
            guard items.indices.contains(index) else { return nil }
            return items[index].pulled
        }
    }
}
