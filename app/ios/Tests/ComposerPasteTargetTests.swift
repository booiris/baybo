import Foundation
import Testing
import UIKit

@testable import Baybo

/// The responder-chain half of paste: what `AppDelegate` answers when UIKit asks
/// it whether it can paste, and where a paste it accepts ends up.
///
/// Both halves are load-bearing, and the ANSWER is the dangerous one.
/// `UIResponder.canPerformAction` returns true for any action the class merely
/// IMPLEMENTS, so an `AppDelegate` that overrides `paste(_:)` and forwards the
/// question to `super` claims every paste in the app — including the plain text
/// ones the composer field handles perfectly, which would then vanish into an
/// attachment strip instead of the draft. That is not a hypothetical: it is the
/// shape the first spike shipped, and nothing about it looks wrong at the call
/// site. These cases are what stands in the way.
///
/// What this tier CANNOT see: whether UIKit actually walks past the SwiftUI text
/// field to reach the delegate. That is a UIKit behaviour, measured once on a
/// throwaway spike (18.6); here the delegate is asked directly.
///
/// `.serialized` because `ComposerPasteTarget.shared` is exactly the kind of
/// process-global state swift-testing's PARALLEL suites cannot share — the same
/// reason `FakePasteboard` exists instead of writing the real board.
@Suite(.serialized) @MainActor
struct ComposerPasteTargetTests {
    private static func smallPNG() -> Data {
        let format = UIGraphicsImageRendererFormat.default()
        format.scale = 1
        return UIGraphicsImageRenderer(size: CGSize(width: 8, height: 8), format: format)
            .pngData { ctx in
                UIColor(white: 0.5, alpha: 1).setFill()
                ctx.fill(CGRect(x: 0, y: 0, width: 8, height: 8))
            }
    }

    private static let paste = #selector(UIResponder.paste(_:))

    /// No chat on screen: the delegate must decline, or a paste anywhere in the
    /// app (the pairing screen's token field, a rename dialog) would be answered
    /// by a composer that isn't there.
    @Test func anUnregisteredDelegateClaimsNothing() {
        let target = ComposerPasteTarget.shared
        let fixture = ComposerFixture(pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        target.detach(fixture.staging)

        #expect(!target.canPaste)
        #expect(!AppDelegate().canPerformAction(Self.paste, withSender: nil))
    }

    /// REGRESSION: a clipboard with no image is not ours, even with a composer on
    /// screen. The field declines an image paste; it also declines for its own
    /// reasons, and answering YES to all of those is how a text paste gets eaten.
    @Test func aClipboardWithoutAnImageIsNotClaimed() {
        let fixture = ComposerFixture(pasteboard: FakePasteboard([.text]))
        ComposerPasteTarget.shared.attach(fixture.staging)
        defer { ComposerPasteTarget.shared.detach(fixture.staging) }

        #expect(!ComposerPasteTarget.shared.canPaste)
        #expect(!AppDelegate().canPerformAction(Self.paste, withSender: nil))
    }

    /// REGRESSION: only `paste:` is ours. Everything else keeps going to `super`
    /// — an override that swallowed the whole selector space would take Copy,
    /// Select All and the keyboard shortcuts with it.
    @Test func onlyPasteIsIntercepted() {
        let fixture = ComposerFixture(pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        ComposerPasteTarget.shared.attach(fixture.staging)
        defer { ComposerPasteTarget.shared.detach(fixture.staging) }
        let delegate = AppDelegate()

        #expect(delegate.canPerformAction(Self.paste, withSender: nil))
        for other in [
            #selector(UIResponder.copy(_:)), #selector(UIResponder.cut(_:)),
            #selector(UIResponder.selectAll(_:)),
        ] {
            #expect(
                !delegate.canPerformAction(other, withSender: nil),
                "the delegate implements none of these and must not claim them")
        }
    }

    /// The accepted paste lands in the registered strip, through the same
    /// admission the `+` row uses (so the cap, the notice and the upload pump
    /// are the same ones).
    @Test func anAcceptedPasteStagesIntoTheRegisteredStrip() async throws {
        let fixture = ComposerFixture(pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        ComposerPasteTarget.shared.attach(fixture.staging)
        defer { ComposerPasteTarget.shared.detach(fixture.staging) }

        AppDelegate().paste(nil)

        #expect(fixture.staging.staged.count == 1)
        #expect(await waitUntil { fixture.client.blobUploadCalls.count == 1 })
        let call = try #require(fixture.client.blobUploadCalls.first)
        #expect(call.path.hasPrefix(StagedAttachment.spoolDirectory.path))
    }

    /// Leaving a chat takes its claim with it — but only its OWN. SwiftUI does
    /// not promise the departing screen's `onDisappear` runs before the arriving
    /// screen's `onAppear`, and a `detach` that cleared unconditionally would
    /// blank a target the next chat had already registered, leaving paste dead
    /// until the user backed out and came in again.
    @Test func onlyTheRegistrantCanClearTheSlot() {
        let leaving = ComposerFixture(pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        let arriving = ComposerFixture(pasteboard: FakePasteboard([.image(Self.smallPNG())]))

        ComposerPasteTarget.shared.attach(leaving.staging)
        ComposerPasteTarget.shared.attach(arriving.staging)
        ComposerPasteTarget.shared.detach(leaving.staging)

        #expect(ComposerPasteTarget.shared.canPaste)
        AppDelegate().paste(nil)
        #expect(arriving.staging.staged.count == 1)
        #expect(leaving.staging.staged.isEmpty)

        ComposerPasteTarget.shared.detach(arriving.staging)
        #expect(!ComposerPasteTarget.shared.canPaste)
    }

    /// The target holds the strip WEAKLY: the strip belongs to the session
    /// registry, and a chat evicted under memory pressure while still registered
    /// must not be kept alive by a global.
    @Test func theTargetDoesNotKeepAChatAlive() throws {
        var fixture: ComposerFixture? = ComposerFixture(
            pasteboard: FakePasteboard([.image(Self.smallPNG())]))
        ComposerPasteTarget.shared.attach(try #require(fixture).staging)
        #expect(ComposerPasteTarget.shared.canPaste)

        fixture = nil

        #expect(!ComposerPasteTarget.shared.canPaste)
    }
}
