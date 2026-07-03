import SwiftUI
import UIKit

/// The design tokens from the mobile style guide (`app/mobile/CLAUDE.md`):
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

    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        Font.custom("Space Mono", size: size).weight(weight)
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
            .overlay(Capsule().strokeBorder(color, lineWidth: 1))
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
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
