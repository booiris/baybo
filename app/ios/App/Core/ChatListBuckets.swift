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

    /// Pinned rows hold the top block — a group included.
    ///
    /// A group is still a *view*, so it has no row to carry the bit: the pin
    /// lives on the JOB (`cron_jobs.pinned`, surfaced on every fire as
    /// `cronGroupPinned`), which is the only object whose identity matches the
    /// group. That is what makes pinning one coherent without storing the group.
    ///
    /// It earns its keep on a LOW-frequency job. A group already rides its
    /// newest member (below), so a job that fires often is permanently near the
    /// top anyway — it is the weekly digest, sinking between fires, that needs a
    /// seat.
    var pinned: Bool {
        switch self {
        case .chat(let row): row.pinned
        case .cronGroup(let group): group.pinned
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
    /// Whether the user pinned this group. Read off the JOB (every member row
    /// carries the same `cronGroupPinned`), never stored for the group itself.
    let pinned: Bool
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
        var groupPins: [String: Bool] = [:]

        for row in rows where !row.archived {
            guard !row.pinned,
                let jobId = row.cronJobId,
                row.cronJobTitle != nil
            else {
                // A pinned FIRE still escapes, even out of a pinned group. The
                // two pins are different things — "keep this one conversation in
                // my face" vs "keep this recurring stream at the top" — and both
                // hold: the escapee renders once in the pinned block, the group
                // renders once as its own pinned row. Neither is drawn twice.
                flat.append(row)
                continue
            }
            grouped[jobId, default: []].append(row)
            // One bit on the job, mirrored onto every fire. `||` rather than
            // last-write-wins so a half-applied optimistic flip (a page that
            // raced the merge) still reads as pinned.
            groupPins[jobId] = (groupPins[jobId] ?? false) || row.cronGroupPinned
        }

        var items = flat.map(ChatListItem.chat)
        for (jobId, members) in grouped {
            let sorted = members.sorted { $0.lastActive > $1.lastActive }
            // The label is the NEWEST member's, NOT last-write-wins over the
            // input order. While the job lives the gateway resolves the same live
            // title for every fire, so it makes no difference — but a deleted
            // job's fires each fall back to the name they were minted under, and
            // if it was renamed mid-life those snapshots differ. Web takes the
            // newest (first over its newest-first list); last-write-wins here took
            // the OLDEST, so iOS showed the birth-time name where web showed the
            // renamed one. The guard above already proved every member's title is
            // non-nil, so the newest's is too.
            guard let title = sorted.first?.cronJobTitle else { continue }
            items.append(
                .cronGroup(
                    CronGroup(
                        jobId: jobId,
                        title: title,
                        pinned: groupPins[jobId] ?? false,
                        members: sorted)))
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
