import SwiftUI

/// A not-yet-built home section (Agents / Projects): a centered line icon over the
/// section title and a quiet "coming soon" line. Monochrome, flat — a calm
/// placeholder until the real screen lands.
struct PlaceholderScreen: View {
    @ObservedObject private var lang = Lang.shared
    let icon: String
    let titleKey: String

    var body: some View {
        VStack(spacing: 14) {
            Spacer()
            Image(systemName: icon)
                .font(.system(size: 46, weight: .light))
                .foregroundStyle(Theme.inkSoft)
            Text(verbatim: lang.t(titleKey))
                .font(Theme.mono(17))
                .foregroundStyle(Theme.ink)
            Text(verbatim: lang.t("placeholder.comingSoon"))
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
            Spacer()
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}
