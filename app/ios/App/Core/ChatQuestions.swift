import SwiftUI

struct PendingQuestion: Codable, Equatable, PendingChatPrompt {
    static let raisedKind = "question_asked"
    static let snapshotKey = "pending_questions"

    struct Question: Codable, Equatable {
        struct Option: Codable, Equatable {
            let label: String
            let description: String
        }
        let header: String
        let question: String
        let options: [Option]
        let multiSelect: Bool
    }

    let askId: String
    let toolCallId: String?
    let questions: [Question]

    static func from(frame: [String: Any]) -> Self? {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let data = try? JSONSerialization.data(withJSONObject: frame),
            let card = try? decoder.decode(Self.self, from: data),
            !card.askId.isEmpty, !card.questions.isEmpty
        else { return nil }
        return card
    }
}

struct QuestionReply: Encodable {
    struct Answer: Encodable, Equatable {
        let header: String
        var selected: [String] = []
        var other: String?
    }

    enum Kind: String, Encodable { case answered, cancelled }
    let kind: Kind
    let answers: [Answer]?

    static let cancelled = Self(kind: .cancelled, answers: nil)
}

struct QuestionPresentation: Encodable {
    struct Tab: Encodable {
        let title: String
        let answered: Bool
    }

    let askId: String
    let active: Int
    let tabs: [Tab]
    let question: PendingQuestion.Question
    let answer: QuestionReply.Answer
    let connected: Bool
    let submitting: Bool
    let canContinue: Bool
    let autoAdvance: Bool
}

struct QuestionAction: Decodable {
    enum Kind: String, Decodable { case tab, select, other, next, autoNext, submit, cancel }
    let askId: String
    let active: Int
    let action: Kind
    let index: Int?
}

@MainActor
final class QuestionDraft: ObservableObject {
    let card: PendingQuestion
    @Published private(set) var active = 0
    @Published private(set) var answers: [QuestionReply.Answer]
    private var advancesOnSelection = true

    init(_ card: PendingQuestion) {
        self.card = card
        answers = card.questions.map { .init(header: $0.header) }
    }

    var question: PendingQuestion.Question { card.questions[active] }
    var current: QuestionReply.Answer { answers[active] }

    func isAnswered(_ index: Int) -> Bool {
        let answer = answers[index]
        return !answer.selected.isEmpty
            || !(answer.other ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    var nextTab: Int? { active + 1 < answers.count ? active + 1 : nil }
    private var autoAdvance: Bool { advancesOnSelection && !question.multiSelect && nextTab != nil }
    private var allAnswered: Bool { answers.indices.allSatisfy { isAnswered($0) } }

    func canContinue(connected: Bool) -> Bool {
        if nextTab != nil { return isAnswered(active) }
        return connected && allAnswered
    }

    func presentation(connected: Bool, submitting: Bool) -> QuestionPresentation {
        .init(askId: card.askId, active: active,
              tabs: answers.indices.map { .init(title: answers[$0].header, answered: isAnswered($0)) },
              question: question, answer: current, connected: connected, submitting: submitting,
              canContinue: !submitting && canContinue(connected: connected), autoAdvance: autoAdvance)
    }

    func selectTab(_ index: Int) {
        guard index != active else { return }
        advancesOnSelection = false
        active = index
    }

    func advance() {
        if let nextTab { active = nextTab }
    }

    func advanceAfterSelection(at index: Int) {
        guard autoAdvance, question.options.indices.contains(index),
              current.selected == [question.options[index].label] else { return }
        advance()
    }

    func select(_ label: String) {
        if !question.multiSelect {
            answers[active].selected = [label]
            answers[active].other = nil
        } else if current.selected.contains(label) {
            answers[active].selected.removeAll { $0 == label }
        } else {
            answers[active].selected.append(label)
        }
    }

    func write(_ text: String, at index: Int) {
        answers[index].other = text
        if !card.questions[index].multiSelect
            && !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        {
            answers[index].selected = []
        }
    }

    var reply: QuestionReply? {
        guard allAnswered else { return nil }
        let trimmed = answers.map { answer in
            var answer = answer
            let text = (answer.other ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            answer.other = text.isEmpty ? nil : text
            return answer
        }
        return QuestionReply(kind: .answered, answers: trimmed)
    }
}
