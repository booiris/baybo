import SwiftUI

struct IssueCardRow: View {
    let issue: IssueInfo
    let run: IssueRunInfo?
    /// The assignee's handle, resolved by the board (the card carries an id).
    let assigneeHandle: String?
    /// And its picture, resolved the same way. A row knows a handle and
    /// nothing about the roster; the board holds both.
    var assigneeAvatar: String? = nil
    let runnerHandle: String?
    var runnerAvatar: String? = nil
    let langCode: String

    @ObservedObject private var lang = Lang.shared
    /// Redrawn on a tick so the run's elapsed advances without a refetch.
    let now: Date

    init(
        issue: IssueInfo, run: IssueRunInfo?, assigneeHandle: String?,
        assigneeAvatar: String? = nil, runnerHandle: String?, runnerAvatar: String? = nil,
        langCode: String, now: Date = Date()
    ) {
        self.issue = issue
        self.run = run
        self.assigneeHandle = assigneeHandle
        self.assigneeAvatar = assigneeAvatar
        self.runnerHandle = runnerHandle
        self.runnerAvatar = runnerAvatar
        self.langCode = langCode
        self.now = now
    }

    private var isCancelled: Bool { issue.cancelledAtMs != nil }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            spine
            VStack(alignment: .leading, spacing: 5) {
                topLine
                title
                if !badges.isEmpty { badgeRow }
                footer
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(minHeight: 52)
        .padding(.vertical, 13)
        .opacity(isCancelled ? 0.5 : 1)
        .contentShape(Rectangle())
        .accessibilityElement(children: .combine)
        .accessibilityIdentifier("issue-row-\(issue.number)")
    }

    @ViewBuilder private var spine: some View {
        let color: Color? =
            switch issue.priority {
            case .urgent, .high: Theme.ink
            case .medium: Theme.lineStrong
            case .low, .none, .unknown: nil
            }
        RoundedRectangle(cornerRadius: 1.5, style: .continuous)
            .fill(color ?? .clear)
            .frame(width: 3)
            .frame(maxHeight: .infinity)
            .accessibilityHidden(true)
    }

    private var topLine: some View {
        HStack(spacing: 6) {
            Text(verbatim: "#\(issue.number)")
                .font(Theme.mono(11))
                .foregroundStyle(Theme.inkSoft)
            Text(verbatim: Self.priorityMark(issue.priority))
                .font(Theme.mono(10))
                .foregroundStyle(Theme.ink)
                .accessibilityLabel(Text(verbatim: Self.priorityWord(issue.priority)))
            if issue.pinned {
                Image(systemName: "pin.fill")
                    .font(.system(size: 9))
                    .foregroundStyle(Theme.inkSoft)
                    .padding(.horizontal, -Self.symbolBearing(9))
                    .accessibilityLabel(Text(verbatim: lang.t("board.pinned")))
            }
            Text(verbatim: RunLabels.compact(seconds: age))
                .font(Theme.mono(10.5))
                .foregroundStyle(Theme.inkSoft)
            if issue.approvalPending {
                // The same glyph the chat list uses for a parked approval, so
                // the two surfaces do not each invent a mark for one gate.
                Image(systemName: "hand.raised")
                    .font(.system(size: 10, weight: .medium))
                    .foregroundStyle(Theme.ink)
                    .padding(.horizontal, -Self.symbolBearing(10))
                    .accessibilityLabel(Text(verbatim: lang.t("board.awaitingApproval")))
            }
            Spacer(minLength: 6)
            if issue.unread > 0, !isCancelled {
                Text(verbatim: issue.unread > 99 ? "99+" : "\(issue.unread)")
                    .font(Theme.sys(10, weight: .medium))
                    .foregroundStyle(Theme.paper)
                    .padding(.horizontal, 5)
                    .frame(minWidth: 16, minHeight: 16)
                    .background(Theme.err, in: Capsule())
                    .accessibilityLabel(
                        Text(verbatim: lang.t("board.unread", "\(issue.unread)")))
            }
        }
    }

    private static func symbolBearing(_ pointSize: CGFloat) -> CGFloat {
        pointSize * 0.19
    }

    private var title: some View {
        Text(verbatim: issue.title)
            .font(Theme.sys(15, weight: .semibold))
            .foregroundStyle(Theme.ink)
            .strikethrough(isCancelled, color: Theme.inkSoft)
            .lineLimit(2)
            .multilineTextAlignment(.leading)
    }

    /// What is wrong with this card, at most a line of it.
    private var badges: [(glyph: String, text: String, isFailure: Bool)] {
        var out: [(String, String, Bool)] = []
        if issue.blockedReason != nil {
            out.append(("hand.raised.slash", lang.t("board.blocked"), false))
        }
        if issue.lastRunFailed {
            out.append(("xmark", lang.t("board.runFailed"), true))
        }
        if let branch = issue.branch, !branch.isEmpty {
            out.append(("arrow.branch", branch, false))
        }
        if let parent = issue.parent {
            out.append(("arrow.turn.down.right", "#\(parent)", false))
        }
        return out
    }

    private var badgeRow: some View {
        HStack(spacing: 6) {
            ForEach(badges, id: \.text) { badge in
                HStack(spacing: 3) {
                    Image(systemName: badge.glyph)
                        .font(.system(size: 8.5, weight: .medium))
                    Text(verbatim: badge.text)
                        .font(Theme.mono(10))
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                .foregroundStyle(badge.isFailure ? Theme.err : Theme.inkSoft)
                .padding(.horizontal, 6)
                .frame(height: 18)
                .overlay(
                    Capsule().strokeBorder(
                        badge.isFailure ? Theme.err.opacity(0.4) : Theme.lineStrong, lineWidth: 1))
            }
            Spacer(minLength: 0)
        }
    }

    private var footer: some View {
        HStack(spacing: 6) {
            if let assigneeHandle {
                AgentFace(handle: assigneeHandle, avatarBlobId: assigneeAvatar, size: 18)
                Text(verbatim: "@\(assigneeHandle)")
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
            } else {
                Text(verbatim: lang.t("board.unassigned"))
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
            }
            if let runnerHandle {
                AgentFace(
                    handle: runnerHandle, avatarBlobId: runnerAvatar, working: true, size: 18)
                    .accessibilityLabel(
                        Text(verbatim: lang.t("board.runningAs", runnerHandle)))
            }
            Spacer(minLength: 6)
            if let word = RunLabels.word(for: run) {
                HStack(spacing: 4) {
                    Text(verbatim: word)
                        .font(Theme.mono(9.5))
                        .kerning(0.8)
                    if let elapsed = RunLabels.elapsed(for: run, now: now) {
                        Text(verbatim: elapsed)
                            .font(Theme.mono(9.5))
                    }
                }
                .foregroundStyle(Theme.ink)
            }
            if let progress = issue.subIssues, progress.total > 0 {
                SubIssueRing(done: Int(progress.done), total: Int(progress.total))
                    .accessibilityLabel(
                        Text(
                            verbatim: lang.t(
                                "board.subIssues", "\(progress.done)", "\(progress.total)")))
            }
        }
    }

    private var age: Int {
        max(0, Int(now.timeIntervalSince1970 - Double(issue.updatedAtMs) / 1000))
    }

    /// Priority as a mark, never as a colour. Two triangles for urgent, one
    /// for high, a diamond for medium, an inverted triangle for low.
    static func priorityMark(_ priority: IssuePriority) -> String {
        switch priority {
        case .urgent: "▲▲"
        case .high: "▲"
        case .medium: "◆"
        case .low: "▽"
        // Unset and unrecognised both mean "nothing was said about
        // this one", and the row should not pretend to tell them apart.
        case .none, .unknown: "·"
        }
    }

    static func priorityWord(_ priority: IssuePriority) -> String {
        switch priority {
        case .urgent: "Urgent"
        case .high: "High"
        case .medium: "Medium"
        case .low: "Low"
        case .none: "No priority"
        case .unknown: "Unknown priority"
        }
    }
}

struct SubIssueRing: View {
    let done: Int
    let total: Int

    var body: some View {
        ZStack {
            Circle()
                .stroke(Theme.lineStrong, lineWidth: 2)
            Circle()
                .trim(from: 0, to: total > 0 ? min(1, Double(done) / Double(total)) : 0)
                .stroke(Theme.ink, style: StrokeStyle(lineWidth: 2, lineCap: .round))
                .rotationEffect(.degrees(-90))
        }
        .frame(width: 14, height: 14)
    }
}
