import Foundation
import Testing

@testable import Baybo

/// The subagent sheet's row logic. Pure — no bridge, no FFI, no clock of its
/// own — which is the whole reason it lives outside the view.
@MainActor
struct SubagentListTests {
    private func at(_ seconds: TimeInterval) -> Date {
        Date(timeIntervalSince1970: 1_700_000_000 + seconds)
    }

    /// The errand the parent authored is what names a row. A child spawned
    /// before the gateway stamped it has none, and falling back to the profile
    /// beats falling back to a uuid — but a row must never be blank, so the id
    /// is still there as a last resort.
    @Test func titleFallsBackFromTaskToProfileToId() {
        #expect(
            SubagentList.title(task: "search sync", subagentType: "explorer", sessionId: "s1")
                == "search sync")
        #expect(SubagentList.title(task: nil, subagentType: "explorer", sessionId: "s1") == "explorer")
        #expect(SubagentList.title(task: "  ", subagentType: "explorer", sessionId: "s1") == "explorer")
        #expect(SubagentList.title(task: nil, subagentType: nil, sessionId: "s1") == "s1")
    }

    /// Every child runs in-process unless the parent asked otherwise, so
    /// printing the backend on every row would be noise on almost all of them.
    @Test func subtitleNamesOnlyAnExternalBackend() {
        #expect(SubagentList.subtitle(subagentType: "explorer", backend: "baybo") == "explorer")
        #expect(
            SubagentList.subtitle(subagentType: "explorer", backend: "claude")
                == "explorer · claude")
        #expect(SubagentList.subtitle(subagentType: nil, backend: "codex") == "codex")
        #expect(SubagentList.subtitle(subagentType: nil, backend: "baybo") == "")
    }

    /// A running child has no end, so its clock runs to NOW — which is what
    /// lets the row tick between the sheet's three-second polls instead of
    /// freezing on whatever the last response said.
    @Test func elapsedRunsToNowWhileTheChildIsOpen() {
        let running = SubagentList.elapsed(startedAt: at(0), endedAt: nil, now: at(42))
        #expect(running == 42)

        let settled = SubagentList.elapsed(startedAt: at(0), endedAt: at(12), now: at(9999))
        #expect(settled == 12)

        // Nothing honest to show before it starts.
        #expect(SubagentList.elapsed(startedAt: nil, endedAt: nil, now: at(5)) == nil)
    }

    /// A clock that ran backwards — the device's moved, or the row was written
    /// by a host whose clock differs — reads as zero, never as a negative age.
    @Test func elapsedNeverGoesNegative() {
        #expect(SubagentList.elapsed(startedAt: at(100), endedAt: nil, now: at(0)) == 0)
        #expect(SubagentList.elapsed(startedAt: at(100), endedAt: at(90), now: at(200)) == 0)
    }

    @Test func durationLabelCoarsensAsItGrows() {
        #expect(SubagentList.durationLabel(0) == "0s")
        #expect(SubagentList.durationLabel(47) == "47s")
        #expect(SubagentList.durationLabel(132) == "2m 12s")
        #expect(SubagentList.durationLabel(3849) == "1h 04m")
    }

    /// The gateway serialises `DateTime<Utc>` WITH fractional seconds, which
    /// the plain ISO8601 formatter rejects outright — parsing only one of the
    /// two shapes would silently drop every duration on the sheet.
    @Test func dateParsesBothWireShapes() {
        #expect(SubagentList.date("2026-08-13T03:04:05.123456Z") != nil)
        #expect(SubagentList.date("2026-08-13T03:04:05Z") != nil)
        #expect(SubagentList.date(nil) == nil)
        #expect(SubagentList.date("") == nil)
        #expect(SubagentList.date("not a date") == nil)
    }

    /// Each status has its own word, and none of them may fall through to
    /// another's: the catalogue is hand-maintained with no parity gate, so a
    /// missing key renders the RAW key on screen.
    @Test func everyStatusHasItsOwnKey() {
        let all: [ChatSubagentStatus] = [
            .pending, .running, .completed, .failed, .cancelled, .unknown,
        ]
        let keys = all.map { SubagentList.statusKey($0) }
        #expect(Set(keys).count == all.count)
        for key in keys {
            #expect(Lang.shared.t(key) != key, "missing localization for \(key)")
        }
    }

    /// The polling loop's whole condition. `unknown` counting as settled is a
    /// deliberate risk the FFI's own doc names — a future non-terminal status
    /// would decode into it and freeze that child's page until reopened.
    @Test func onlyPendingAndRunningAreLive() {
        #expect(ChatSubagentStatus.pending.isLive)
        #expect(ChatSubagentStatus.running.isLive)
        #expect(!ChatSubagentStatus.completed.isLive)
        #expect(!ChatSubagentStatus.failed.isLive)
        #expect(!ChatSubagentStatus.cancelled.isLive)
        #expect(!ChatSubagentStatus.unknown.isLive)
    }
}
