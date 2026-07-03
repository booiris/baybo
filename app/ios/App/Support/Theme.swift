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

/// Primary CTA: soft-filled ink pill, paper text, no shadow; press feedback is
/// a gentle dim + scale (the style guide's `opacity .7 + scale(.98)`).
struct InkPillButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(15, weight: .bold))
            .textCase(.uppercase)
            .kerning(2)
            .foregroundStyle(Theme.paper)
            .padding(.vertical, 14)
            .frame(maxWidth: .infinity)
            .background(Theme.ink, in: Capsule())
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}

/// Secondary CTA: ink outline pill.
struct OutlinePillButtonStyle: ButtonStyle {
    var color: Color = Theme.ink

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(15, weight: .bold))
            .textCase(.uppercase)
            .kerning(2)
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
