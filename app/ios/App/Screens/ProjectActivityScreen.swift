import SwiftUI

/// A board's activity feed: what happened, most recent first.
///
/// Shaped like a card's timeline because most lines are one — but `number` is
/// optional here, since joining the team is a fact about the BOARD and has no
/// card to point at. A line naming a card opens it; one that names none does
/// nothing, rather than opening something arbitrary.
struct ProjectActivityScreen: View {
    let projectId: String
    var client: any BayboClientProtocol = Baybo.client

    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss
    @State private var entries: [FeedEntry] = []
    @State private var loaded = false
    @State private var failed = false

    var body: some View {
        VStack(spacing: 0) {
            titleRow
            Rectangle().fill(Theme.lineStrong).frame(height: 1)
            content
        }
        .background(Theme.paper)
        .task { await load() }
    }

    private var titleRow: some View {
        ZStack {
            Text(verbatim: lang.t("activity.title"))
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
            HStack {
                Spacer()
                Button(lang.t("common.done")) { dismiss() }
                    .buttonStyle(LinkButtonStyle())
            }
        }
        .padding(.horizontal, 16)
        .frame(height: 52)
    }

    @ViewBuilder private var content: some View {
        if !loaded {
            ProgressView().tint(Theme.inkSoft).frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if failed {
            message(lang.t("activity.failed"))
        } else if entries.isEmpty {
            message(lang.t("activity.empty"))
        } else {
            List(entries) { entry in
                row(entry)
                    .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
                    .listRowBackground(Theme.paper)
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
        }
    }

    private func message(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.sys(13))
            .foregroundStyle(Theme.inkSoft)
            .multilineTextAlignment(.center)
            .padding(30)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    @ViewBuilder private func row(_ entry: FeedEntry) -> some View {
        let body = VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text(verbatim: entry.who ?? lang.t("activity.system"))
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                if let number = entry.number {
                    Text(verbatim: "#\(number)")
                        .font(Theme.mono(10.5))
                        .foregroundStyle(Theme.inkSoft)
                }
                Spacer(minLength: 6)
                Text(verbatim: RunLabels.compact(seconds: entry.ageSeconds))
                    .font(Theme.mono(10))
                    .foregroundStyle(Theme.inkSoft)
            }
            Text(verbatim: entry.line)
                .font(Theme.sys(13.5))
                .foregroundStyle(Theme.ink)
                .multilineTextAlignment(.leading)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 10)
        .contentShape(Rectangle())

        if let number = entry.number {
            Button {
                Haptics.tap()
                dismiss()
                appStore.openProjectIssue(project: projectId, number: number)
            } label: { body }
            .buttonStyle(.plain)
            .accessibilityIdentifier("activity-row-\(number)")
        } else {
            body
        }
    }

    private func load() async {
        do {
            let json = try await client.projectFeed(projectId: projectId, beforeMs: nil, limit: 100)
            entries = FeedEntry.decodeList(json)
            failed = false
        } catch {
            failed = true
        }
        loaded = true
    }
}

/// One line of a board's feed.
///
/// Decoded leniently and shallowly, exactly as `IssueEvent` is and for the
/// same reason: the gateway adds kinds on its own schedule, and a decoder that
/// threw on one would take the whole feed with it. An unrecognised kind prints
/// as its own name — more useful than a blank row, and infinitely more useful
/// than a screen that fails to open.
struct FeedEntry: Identifiable {
    let id: String
    let number: Int64?
    let who: String?
    let kind: String
    let createdAtMs: Int64

    var line: String { kind.replacingOccurrences(of: "_", with: " ") }

    var ageSeconds: Int {
        max(0, Int(Date().timeIntervalSince1970 - Double(createdAtMs) / 1000))
    }

    static func decodeList(_ json: String) -> [FeedEntry] {
        guard let data = json.data(using: .utf8),
            let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
            let items = root["items"] as? [[String: Any]]
        else { return [] }
        return items.enumerated().compactMap { index, item in
            guard let body = item["body"] as? [String: Any],
                let kind = body["kind"] as? String
            else { return nil }
            let actor = item["actor"] as? [String: Any]
            return FeedEntry(
                // The feed carries no entry id — a board-level line has no row
                // of its own — so position stands in. Stable within one read,
                // which is all a list that replaces wholesale needs.
                id: "\(index)",
                number: (item["number"] as? NSNumber)?.int64Value,
                who: (actor?["handle"] as? String).map { "@\($0)" },
                kind: kind,
                createdAtMs: (item["created_at_ms"] as? NSNumber)?.int64Value ?? 0)
        }
    }
}
