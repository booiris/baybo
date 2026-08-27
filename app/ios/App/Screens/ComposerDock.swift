import SwiftUI

/// The chat's bottom dock as a LAYER: whatever rows the composer stack is made
/// of, plus the attach panel floating above them, plus the collapse an expanded
/// HTML preview asks for.
///
/// Extracted from `ChatScreen` for one reason — nothing in the app could test
/// it there. The chain lived inside `ChatScreen.body`, which needs a `ChatStore`
/// and a live `TranscriptHost` webview to instantiate, so the only tier that
/// could see it was XCUITest. That is how `50b4e33f` shipped a `.clipped()` on
/// this chain that erased `AttachMenuPanel` outright: the panel is drawn
/// ENTIRELY at negative y (`AttachMenuPanel.box`), so clipping to the dock's own
/// bounds deleted its paint while leaving its layout, its hit region and its
/// accessibility frame correct — every existing assertion, at both tiers,
/// stayed green while the `+` dimmed the screen and showed nothing.
///
/// Store-free and generic on purpose: `ComposerDockTests` renders THIS type,
/// with the real `AttachMenuPanel` inside it, through `ImageRenderer` and reads
/// the pixels. A stand-in view reproducing the chain would have been worse than
/// no test — it would stay green the next time the real screen diverged.
///
/// THE INVARIANT: this chain must never clip, and the collapse must not RESIZE
/// it either. Everything the dock shows that matters — the attach panel above
/// its top edge, `composerVeil`'s solid tail down over the home indicator, the
/// pill's ambient shadow — is drawn OUTSIDE these bounds by design, so a clip
/// deletes it. And the dock's own geometry is what native reports to the web
/// side as the transcript's bottom obstruction (`ComposerView`'s
/// `onGeometryChange` → `setComposerTop`), so collapsing the height MOVES that
/// edge: the thread reflowed when a preview took the screen and reflowed back
/// when it left. Nobody saw the first one — but the preview's edge-swipe
/// dismissal uncovers the thread WHILE it travels, and the second played in
/// full view. `opacity` alone hides it, without cutting anything and without
/// telling the transcript its floor moved.
struct ComposerDock<Content: View, Panel: View>: View {
    /// An expanded HTML preview owns the screen: the dock goes invisible, deaf
    /// and weightless, but stays MOUNTED — the composer's unsent text is its own
    /// `@State` and an `if` here would drop the user's draft.
    let collapsed: Bool
    @ViewBuilder let content: () -> Content
    /// The attach panel, or nothing. Presented HERE rather than in the screen's
    /// own stack because this is the only layer that composites over the dock's
    /// own rows and the jump disc.
    @ViewBuilder let panel: () -> Panel

    var body: some View {
        content()
            // The space the `+` reports its frame in and the space the panel is
            // laid out in, in one container — no `.global` round trip between
            // two of them, so the panel's paint and its hit region are the same
            // rectangle.
            .coordinateSpace(.named(AttachMenuPanel.dockSpace))
            .overlay(alignment: .topLeading) { panel() }
            .opacity(collapsed ? 0 : 1)
            .allowsHitTesting(!collapsed)
            .accessibilityHidden(collapsed)
            // NO `.clipped()` and NO collapsing `.frame(height:)` — see THE
            // INVARIANT above. Invisible, deaf and weightless, but the same
            // size, so the floor it reports to the transcript never moves.
    }
}
