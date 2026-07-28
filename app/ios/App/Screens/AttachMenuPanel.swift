import SwiftUI

/// The composer's attach panel — the two picker entries behind the inline `+`,
/// blooming UPWARD out of it. `ModelMenuPanel`'s sibling in every visual
/// respect (scrim, glass, transition grammar), hand-rolled for a reason of its
/// own: a stock `Menu` is a `UIMenu`, which dims the WHOLE screen, lifts its
/// anchor into a system layer and puts the bubble where it decides. This one
/// has to leave the composer pill exactly where it is, visible and live.
///
/// It is presented in TWO layers, and that split is the whole design:
/// * the panel — this view — is an `.overlay` on the DOCK's content, so it
///   stacks above everything the dock grows upward (the notice line, the
///   approval card, the staged strip) and above the jump-to-latest disc that
///   shares its stack. From inside `ChatScreen`'s ZStack it could clear none of
///   them: the dock's `.safeAreaInset` composites ABOVE that stack, so the
///   panel drew behind the strip and the disc took its taps.
/// * the scrim (`AttachMenuScrim`) stays in that ZStack, where it covers the
///   transcript and the header but NOT the dock — which is exactly what leaves
///   the pill un-dimmed and still tappable under an open panel.
///
/// Riding with the dock is also what makes the panel's hit region and its paint
/// the same rectangle: it is laid out in the space it is measured in
/// (`dockSpace`), with no `.global` round trip between two containers to drift
/// apart.
struct AttachMenuPanel: View {
    /// The `+`'s own frame in the DOCK's coordinate space (`dockSpace`), which
    /// is what gives the panel its column.
    let anchor: CGRect
    @Binding var isPresented: Bool
    let onPick: (AttachSource) -> Void

    /// The one width knob (`ModelMenuPanel.panelWidth`'s sibling):
    /// content-independent, so a long localized row truncates rather than
    /// stretching the panel.
    static let panelWidth: CGFloat = 200
    /// The air between the panel's bottom edge and the dock's top edge.
    static let anchorGap: CGFloat = 10

    /// One row, and the whole panel. Fixed rather than measured: the panel is
    /// positioned by an offset, which needs its height BEFORE it lays out, and
    /// every face in this chrome is a fixed size (`Theme.mono(14)`, `.system(
    /// size: 16)`) rather than a Dynamic Type style, so the rows cannot grow.
    static let rowHeight: CGFloat = 46
    static var panelHeight: CGFloat { rowHeight * CGFloat(AttachSource.allCases.count) }
    /// The panel's beat, shared by everything that raises or drops it: the `+`,
    /// a row, the scrim.
    static let fade: Animation = .easeOut(duration: 0.15)
    /// The dock content's own coordinate space. The `+` reports its frame in it
    /// and the panel is laid out in it, so where the panel is drawn and where
    /// it is touched cannot disagree.
    static let dockSpace = "baybo.chat.dock"

    var body: some View {
        let box = Self.box(anchor: anchor)
        return panel
            // `.offset`, NOT `.alignmentGuide`: a guide is only consulted by a
            // container that resolves that alignment, and `.overlay(alignment:)`
            // does not — it pins the child to the named corner and the guides
            // resolve as if never written, which parked the panel inside the
            // dock at x = 0 in every configuration. An offset also carries the
            // hit region with it, so touch cannot drift from paint.
            .offset(x: box.minX, y: box.minY)
            .transition(.scale(scale: 0.92, anchor: .bottomLeading).combined(with: .opacity))
    }

    /// The panel's box in the DOCK's own space — origin at the dock content's
    /// top-leading corner, so the dock's top edge is `y = 0` and the whole box
    /// is NEGATIVE: it floats above it.
    ///
    /// The `+` gives the panel its COLUMN; the dock's top edge gives it its
    /// FLOOR. Hanging the bottom edge off the `+` instead is what put the panel
    /// inside the dock: the dock grows UPWARD with the notice line, the approval
    /// card and the staged strip, and the `+` does not move up with it. Against
    /// a 4-tile strip that rule left 68 of the panel's 92pt behind the strip,
    /// with the Files row completely hidden and untappable; behind an approval
    /// card it left nothing visible at all.
    static func box(anchor: CGRect) -> CGRect {
        CGRect(
            x: max(0, anchor.minX),
            y: -(panelHeight + anchorGap),
            width: panelWidth,
            height: panelHeight)
    }

    private var panel: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(AttachSource.allCases, id: \.self) { source in
                row(source)
            }
        }
        .frame(width: Self.panelWidth)
        .glassSurface(
            in: RoundedRectangle(cornerRadius: Theme.radiusModal),
            fallback: .regularMaterial, fallbackShadow: true)
    }

    private func row(_ source: AttachSource) -> some View {
        Button {
            Haptics.tap()
            withAnimation(Self.fade) { isPresented = false }
            onPick(source)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: source.glyph)
                    .font(.system(size: 16, weight: .light))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 22)
                Text(verbatim: source.title)
                    .font(Theme.mono(14))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 16)
            .frame(height: Self.rowHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(Text(verbatim: source.title))
    }
}

/// The attach panel's other half, presented by `ChatScreen` inside the stack
/// the panel itself had to leave: the model panel's whisper of dim, doubling as
/// the outside-tap dismiss surface, and it must swallow scrolls so the
/// transcript can't move underneath.
///
/// It covers the transcript and the header — and NOT the dock, which the
/// `.safeAreaInset` composites above this stack. That is not an oversight to
/// tidy up: it is what leaves the pill un-dimmed and the `+`, the field and
/// send all live under an open panel, which is the whole reason a stock `Menu`
/// was rejected.
struct AttachMenuScrim: View {
    @Binding var isPresented: Bool

    var body: some View {
        Theme.panelScrim
            .ignoresSafeArea()
            .onTapGesture {
                withAnimation(AttachMenuPanel.fade) { isPresented = false }
            }
            .transition(.opacity)
    }
}

/// Which picker a panel row asks for. The panel reports the pick and nothing
/// else: the pickers stay modifiers on `ComposerView`, where their selection
/// cap reads the strip's free slots and their results feed `ComposerStaging`,
/// so no staging state comes up here.
enum AttachSource: CaseIterable {
    case photos
    case files

    @MainActor var title: String {
        switch self {
        case .photos: return Lang.shared.t("chat.attachPhotos")
        case .files: return Lang.shared.t("chat.attachFiles")
        }
    }

    var glyph: String {
        switch self {
        case .photos: return "photo.on.rectangle"
        case .files: return "folder"
        }
    }
}

/// The composer's attach affordance as `ChatScreen` holds it: whether the
/// panel is up, where the `+` it hangs off is, and which picker a row asked
/// for.
@MainActor
final class AttachMenu: ObservableObject {
    @Published var isOpen = false
    /// The row the panel picked, cleared by the picker that answers it.
    @Published var pick: AttachSource?
    /// The `+`'s frame in the DOCK's coordinate space (`AttachMenuPanel
    /// .dockSpace`), reported by `ComposerView` per layout tick. Deliberately
    /// not `.global`: the panel is laid out in that same space, and the two
    /// disagreed by ~14pt when the frames came from different containers —
    /// enough that the top of a row was scrim and the empty gap under the panel
    /// fired a picker.
    private(set) var anchor: CGRect = .zero

    /// Republish the anchor only while the panel is UP. The `+` moves on every
    /// tick of the focus and keyboard animations, and nothing is anchored to
    /// it the rest of the time — the same reason the transcript's own composer
    /// geometry (`TranscriptBridge.setComposerTop`) publishes nothing.
    func report(anchor next: CGRect) {
        guard next != anchor else { return }
        if isOpen { objectWillChange.send() }
        anchor = next
    }
}
