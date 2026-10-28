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

    /// What the row's badge shows — the conversation's own count, or the group's
    /// sum over the fires drawn INSIDE it. One home for "does this row still want
    /// the user", so the unread WALK and the drawn badge cannot answer it apart.
    var unread: Int {
        switch self {
        case .chat(let row): row.unread
        case .cronGroup(let group): group.unread
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
    /// Whether any fire in this group is blocked on the approval gate.
    ///
    /// An OR, never a count: a count would imply the user could answer N of
    /// them from the list, and they cannot. The group's mark says only
    /// "something inside needs you"; the decision lives one tap deeper, where
    /// `CronGroupScreen` renders each member through the same row view and
    /// shows exactly which fire is waiting.
    var approvalPending: Bool { members.contains(where: \.approvalPending) }
    /// The sessions the group's own actions act on — "mark all read", and the
    /// delete that clears the job's execution records. Escaped rows are their own
    /// row elsewhere and answer for themselves there.
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
        //
        // The `id` tiebreak makes the order TOTAL. `Array.sorted(by:)` is not
        // documented stable and the group half is appended while iterating a
        // `Dictionary`, so two rows sharing `(pinned, lastActive)` had no defined
        // relative order between two calls — and `nextUnread` walks each row
        // exactly once, which an undefined order cannot support.
        return items.sorted {
            if $0.pinned != $1.pinned { return $0.pinned }
            if $0.lastActive != $1.lastActive { return $0.lastActive > $1.lastActive }
            return $0.id < $1.id
        }
    }

    /// The row a re-tap of the Chats tab steps to: the first row carrying unread
    /// STRICTLY AFTER `cursor` in the rendered order, wrapping back to the top.
    ///
    /// It WRAPS rather than stopping because the gesture deliberately does not
    /// consume unread (`docs/chat-list.md`), so the walk can never end by
    /// exhaustion — a terminal tap would leave an invisible gesture permanently
    /// dead. And it is addressed by `id`, never by index: rows arrive, re-sort and
    /// leave between taps, so a position is not a handle. A cursor whose row has
    /// left the list restarts the walk from the top rather than guessing a slot.
    static func nextUnread(after cursor: ChatListItem.ID?, in items: [ChatListItem])
        -> ChatListItem.ID?
    {
        guard !items.isEmpty else { return nil }
        let resume = cursor.flatMap { id in items.firstIndex { $0.id == id } }.map { $0 + 1 } ?? 0
        for step in 0..<items.count {
            let item = items[(resume + step) % items.count]
            if item.unread > 0 { return item.id }
        }
        return nil
    }
}

/// One line of the split view's sidebar (`ChatSplit`): the web sidebar's shape,
/// where a cron group is a FOLDER that opens in place instead of a row that
/// pushes a screen.
///
/// Every line is still its own `List` row, never a `DisclosureGroup` — a
/// disclosure nests its children inside one cell, which kills their per-row
/// swipe actions (`docs/cron-groups.md`).
enum SidebarEntry: Identifiable, Equatable {
    case section(SidebarSection)
    /// `nested`: drawn inside an open cron folder.
    case chat(SessionRow, nested: Bool)
    case cronFolder(CronGroup, expanded: Bool)

    /// The same ids the list's rows carry (`ChatListItem.id`), so the unread
    /// walk's cursor and `scrollTo` address a row identically in both shapes.
    var id: String {
        switch self {
        case .section(let section): "section:\(section.rawValue)"
        case .chat(let row, _): ChatListItem.chat(row).id
        case .cronFolder(let group, _): ChatListItem.cronGroup(group).id
        }
    }

    /// What the unread walk can land on. An OPEN folder hands its count to the
    /// fires drawn under it, or one tap would land on the folder and the next on
    /// the fire it was counting.
    var walkItem: ChatListItem? {
        switch self {
        case .section: nil
        case .chat(let row, _): .chat(row)
        case .cronFolder(let group, let expanded): expanded ? nil : .cronGroup(group)
        }
    }
}

/// The sidebar's blocks, in the web sidebar's order: Pinned, then the cron
/// folders where web keeps its folders, then every other conversation.
enum SidebarSection: String {
    case pinned, scheduled, chats

    var titleKey: String { "split.\(rawValue)" }
}

extension ChatListBuckets {
    /// The sidebar's lines, from the list's own items, so a row lands in the same
    /// bucket in both shapes (`items(from:)` owns every rule about that).
    ///
    /// A pinned GROUP stays in the folder block, at its head, as on the web: the
    /// Pinned block holds conversations. `items` already sorts pinned first, so
    /// filtering keeps that order. The trailing block's label only renders when
    /// a block above it does — alone, it would be labelling the whole list.
    static func sidebar(from items: [ChatListItem], expanded: Set<String>) -> [SidebarEntry] {
        var pinned: [SidebarEntry] = []
        var folders: [SidebarEntry] = []
        var chats: [SidebarEntry] = []
        for item in items {
            switch item {
            case .chat(let row) where row.pinned:
                pinned.append(.chat(row, nested: false))
            case .chat(let row):
                chats.append(.chat(row, nested: false))
            case .cronGroup(let group):
                let open = expanded.contains(group.jobId)
                folders.append(.cronFolder(group, expanded: open))
                if open { folders += group.members.map { .chat($0, nested: true) } }
            }
        }

        var entries: [SidebarEntry] = []
        if !pinned.isEmpty { entries += [.section(.pinned)] + pinned }
        if !folders.isEmpty { entries += [.section(.scheduled)] + folders }
        if !chats.isEmpty {
            if !entries.isEmpty { entries.append(.section(.chats)) }
            entries += chats
        }
        return entries
    }

    /// The cron group that draws `sessionId`, if any — the folder the sidebar
    /// opens so the docked conversation's row is on screen, as web expands the
    /// active chat's group.
    static func group(drawing sessionId: String, in items: [ChatListItem]) -> String? {
        for case .cronGroup(let group) in items where group.memberIds.contains(sessionId) {
            return group.jobId
        }
        return nil
    }
}
