import SwiftUI

struct LlmModelIdsField: View {
    @Binding var text: String
    var identifier: String
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(verbatim: lang.t("llm.manualModels")).font(Theme.mono(12))
            TextField("provider/model-id", text: $text, axis: .vertical)
                .lineLimit(3...8)
                .font(Theme.mono(14))
                .autocorrectionDisabled()
                .textInputAutocapitalization(.never)
                .padding(12)
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.radius).strokeBorder(Theme.lineStrong)
                )
                .accessibilityIdentifier(identifier)
            Text(verbatim: lang.t("llm.manualModelsHint")).font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
        }
        .foregroundStyle(Theme.ink)
        .padding(.horizontal, 20)
        .padding(.top, 14)
    }
}
