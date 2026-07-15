import Foundation
import Testing

@testable import Baybo

/// Cron groups (`docs/cron-groups.md`): a scheduled job's fires collapsed into
/// one chat-list row.
///
/// The grouping is a **derived view** over `SessionRow.cronJobId`, never a stored
/// object — so what is worth testing is not "does a folder exist" (none does) but
/// the two rules that decide what the user actually sees: **every row appears
/// exactly once**, and the group's aggregates count only the rows drawn inside it.
@Suite @MainActor
struct CronGroupBucketTests {
    private func row(
        _ id: String,
        minutesAgo: Int,
        unread: Int = 0,
        pinned: Bool = false,
        archived: Bool = false,
        jobId: String? = nil,
        jobTitle: String? = nil,
        groupPinned: Bool = false,
        preview: String? = nil
    ) -> SessionRow {
        SessionRow(
            id: id,
            createdAt: Date(timeIntervalSince1970: 0),
            lastActive: Date(timeIntervalSince1970: 1_000_000 - Double(minutesAgo) * 60),
            preview: preview,
            pinned: pinned,
            archived: archived,
            unread: unread,
            cronJobId: jobId,
            cronJobTitle: jobTitle,
            cronGroupPinned: groupPinned)
    }

    private func group(_ items: [ChatListItem], _ jobId: String) -> CronGroup? {
        items.compactMap { item -> CronGroup? in
            guard case .cronGroup(let g) = item, g.jobId == jobId else { return nil }
            return g
        }.first
    }

    private func chatIds(_ items: [ChatListItem]) -> [String] {
        items.compactMap { item -> String? in
            guard case .chat(let row) = item else { return nil }
            return row.id
        }
    }

    /// The whole point: a job that fires every half hour puts 48 rows a day in
    /// the list, and they must arrive as ONE.
    @Test func aJobsFiresCollapseIntoASingleRow() {
        let rows = (0..<48).map {
            row("fire-\($0)", minutesAgo: $0 * 30, jobId: "cj-1", jobTitle: "Morning brief")
        }
        let items = ChatListBuckets.items(from: rows)

        #expect(items.count == 1)
        #expect(group(items, "cj-1")?.members.count == 48)
        #expect(chatIds(items).isEmpty)
    }

    /// The group rides its newest visible member, so when the job fires the row
    /// floats up. That is what turns 48 rows a day into one row moving.
    @Test func theGroupSortsOnItsNewestVisibleFire() {
        let items = ChatListBuckets.items(from: [
            row("old-chat", minutesAgo: 5),
            row("fire-new", minutesAgo: 1, jobId: "cj-1", jobTitle: "Brief"),
            row("fire-old", minutesAgo: 900, jobId: "cj-1", jobTitle: "Brief"),
        ])

        #expect(items.first?.id == "cron:cj-1")
        #expect(group(items, "cj-1")?.lastActive == items.first?.lastActive)
    }

    /// The group's NAME is the newest fire's — matching web, which takes the
    /// first member over a newest-first list. This split from web (a master bug,
    /// shipped with cron groups): the fold used to be a plain last-write-wins
    /// over the same newest-first input, so it kept the OLDEST fire's label. It
    /// only shows when fires carry different labels — a renamed-then-deleted job,
    /// whose tombstone fires each fall back to the name they were minted under —
    /// where web then showed the renamed name and iOS the birth-time one.
    @Test func theGroupNameIsTheNewestFiresNotTheOldest() {
        // Input arrives newest-first (the gateway's `ORDER BY last_active DESC`).
        let items = ChatListBuckets.items(from: [
            row("fire-new", minutesAgo: 1, jobId: "cj-1", jobTitle: "Evening brief"),
            row("fire-old", minutesAgo: 900, jobId: "cj-1", jobTitle: "Morning brief"),
        ])

        #expect(
            group(items, "cj-1")?.title == "Evening brief",
            "the label must be the newest fire's, not the last-iterated (oldest) one")
    }

    /// Order-independence: the same fires shuffled into oldest-first order must
    /// still name the group after the newest one. (A last-write-wins fold would
    /// flip its answer with the input order; this must not.)
    @Test func theGroupNameIgnoresInputOrder() {
        let items = ChatListBuckets.items(from: [
            row("fire-old", minutesAgo: 900, jobId: "cj-1", jobTitle: "Morning brief"),
            row("fire-new", minutesAgo: 1, jobId: "cj-1", jobTitle: "Evening brief"),
        ])

        #expect(group(items, "cj-1")?.title == "Evening brief")
    }

    /// **Every row appears exactly once.** Pinning a fire means "keep this in my
    /// face" — burying it behind a tap would make the gesture a no-op — so it
    /// escapes to the pinned block, exactly as the web sidebar does. And because
    /// it escaped, the group must NOT still count it: the badge would then
    /// disagree with what the user sees on opening the group.
    @Test func aPinnedFireEscapesTheGroupAndIsNotCountedTwice() {
        let items = ChatListBuckets.items(from: [
            row("fire-pinned", minutesAgo: 1, unread: 3, pinned: true, jobId: "cj-1", jobTitle: "B"),
            row("fire-a", minutesAgo: 5, unread: 2, jobId: "cj-1", jobTitle: "B"),
        ])

        #expect(chatIds(items) == ["fire-pinned"], "a pinned fire is its own row, at the top")
        let g = group(items, "cj-1")
        #expect(g?.memberIds == ["fire-a"])
        #expect(g?.unread == 2, "the escaped row's 3 unread must not be summed here as well")
    }

    /// Archiving a fire sends it to the Archived screen — out of the group, and
    /// out of the main list entirely.
    @Test func anArchivedFireLeavesTheGroupAndTheList() {
        let items = ChatListBuckets.items(from: [
            row("fire-arch", minutesAgo: 1, unread: 9, archived: true, jobId: "cj-1", jobTitle: "B"),
            row("fire-a", minutesAgo: 5, unread: 1, jobId: "cj-1", jobTitle: "B"),
        ])

        #expect(chatIds(items).isEmpty)
        #expect(group(items, "cj-1")?.memberIds == ["fire-a"])
        #expect(group(items, "cj-1")?.unread == 1)
    }

    /// An empty group is unrepresentable — a group exists iff it has a visible
    /// member. So there is no "hide empty groups" rule to get wrong, and a job on
    /// another channel can never leave a ghost row behind.
    @Test func aGroupWhoseFiresAllEscapedDoesNotRender() {
        let items = ChatListBuckets.items(from: [
            row("f1", minutesAgo: 1, pinned: true, jobId: "cj-1", jobTitle: "B"),
            row("f2", minutesAgo: 2, archived: true, jobId: "cj-1", jobTitle: "B"),
        ])

        #expect(group(items, "cj-1") == nil)
        #expect(chatIds(items) == ["f1"], "only the pinned one is left, as its own row")
    }

    /// A fire minted before the title snapshot existed, whose job has since been
    /// deleted, can be named from neither source. It stays FLAT rather than
    /// joining a group with no name.
    @Test func anUnnameableFireStaysFlat() {
        let items = ChatListBuckets.items(from: [
            row("orphan", minutesAgo: 1, jobId: "cj-gone", jobTitle: nil)
        ])

        #expect(chatIds(items) == ["orphan"])
        #expect(group(items, "cj-gone") == nil)
    }

    /// Two jobs are two groups — never one bucket of "scheduled things".
    @Test func separateJobsAreSeparateGroups() {
        let items = ChatListBuckets.items(from: [
            row("a1", minutesAgo: 1, jobId: "cj-a", jobTitle: "A"),
            row("b1", minutesAgo: 2, jobId: "cj-b", jobTitle: "B"),
        ])

        #expect(items.count == 2)
        #expect(group(items, "cj-a")?.title == "A")
        #expect(group(items, "cj-b")?.title == "B")
    }

    /// The group's preview / timestamp come from its newest VISIBLE fire — the
    /// same rule as its unread sum, for the same reason.
    @Test func theGroupPreviewIsTheNewestVisibleFires() {
        let items = ChatListBuckets.items(from: [
            row("newest", minutesAgo: 1, pinned: true, jobId: "cj-1", jobTitle: "B", preview: "pinned"),
            row("visible", minutesAgo: 5, jobId: "cj-1", jobTitle: "B", preview: "in the group"),
        ])

        #expect(group(items, "cj-1")?.preview == "in the group")
    }
}

/// The persistence half. `PersistedFormatTests` only asserts a key is PRESENT in
/// the encoded JSON, so it cannot catch a field that is written but never decoded
/// — and `SessionRow`'s decoder is hand-written, one `decodeIfPresent` per
/// optional. Drop the cron lines and every group silently vanishes on cold start:
/// the rows come back, they just forget which job they belong to. Hence a real
/// round-trip through disk.
@Suite @MainActor
struct CronGroupPersistenceTests {
    @Test func theGroupingKeySurvivesAColdStart() throws {
        let temp = TempSupportDir()
        let index = temp.makeIndex()

        index.merge(
            remote: [
                ChatSessionSummary(
                    sessionId: "fire-1",
                    createdAt: "2026-07-01T00:00:00Z",
                    lastActive: "2026-07-14T08:00:00Z",
                    lastUserText: nil,
                    lastMessageText: "the brief",
                    title: "Morning brief · 7/14",
                    pinned: false,
                    archived: false,
                    unreadCount: 1,
                    cronJobId: "cj-1",
                    cronJobTitle: "Morning brief",
                    cronGroupPinned: false)
            ],
            fetchEpoch: index.mutationEpoch)

        // A second index over the same directory IS the cold start: it loads
        // sessions.json from disk with no in-memory state carried over.
        let reloaded = temp.makeIndex()
        let row = try #require(reloaded.rows.first { $0.id == "fire-1" })

        #expect(row.cronJobId == "cj-1")
        #expect(row.cronJobTitle == "Morning brief")
        #expect(
            ChatListBuckets.items(from: reloaded.rows).first?.id == "cron:cj-1",
            "a reloaded fire must still group — a missing decode reads nil off disk forever")
    }

    /// A `sessions.json` written before cron groups existed must load, not throw.
    @Test func aLegacyRowWithoutTheCronKeysStillDecodes() throws {
        let legacy = """
            [{"id":"s-1","createdAt":770000000,"lastActive":770000000,"pinned":false}]
            """
        let row = try JSONDecoder().decode([SessionRow].self, from: Data(legacy.utf8))

        #expect(row.count == 1)
        #expect(row[0].cronJobId == nil)
        #expect(row[0].cronJobTitle == nil)
    }
}

/// Pinning a cron GROUP. The bit lives on the JOB (`cron_jobs.pinned`), mirrored
/// onto every fire as `cronGroupPinned` — a group is a view, so it has no row of
/// its own to hold it, and the job is the only object whose identity matches.
@Suite @MainActor
struct CronGroupPinTests {
    private func fire(
        _ id: String, minutesAgo: Int, jobId: String = "cj-1",
        groupPinned: Bool = false, pinned: Bool = false
    ) -> SessionRow {
        SessionRow(
            id: id,
            createdAt: Date(timeIntervalSince1970: 0),
            lastActive: Date(timeIntervalSince1970: 1_000_000 - Double(minutesAgo) * 60),
            preview: nil,
            pinned: pinned,
            cronJobId: jobId,
            cronJobTitle: "Weekly digest",
            cronGroupPinned: groupPinned)
    }

    private func chat(_ id: String, minutesAgo: Int) -> SessionRow {
        SessionRow(
            id: id,
            createdAt: Date(timeIntervalSince1970: 0),
            lastActive: Date(timeIntervalSince1970: 1_000_000 - Double(minutesAgo) * 60),
            preview: nil,
            pinned: false)
    }

    /// The whole point of the feature. A group already rides its newest member,
    /// so a job that fires often is near the top anyway — it is the LOW-frequency
    /// job (the weekly digest, stale for six days) that sinks under ordinary
    /// chats. The pin lifts it into the pinned block.
    @Test func aPinnedGroupLeadsTheListEvenWhenItsLastFireIsAncient() {
        let items = ChatListBuckets.items(from: [
            chat("fresh", minutesAgo: 1),
            fire("old-fire", minutesAgo: 10_000, groupPinned: true),
        ])
        #expect(items.first?.id == "cron:cj-1")
        #expect(items.first?.pinned == true)
    }

    /// The pin is on the JOB, not its fires: no session's own `pinned` flips, so
    /// the members stay INSIDE the group rather than escaping to the pinned block.
    @Test func pinningTheGroupDoesNotPinItsFires() {
        let items = ChatListBuckets.items(from: [
            fire("f1", minutesAgo: 5, groupPinned: true),
            fire("f2", minutesAgo: 9, groupPinned: true),
        ])
        let group = items.compactMap { item -> CronGroup? in
            guard case .cronGroup(let g) = item else { return nil }
            return g
        }.first
        #expect(group?.pinned == true)
        #expect(group?.memberIds == ["f1", "f2"])
    }

    /// The two pins are different things and BOTH hold. A fire the user pinned by
    /// hand still escapes — even out of a pinned group — and each row is rendered
    /// exactly once: the escapee in the pinned block, the group as its own row.
    @Test func anIndividuallyPinnedFireStillEscapesAPinnedGroup() {
        let items = ChatListBuckets.items(from: [
            fire("escapee", minutesAgo: 5, groupPinned: true, pinned: true),
            fire("inside", minutesAgo: 9, groupPinned: true),
        ])
        let chatIds = items.compactMap { item -> String? in
            guard case .chat(let row) = item else { return nil }
            return row.id
        }
        let group = items.compactMap { item -> CronGroup? in
            guard case .cronGroup(let g) = item else { return nil }
            return g
        }.first
        #expect(chatIds == ["escapee"])
        #expect(group?.pinned == true)
        #expect(group?.memberIds == ["inside"], "the escapee must not also render inside")
    }

    /// An unpinned group keeps the old behaviour exactly: it sorts among the
    /// chats by its newest fire, not above them.
    @Test func anUnpinnedGroupStillSortsByRecency() {
        let items = ChatListBuckets.items(from: [
            chat("fresh", minutesAgo: 1),
            fire("old-fire", minutesAgo: 10_000),
        ])
        #expect(items.first?.id == "fresh")
        #expect(items.first?.pinned == false)
    }
}
