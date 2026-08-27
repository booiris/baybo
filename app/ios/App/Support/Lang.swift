import SwiftUI

/// The in-app language override, ported from the web's i18next setup: EN/中
/// cycle, instant switching (no relaunch), persisted under the same conceptual
/// key (`baybo.lang`), defaulting to the device language. Strings resolve
/// through the chosen `.lproj` bundle instead of `String(localized:)` so a
/// toggle re-renders live.
@MainActor
final class Lang: ObservableObject {
    struct Language: Sendable {
        let code: String
        let short: String
        let label: String
        /// The compiled String Catalog's lproj directory for this code.
        let lproj: String
    }

    /// Mirrors the web `SUPPORTED_LANGUAGES` (order defines the cycle).
    ///
    /// `nonisolated`: an immutable table of the languages that exist, which
    /// `catalog(lproj:)` must read off the main actor.
    nonisolated static let supported: [Language] = [
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

    func t(_ key: String, _ args: String...) -> String {
        String(format: t(key), arguments: args)
    }

    /// The catalog for an EXPLICIT language, rather than the app's current one.
    ///
    /// For pure formatters that are told which language to speak (they already
    /// take one, to drive `Locale`-based date/list formatting) — so the sentence
    /// template and the OS-formatted parts inside it cannot end up in different
    /// languages, which is what "每Sun 9:00 AM" was. It also makes such a
    /// formatter testable: `t` reads the ambient setting, which on a test host is
    /// the MACHINE's language — English on a CI runner, whatever the developer
    /// has locally.
    /// `nonisolated`: resolving a catalog reads the app bundle and nothing this
    /// class owns, so a pure formatter can be told a language without also having
    /// to be on the main actor.
    nonisolated static func catalog(lproj: String) -> Bundle {
        bundle(for: supported.first { $0.lproj == lproj } ?? supported[0])
    }

    nonisolated private static func bundle(for language: Language) -> Bundle {
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
