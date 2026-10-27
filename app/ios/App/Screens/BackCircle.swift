import SwiftUI

/// The glass back circle every pushed screen's header carries — in the bar's
/// leading slot, or first in the side rail. Semibold glyph so the ink reads as
/// solid black over the bright glass.
struct BackCircle: View {
    let label: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: "chevron.left")
                .font(.system(size: 18, weight: .semibold))
                .foregroundStyle(Theme.ink)
                .frame(width: 42, height: 42)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityLabel(Text(verbatim: label))
    }
}
