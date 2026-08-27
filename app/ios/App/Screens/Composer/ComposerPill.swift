import SwiftUI

struct ComposerPill<Leading: View, Trailing: View>: View {
    @Binding var text: String
    /// The prompt drawn in an empty field. May be empty — the card's is — and
    /// then `accessibilityLabel` is what names the field.
    let placeholder: String
    let accessibilityLabel: String
    let fieldIdentifier: String
    let lineLimit: ClosedRange<Int>
    let focused: FocusState<Bool>.Binding
    @ViewBuilder let leading: () -> Leading
    @ViewBuilder let trailing: () -> Trailing

    /// How far outside the field a tap still counts as "focus the field".
    private static var hitSlop: CGFloat { 10 }

    var body: some View {
        HStack(alignment: .bottom, spacing: 4) {
            leading()

            TextField(placeholder, text: $text, axis: .vertical)
                .lineLimit(lineLimit)
                .font(.system(size: 17))
                .accessibilityIdentifier(fieldIdentifier)
                .accessibilityLabel(Text(verbatim: accessibilityLabel))
                .focused(focused)
                .padding(.vertical, 13)
                .background {
                    Color.clear
                        .padding(-Self.hitSlop)
                        .contentShape(Rectangle())
                        .onTapGesture { focused.wrappedValue = true }
                }

            trailing()
        }
        .frame(minHeight: 48)
        .glassSurface(tint: Theme.paper.opacity(0.25), in: .rect(cornerRadius: 24))
        .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
        .padding(.horizontal, focused.wrappedValue ? Self.focusedGutter : Self.restGutter)
        .animation(.easeOut(duration: 0.25), value: focused.wrappedValue)
    }

    private static var restGutter: CGFloat { 40 }
    private static var focusedGutter: CGFloat { 14 }
}

struct ComposerSendCircle: View {
    let systemName: String
    var glyphSize: CGFloat = 15
    /// Ink when it will do something, hairline grey when it will not.
    let filled: Bool

    var body: some View {
        Circle()
            .fill(filled ? Theme.ink : Theme.line)
            .frame(width: 36, height: 36)
            .overlay(
                Image(systemName: systemName)
                    .font(.system(size: glyphSize, weight: .semibold))
                    .foregroundStyle(Theme.paper)
                    .contentTransition(.symbolEffect(.replace))
            )
    }
}
