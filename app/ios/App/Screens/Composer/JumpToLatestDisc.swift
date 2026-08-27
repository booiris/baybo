import SwiftUI

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
