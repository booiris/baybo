import SwiftUI

struct QuestionAnswerEditor: View {
    @ObservedObject var draft: QuestionDraft
    let index: Int
    let onChange: () -> Void
    let onDone: () -> Void
    @ObservedObject private var lang = Lang.shared
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 8) {
            TextField(lang.t("ask.other"), text: Binding(
                get: { draft.answers[index].other ?? "" },
                set: { draft.write($0, at: index); onChange() }
            ), axis: .vertical)
                .lineLimit(1...3)
                .focused($focused)
                .accessibilityIdentifier("question.other.field")
            Button(action: onDone) {
                Text(verbatim: lang.t("common.done"))
                    .frame(minWidth: 44, minHeight: 44)
            }
            .buttonStyle(.plain)
            .accessibilityIdentifier("question.other.done")
        }
        .font(Theme.promptBody)
        .foregroundStyle(Theme.ink)
        .padding(.horizontal, 12)
        .background(Theme.paper, in: RoundedRectangle(cornerRadius: 8))
        .overlay { RoundedRectangle(cornerRadius: 8).strokeBorder(Theme.lineStrong) }
        .padding(.horizontal, 14)
        .onAppear { focused = true }
        .onChange(of: focused) { _, focused in if !focused { onDone() } }
    }
}
