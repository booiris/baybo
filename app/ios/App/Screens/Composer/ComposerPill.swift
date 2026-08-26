import SwiftUI

/// The composer's input pill: a leading accessory, an autosizing field, a
/// trailing accessory, inside one glass capsule.
///
/// A SHELL WITH SLOTS, not a view with a mode. The two docks that use it
/// differ genuinely in what hangs off either end — the chat's control morphs
/// between send and stop off a running turn, the card's is a plain send beside
/// an unblock toggle — and a `variant: .chat / .card` parameter would put both
/// bodies in here behind a branch, which is the false dedup this codebase has
/// a rule against. What is shared is what is literally identical: the 48pt
/// floor, the 17pt field and its 13pt padding, the hit slop that makes the
/// whole capsule focus the field, the glass, and the shadow that carries the
/// boundary over blank paper.
///
/// **The gutters are here** (2026-08-26). They used to belong to each dock,
/// because only the chat's animated with focus — the card's sat still at a
/// gutter of its own, which made the same control two different widths one
/// push apart. Both surfaces now want the same shape, so the shape has one
/// home; the rows a dock stacks ABOVE the pill keep their own gutters, since
/// those genuinely differ.
struct ComposerPill<Leading: View, Trailing: View>: View {
    @Binding var text: String
    /// The prompt drawn in an empty field. May be empty — the card's is — and
    /// then `accessibilityLabel` is what names the field.
    let placeholder: String
    /// What a screen reader calls the field. A `TextField` takes its name from
    /// its prompt, so a field with no prompt has no name at all: something
    /// VoiceOver can land on and not say what it is.
    let accessibilityLabel: String
    /// The field's accessibility id. Each dock has its own — the UI smokes
    /// address them by it, and a shared one would make "the field" ambiguous
    /// on a screen that has both a card dock and a chat behind it.
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

            // 13pt vertical padding makes the single-line field exactly the
            // row's 48pt (17pt body ≈ 22pt line), so the cursor sits vertically
            // centered despite the .bottom stack alignment; extra lines still
            // grow upward.
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
        // Borderless pill (ChatGPT-style): over blank white the untinted glass
        // is nearly invisible, so a soft ambient shadow carries the boundary
        // instead of a hairline.
        .glassSurface(tint: Theme.paper.opacity(0.25), in: .rect(cornerRadius: 24))
        .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
        // At rest the pill holds a moderate width; focus stretches it out
        // toward the screen edges — a small gutter stays — on the keyboard's
        // beat.
        .padding(.horizontal, focused.wrappedValue ? Self.focusedGutter : Self.restGutter)
        .animation(.easeOut(duration: 0.25), value: focused.wrappedValue)
    }

    /// How far the pill sits from the screen edges, unfocused and focused.
    /// Computed rather than stored: this type is generic on its two accessory
    /// slots, and a generic type may hold no static storage.
    private static var restGutter: CGFloat { 40 }
    private static var focusedGutter: CGFloat { 14 }
}

/// The pill's round trailing control, as a LOOK: each dock wraps it in its own
/// `Button` with its own action, disabled rule and label.
///
/// Not a button itself, deliberately. The chat's is two controls in one glyph
/// (send when idle, stop while a turn runs, with the fill following a
/// different condition than the glyph), and folding that in here would mean a
/// parameter per state for the surface that does not have them.
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
