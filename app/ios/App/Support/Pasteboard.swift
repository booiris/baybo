import UIKit
import UniformTypeIdentifiers

/// One image taken off the clipboard, as staging takes it: the ENCODED bytes,
/// plus whatever flavour the clipboard called them.
///
/// Never a `UIImage`. The mime the gateway stores and the provider is handed is
/// sniffed from these bytes (`StagedAttachment.photoMime`), so decoding and
/// re-encoding would silently replace the user's HEIC or JPEG with a different
/// format — bigger, and under a mime that no longer describes what they copied.
struct PastedImage {
    let data: Data
    /// What the clipboard flavour claims. A HINT only: `photoMime` sniffs the
    /// bytes first and reaches for this just for bytes the sniff can't name.
    let mime: String?
}

/// The system clipboard, as the composer reads it.
///
/// A seam rather than a bare `UIPasteboard.general` call, for two reasons that
/// are the same reason — the board is process-global. Tests cannot write it
/// (swift-testing runs suites in parallel, which is why `TempSupportDir`
/// exists), and the split below is what keeps the PRESENCE probe away from the
/// byte pull:
/// * `imageItemIndices()` reads `types(forItemSet:)`, which Apple documents as
///   NOT notifying the user. It is therefore safe to call just to decide whether
///   the Paste row is worth offering.
/// * `image(at:)` pulls bytes. For content copied in another app that raises the
///   system "Allow Paste?" alert, and the read blocks the thread it is on until
///   the user answers — so it must never run on the main actor.
protocol PasteboardReading: Sendable {
    /// Which clipboard items declare an image flavour. Presence only, no bytes.
    func imageItemIndices() -> [Int]
    /// The image in ONE item, or `nil` if nothing in it decodes as one.
    func image(at index: Int) -> PastedImage?
}

/// The composer the SYSTEM's own Paste command routes an image to.
///
/// Long-pressing the composer field offers Paste whatever the clipboard holds —
/// iOS puts that item there, and it is there even for an empty board — but
/// SwiftUI's `TextField` only ever inserts text, so on an image it does nothing.
/// The only place to catch that is the responder chain: UIKit asks the first
/// responder for `canPerformAction(paste:)` and, when it says NO, keeps walking
/// up. `AppDelegate` is the chain's terminus, and it has no idea which
/// conversation is on screen — so the open chat registers its strip here.
///
/// **This catches image-ONLY clipboards, by construction and not by choice.**
/// When the board also carries text (or a URL — a rich-text range with an inline
/// image is the usual shape), the field answers YES, *it* becomes the target,
/// it inserts the text, and nothing above it is ever asked. An ancestor can
/// never outrank the first responder. Handling that case means owning the first
/// responder, i.e. replacing the field with a `UITextView` subclass — a
/// deliberate non-goal here; the `+` panel's Paste row is the affordance that
/// works for every clipboard shape.
@MainActor
final class ComposerPasteTarget {
    static let shared = ComposerPasteTarget()

    /// Weak: the strip belongs to the session registry, and a chat that is torn
    /// down while still registered must not be kept alive by this.
    private weak var staging: ComposerStaging?

    func attach(_ staging: ComposerStaging) {
        self.staging = staging
    }

    /// Only the registrant can clear the slot. SwiftUI does not promise that a
    /// leaving screen's `onDisappear` runs before the arriving screen's
    /// `onAppear`, and unconditionally clearing on the way out would then blank
    /// a target the next chat had already claimed.
    func detach(_ staging: ComposerStaging) {
        guard self.staging === staging else { return }
        self.staging = nil
    }

    /// Whether the chain should claim `paste:` at all. False with no composer on
    /// screen and false for a clipboard holding no image — which is what keeps
    /// this from swallowing an ordinary text paste that the field declined for
    /// some other reason.
    var canPaste: Bool { staging?.pasteReady == true }

    func paste() {
        // The system's own Paste command IS the user intent iOS looks for, so
        // the read that follows is authorised and silent.
        staging?.stagePasteboard(authorized: true)
    }
}

/// Which clipboard the app runs against. `SystemPasteboard` everywhere but a
/// headless UI run: the real board cannot be seeded from a UI test, and the
/// Paste row does not even appear unless one holds an image, so the whole
/// affordance would be undrivable without the swap.
enum Pasteboards {
    static func launch() -> any PasteboardReading {
        #if DEBUG
            if ProcessInfo.processInfo.arguments.contains(demoPasteArg) {
                return DemoPasteboard()
            }
        #endif
        return SystemPasteboard()
    }

    #if DEBUG
        static let demoPasteArg = "-baybo-demo-paste"
    #endif
}

#if DEBUG
    /// `-baybo-demo-paste`: a clipboard holding exactly one image. Stateless on
    /// purpose — a second tap on the Paste row reads it again and stages a second
    /// tile, which is what proves the row is not one-shot.
    struct DemoPasteboard: PasteboardReading {
        func imageItemIndices() -> [Int] { [0] }

        func image(at index: Int) -> PastedImage? {
            guard index == 0 else { return nil }
            let format = UIGraphicsImageRendererFormat.default()
            format.scale = 1
            let data = UIGraphicsImageRenderer(size: CGSize(width: 64, height: 64), format: format)
                .pngData { ctx in
                    UIColor(white: 0.55, alpha: 1).setFill()
                    ctx.fill(CGRect(x: 0, y: 0, width: 64, height: 64))
                }
            return PastedImage(data: data, mime: "image/png")
        }
    }
#endif

struct SystemPasteboard: PasteboardReading {
    /// Flavours whose bytes `StagedAttachment.sniffMime` already names, best
    /// first. Handing the ORIGINAL bytes over is the rule: it keeps the stored
    /// mime equal to what the user copied, and it never inflates a 12 MP HEIC
    /// into a far larger PNG.
    private static let encoded: [(type: UTType, mime: String)] = [
        (.png, "image/png"),
        (.jpeg, "image/jpeg"),
        (.heic, "image/heic"),
        (.heif, "image/heic"),
        (.gif, "image/gif"),
        (.webP, "image/webp"),
    ]

    func imageItemIndices() -> [Int] {
        guard let perItem = UIPasteboard.general.types(forItemSet: nil) else { return [] }
        return perItem.enumerated().compactMap { index, types in
            types.contains(where: Self.isImage) ? index : nil
        }
    }

    func image(at index: Int) -> PastedImage? {
        let board = UIPasteboard.general
        let items: IndexSet = [index]
        // Read the item's declared flavours first — that read is free — and then
        // ask only for ones it actually has. Every `data(forPasteboardType:)` is
        // a pasteboard ACCESS, so probing types blindly is extra work on the
        // path that can be sitting behind a modal alert.
        guard let declared = board.types(forItemSet: items)?.first else { return nil }
        for entry in Self.encoded where declared.contains(entry.type.identifier) {
            guard
                let data = board.data(forPasteboardType: entry.type.identifier, inItemSet: items)?
                    .first
            else { continue }
            return PastedImage(data: data, mime: entry.mime)
        }
        // Nothing the sniff can name. `public.tiff` is the flavour that really
        // turns up — a copy originating on macOS carries it, and paste is the
        // only path that can hand this app one. TIFF is in neither the blob
        // store's FROZEN extension table nor the provider's image whitelist, so
        // uploading those bytes as they are would store a plain file card and
        // reach the model as a placeholder. Decoding and re-encoding to PNG is
        // what keeps a pasted screenshot an image end to end — and it is
        // deliberately the fallback, because teaching the storage table about
        // `image/tiff` is a silent data migration rather than a fix.
        for type in declared where Self.isImage(type) {
            guard let data = board.data(forPasteboardType: type, inItemSet: items)?.first,
                let png = UIImage(data: data)?.pngData()
            else { continue }
            return PastedImage(data: png, mime: "image/png")
        }
        return nil
    }

    private static func isImage(_ identifier: String) -> Bool {
        UTType(identifier)?.conforms(to: .image) == true
    }
}
