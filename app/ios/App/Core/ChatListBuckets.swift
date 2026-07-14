import Foundation

/// One row of the chat list: an ordinary conversation, or a **cron group** —
/// every fire of one scheduled job collapsed into a single row that pushes a
/// screen holding that job's history.
///
/// A group is a *view*, never a stored object (see `docs/cron-groups.md`): it is
/// read from `SessionRow.cronJobId`, which the gateway derives from the fire's
/// trigger. Nothing creates it, so nothing can leave one behind.
enum ChatListItem: Identifiable, Equatable {
    case chat(SessionRow)
    case cronGroup(CronGroup)

    var id: String {
        switch self {
        case .chat(let row): row.id
        case .cronGroup(let group): "cron:\(group.jobId)"
        }
    }

    /// What the row sorts on — a group rides its newest visible member, so when
    /// the job fires the group floats up. That is the whole point: 48 fires a
    /// day become **one row moving**.
    var lastActive: Date {
        switch self {
        case .chat(let row): row.lastActive
        case .cronGroup(let group): group.lastActive
        }
    }

    /// Pinned chats hold the top block. A group is never pinned — pinning is an
    /// affordance of an object, and a group is a view.
    var pinned: Bool {
        switch self {
        case .chat(let row): row.pinned
        case .cronGroup: false
        }
    }
}

/// One scheduled job's fires, collapsed. Derived per render from the rows — it
/// holds no identity of its own beyond the job's id.
struct CronGroup: Identifiable, Equatable {
    /// The `cron_job_id` every member shares. Also the group's identity.
    let jobId: String
    /// The job's live title, or the name it had when it was deleted.
    let title: String
    /// The fires drawn **inside** this group, newest first. Excludes members
    /// that escaped (see `ChatListBuckets`), so the aggregates below can never
    /// double-count a row that is already visible elsewhere.
    let members: [SessionRow]

    var id: String { jobId }

    /// Newest visible member's timestamp / preview / summed unread. All three
    /// are computed over `members` alone — a pinned fire is its own row up in
    /// the pinned block, and counting it here too would make the badge disagree
    /// with what the user sees when they open the group.
    var lastActive: Date { members.first?.lastActive ?? .distantPast }
    var preview: String? { members.first?.preview }
    var unread: Int { members.reduce(0) { $0 + $1.unread } }
    /// The sessions a "mark all read" acts on. Escaped rows clear themselves
    /// where they live.
    var memberIds: [String] { members.map(\.id) }
}

/// The pure bucketing rule behind the chat list. Lifted out of the view so it
/// can be tested — SwiftUI rendering cannot be, and this is where every
/// invariant that matters lives.
enum ChatListBuckets {
    /// **Every row appears exactly once.** A cron fire is drawn inside its group
    /// iff it is neither archived nor pinned:
    ///
    /// - **archived** → the Archived screen (filtered out here entirely),
    /// - **pinned** → escapes to the main list's pinned block, matching the web
    ///   sidebar, where `pinned` short-circuits before any grouping. Pinning
    ///   means *"keep this in my face"*, and burying it behind a tap would make
    ///   the gesture a no-op.
    ///
    /// A fire whose group cannot be **named** (`cronJobTitle == nil` — a fire
    /// minted before the title snapshot existed, whose job has since been
    /// deleted) stays flat rather than joining a nameless group.
    ///
    /// An empty group is unrepresentable: a group exists iff it has a member.
    /// So there is no "hide empty groups" rule here, and none is needed — nor
    /// can a job on another channel produce a ghost, since the session list is
    /// already channel-scoped server-side.
    static func items(from rows: [SessionRow]) -> [ChatListItem] {
        var flat: [SessionRow] = []
        var grouped: [String: [SessionRow]] = [:]
        var titles: [String: String] = [:]

        for row in rows where !row.archived {
            guard !row.pinned,
                let jobId = row.cronJobId,
                let title = row.cronJobTitle
            else {
                flat.append(row)
                continue
            }
            grouped[jobId, default: []].append(row)
            // Every member carries the same label (the gateway resolves it from
            // the job, not per-fire), so last-write-wins is a no-op — but taking
            // the newest member's keeps the group honest if a rename ever lands
            // mid-page.
            titles[jobId] = title
        }

        var items = flat.map(ChatListItem.chat)
        for (jobId, members) in grouped {
            guard let title = titles[jobId] else { continue }
            items.append(
                .cronGroup(
                    CronGroup(
                        jobId: jobId,
                        title: title,
                        members: members.sorted { $0.lastActive > $1.lastActive })))
        }

        // The list's existing grain: pinned block first, then most recent. A
        // group sorts among the chats, not above or below them — it *is* a
        // conversation stream.
        return items.sorted {
            if $0.pinned != $1.pinned { return $0.pinned }
            return $0.lastActive > $1.lastActive
        }
    }
}
