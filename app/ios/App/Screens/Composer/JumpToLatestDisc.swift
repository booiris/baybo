import SwiftUI

/// The way back to the newest thing on a page — the chat's thread and a card's
/// Activity share it exactly, so it is one view rather than two copies that
/// have to be kept the same size.
///
/// **It hangs off the composer as an OVERLAY, not as a row above it**
/// (2026-08-27). It sat in the dock's own stack until the attach panel started
/// colliding with it. The panel hangs off the DOCK CONTENT's top edge
/// (`AttachMenuPanel.box`) — which is what keeps it clear of the notice line,
/// the approval card and the staged strip — so a disc in that stack raised the
/// panel by the disc's whole height as well: press `+` with the disc up and
/// the panel jumped a disc clear of the `+` that opened it.
///
/// Out of the stack, the content's top edge is the composer's again, the panel
/// opens in the same place whether or not the disc is up, and the disc simply
/// passes UNDER it — the panel is an overlay on the dock's content, so
/// anything inside that content is behind it, taps included.
///
/// That last part is why this is safe now and was not before. The panel used
/// to be presented in the screen's own ZStack, BELOW the `.safeAreaInset` the
/// disc lives in, and the disc ate 44pt of the Files row: a pick that scrolled
/// the transcript instead of opening a picker. Covering the disc is only ever
/// correct from ABOVE it.
struct JumpToLatestDisc: View {
    let label: String
    let identifier: String
    let action: () -> Void

    /// The disc itself, and the air between it and the composer under it.
    static let size: CGFloat = 44
    static let gap: CGFloat = 12
    /// How far above the composer's top edge it floats. Computed rather than
    /// written out, so the gap under the disc cannot drift from the disc.
    static var lift: CGFloat { size + gap }
    /// Its arrival and departure. It used to be the DOCK's animation, which
    /// had to carry the panel's travel too; nothing travels now, so the beat
    /// belongs to the only thing that moves.
    static let beat: Animation = .easeOut(duration: 0.16)

    var body: some View {
        Button {
            Haptics.tap()
            action()
        } label: {
            Image(systemName: "arrow.down")
                .font(.system(size: 17, weight: .medium))
                .foregroundStyle(Theme.ink)
                .frame(width: Self.size, height: Self.size)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityIdentifier(identifier)
        .accessibilityLabel(Text(verbatim: label))
        .transition(.scale(scale: 0.7).combined(with: .opacity))
    }
}

extension View {
    /// Float the jump disc over this view's top edge, centred on it.
    ///
    /// Applied to the COMPOSER, never to the stack around it: the composer's
    /// top edge is what the attach panel and the web side both take as the
    /// dock's floor, and a disc that is part of that stack moves both.
    func jumpToLatestDisc(
        visible: Bool, label: String, identifier: String, action: @escaping () -> Void
    ) -> some View {
        overlay(alignment: .top) {
            if visible {
                JumpToLatestDisc(label: label, identifier: identifier, action: action)
                    .offset(y: -JumpToLatestDisc.lift)
            }
        }
        .animation(JumpToLatestDisc.beat, value: visible)
    }
}
