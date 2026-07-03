import SwiftUI

/// The in-app language override, ported from the web's i18next setup: EN/中
/// cycle, instant switching (no relaunch), persisted under the same conceptual
/// key (`baybo.lang`), defaulting to the device language. Strings resolve
/// through the chosen `.lproj` bundle instead of `String(localized:)` so a
/// toggle re-renders live.
@MainActor
final class Lang: ObservableObject {
    struct Language {
        let code: String
        let short: String
        let label: String
        /// The compiled String Catalog's lproj directory for this code.
        let lproj: String
    }

    /// Mirrors the web `SUPPORTED_LANGUAGES` (order defines the cycle).
    static let supported: [Language] = [
        Language(code: "en", short: "EN", label: "English", lproj: "en"),
        Language(code: "zh", short: "中", label: "简体中文", lproj: "zh-Hans"),
    ]

    static let shared = Lang()
    private static let defaultsKey = "baybo.lang"

    @Published private(set) var code: String
    private var bundle: Bundle

    private init() {
        let stored = UserDefaults.standard.string(forKey: Self.defaultsKey)
        let device = Locale.preferredLanguages.first ?? "en"
        let initial =
            stored
            ?? (device.hasPrefix("zh")
                ? "zh" : Self.supported.first { device.hasPrefix($0.code) }?.code ?? "en")
        let resolved = Self.supported.first { $0.code == initial } ?? Self.supported[0]
        code = resolved.code
        bundle = Self.bundle(for: resolved)
    }

    var current: Language {
        Self.supported.first { $0.code == code } ?? Self.supported[0]
    }

    /// The language a tap switches to (the cycle's next entry).
    var next: Language {
        let idx = Self.supported.firstIndex { $0.code == code } ?? 0
        return Self.supported[(idx + 1) % Self.supported.count]
    }

    func toggle() {
        let next = next
        code = next.code
        bundle = Self.bundle(for: next)
        UserDefaults.standard.set(next.code, forKey: Self.defaultsKey)
    }

    func t(_ key: String) -> String {
        bundle.localizedString(forKey: key, value: key, table: nil)
    }

    func t(_ key: String, _ arg: String) -> String {
        String(format: t(key), arg)
    }

    private static func bundle(for language: Language) -> Bundle {
        Bundle.main.path(forResource: language.lproj, ofType: "lproj")
            .flatMap(Bundle.init(path:)) ?? .main
    }
}

/// The web `.lang-switch`: a fixed top-right outline pill with a globe + the
/// current language's short label; tapping cycles to the next language.
struct LangSwitcher: View {
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        Button {
            lang.toggle()
        } label: {
            HStack(spacing: 5) {
                Image(systemName: "globe")
                    .font(.system(size: 15, weight: .light))
                Text(verbatim: lang.current.short)
                    .font(Theme.mono(13))
                    .kerning(0.5)
            }
            .foregroundStyle(Theme.ink)
            .padding(.horizontal, 11)
            .frame(minHeight: 34)
            .background(Theme.paper, in: Capsule())
            .overlay(Capsule().strokeBorder(Theme.line, lineWidth: 1))
        }
        .frame(minWidth: 44, minHeight: 44) // tap target floor
        .accessibilityLabel(
            Text(verbatim: "\(lang.t("lang.label")): \(lang.current.short) → \(lang.next.label)"))
    }
}
