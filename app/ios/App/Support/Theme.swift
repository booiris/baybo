import SwiftUI
import UIKit

/// The design tokens from the style guide (`app/ios/docs/design-system.md`):
/// monochrome soft line minimalism, light-only, ink-on-paper. Flat surfaces,
/// 1px hairlines, pill corners, red reserved for destructive/error state.
///
/// Chrome type is Space Mono (bundled, `UIAppFonts`), matching the web bundle's
/// self-hosted face; `Font.custom` falls back to the system face if the TTFs
/// ever go missing.
enum Theme {
    static let paper = Color.white
    static let surface = Color(red: 0.98, green: 0.98, blue: 0.98)
    static let ink = Color(red: 0x11 / 255.0, green: 0x11 / 255.0, blue: 0x11 / 255.0)
    static let inkSoft = Color(red: 0x6B / 255.0, green: 0x6B / 255.0, blue: 0x6B / 255.0)
    static let line = Color(red: 0xE4 / 255.0, green: 0xE4 / 255.0, blue: 0xE4 / 255.0)
    static let err = Color(red: 0xD4 / 255.0, green: 0, blue: 0)

    static let radius: CGFloat = 14
    /// Floating layers only (the confirm dialog): in-plane inset boxes keep
    /// `radius`; a floating card scales its corners with elevation, continuous
    /// curvature, or it reads sharp next to iOS 26 chrome.
    static let radiusModal: CGFloat = 20

    /// The whisper of dim a hand-rolled panel (`ModelMenuPanel`,
    /// `AttachMenuPanel`) lays over what it floats above — enough to read as a
    /// layer, not enough to darken the screen. It doubles as the panel's
    /// outside-tap dismiss surface.
    static let panelScrim = Color.black.opacity(0.06)

    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        Font.custom("Space Mono", size: size).weight(weight)
    }

    /// The system face (SF Pro), for surfaces that deliberately step out of the
    /// Space Mono chrome — currently the chat list, which reads as a native,
    /// content-dense list rather than monospaced chrome.
    static func sys(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        Font.system(size: size, weight: weight)
    }
}

/// Primary CTA — the web `.cta`: soft-filled ink pill, paper text, REGULAR
/// weight, uppercase with 0.18em tracking (+ the matching lead-in that keeps
/// wide tracking optically centered, the CSS `text-indent` trick); press
/// feedback is a gentle dim + scale (`opacity .7 + scale(.98)`).
struct InkPillButtonStyle: ButtonStyle {
    /// 0.18em of the 15pt CTA face.
    private static let tracking: CGFloat = 15 * 0.18

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(15))
            .textCase(.uppercase)
            .kerning(Self.tracking)
            .padding(.leading, Self.tracking)
            .foregroundStyle(Theme.paper)
            .padding(.vertical, 14)
            .frame(maxWidth: .infinity)
            .background(Theme.ink, in: Capsule())
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}

/// Secondary CTA — the web `.cta-secondary`: ink outline pill with NO case
/// transform and NO tracking (the web class resets both on purpose — quiet
/// next to the primary).
struct OutlinePillButtonStyle: ButtonStyle {
    var color: Color = Theme.ink

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(15))
            .foregroundStyle(color)
            .padding(.vertical, 14)
            .frame(maxWidth: .infinity)
            // Stroke-only pill: without an explicit shape the transparent
            // interior doesn't hit-test, leaving only the text column tappable.
            .contentShape(Capsule())
            .overlay(Capsule().strokeBorder(color, lineWidth: 1))
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}

/// The web base `button`: filled ink pill, regular weight, no case transform —
/// what multi-button rows (pair confirm's Cancel | Pair) render as.
struct FilledPillButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(15))
            .foregroundStyle(Theme.paper)
            .padding(.vertical, 14)
            .padding(.horizontal, 19)
            .frame(maxWidth: .infinity)
            .background(Theme.ink, in: Capsule())
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}

/// The web `.link-btn`: a quiet borderless text button (the direct form's
/// "← Back").
struct LinkButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(13.5))
            .foregroundStyle(Theme.inkSoft)
            .padding(.horizontal, 13)
            .frame(minHeight: 44)
            // The 44pt target is the point: hit-test the padding, not the glyphs.
            .contentShape(Rectangle())
            .opacity(configuration.isPressed ? 0.6 : 1)
    }
}

extension View {
    /// Liquid Glass with a pre-26 fallback (deployment target is iOS 18): on
    /// 26+ the real `glassEffect`; below, the same shape filled with a
    /// material plus the tint as a wash over it. Deliberately strokeless —
    /// the composer pill is borderless by design (its ink shadow carries the
    /// boundary) and the glass circles read as bare frosted discs.
    /// `.interactive()`'s shimmer has no pre-26 equivalent and is dropped.
    /// The two `fallback*` knobs exist for panel-scale surfaces
    /// (`ModelMenuPanel`): `.ultraThinMaterial` lets a black bubble bleed
    /// through as a grey wash where real glass reads near-white, and real
    /// glass draws its own soft elevation shadow — both only visible at
    /// panel size, so the discs keep the bare defaults.
    @ViewBuilder
    func glassSurface<S: Shape>(
        tint: Color? = nil, interactive: Bool = false, in shape: S,
        fallback: Material = .ultraThinMaterial, fallbackShadow: Bool = false
    ) -> some View {
        if #available(iOS 26.0, *) {
            let base: Glass = tint.map { Glass.regular.tint($0) } ?? .regular
            glassEffect(interactive ? base.interactive() : base, in: shape)
        } else {
            background {
                shape.fill(fallback)
                    .overlay {
                        if let tint {
                            shape.fill(tint)
                        }
                    }
                    .shadow(
                        color: fallbackShadow ? Theme.ink.opacity(0.10) : .clear,
                        radius: 20, y: 8)
            }
        }
    }

    /// Web-page parity: tapping empty space blurs the focused field and drops
    /// the keyboard (a WKWebView resigns its input on outside taps; native
    /// SwiftUI does not). A plain tap gesture, so child buttons/fields keep
    /// winning their own taps.
    func tapToDismissKeyboard() -> some View {
        contentShape(Rectangle())
            .onTapGesture {
                UIApplication.shared.sendAction(
                    #selector(UIResponder.resignFirstResponder), to: nil, from: nil, for: nil)
            }
    }
}

enum Haptics {
    /// Light tap on primary-CTA presses (the style guide's physical beat).
    static func tap() {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    static func success() {
        UINotificationFeedbackGenerator().notificationOccurred(.success)
    }
}

/// An answer pill: shorter than the standard pills above, which size for a
/// full-page CTA and read bulky stacked inside a card. Both surfaces that ask
/// the operator to approve something use it — the transcript's approval card
/// and the board's Waiting strip — so the two answers are the same size and
/// shape wherever they are asked.
///
/// `fill: nil` draws the stroke-only variant. The 44pt floor is the HIG minimum
/// hit target and is the real lower bound here: the padding may shrink, the
/// tappable area may not — this is the one control in the app where a mis-tap
/// runs a command the user meant to refuse.
struct CompactPillButtonStyle: ButtonStyle {
    let fill: Color?
    let color: Color
    /// Full-width by default, which is what a dock card's pair wants. The
    /// board's Waiting strip sets it false: those two answers sit at the end
    /// of a row that already carries the question.
    var expands: Bool = true

    func makeBody(configuration: Configuration) -> some View {
        // The PAINTED capsule and the TAPPABLE area are deliberately different
        // sizes in the inline variant. Sizing the capsule itself to the 44pt
        // floor makes a pill that outweighs the row it sits in — "Deny" came
        // out a disc — so the capsule takes its natural height and the 44pt
        // frame goes around it. The target never shrinks; only the paint does.
        configuration.label
            .font(Theme.mono(expands ? 14 : 12))
            .foregroundStyle(color)
            .padding(.vertical, expands ? 8 : 6)
            .padding(.horizontal, expands ? 0 : 12)
            .frame(maxWidth: expands ? .infinity : nil, minHeight: expands ? 44 : 0)
            .background {
                if let fill {
                    Capsule().fill(fill)
                }
            }
            .overlay(fill == nil ? Capsule().strokeBorder(color, lineWidth: 1) : nil)
            .frame(minHeight: 44)
            // Stroke-only pills don't hit-test their interior without a shape,
            // and the inline variant's target extends past its capsule.
            .contentShape(Rectangle())
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}
