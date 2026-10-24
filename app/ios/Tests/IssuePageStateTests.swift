import Foundation
import Testing

@testable import Baybo

/// The card page's view state as native carries it across a covered visit:
/// what the page posts (`issueState` / `snapshotState`) and what goes back to it
/// as `restoredState`. The `anchor` is what lets a visit covered on one display
/// and uncovered on the other — a foldable folded or opened in between —
/// restore by the block the reader was on rather than by a `scrollTop` that
/// every rewrapped row above it has invalidated.
@Suite @MainActor
struct IssuePageStateTests {
    private typealias PageState = IssueStore.PageState

    /// The page posts through `JSON.stringify` and the bridge reads it through
    /// `JSONSerialization`, so numbers arrive as `NSNumber` — parse what that
    /// path really produces rather than a hand-built Swift dictionary.
    private static func posted(_ json: String) -> [String: Any] {
        (try? JSONSerialization.jsonObject(with: Data(json.utf8)) as? [String: Any]) ?? [:]
    }

    @Test func anAnchoredStateRoundTrips() throws {
        let state = try #require(
            PageState(
                wire: Self.posted(
                    #"{"scrollTop":812.5,"folds":{"runs":true},"anchor":{"id":"e7","offset":-14.25,"width":402}}"#
                )))
        #expect(state.scrollTop == 812.5)
        #expect(state.folds == ["runs": true])
        #expect(state.anchor == PageState.ScrollAnchor(id: "e7", offset: -14.25, width: 402))

        // …and what goes back to the page parses to the same state.
        let data = try JSONSerialization.data(withJSONObject: state.wire)
        let back = try #require(
            JSONSerialization.jsonObject(with: data) as? [String: Any])
        #expect(PageState(wire: back) == state)
    }

    /// No anchor on the way in is today's state, and none goes back out — not
    /// even a null key, so the page keeps restoring by `scrollTop`.
    @Test func anUnanchoredStateSendsNoAnchorBack() throws {
        let state = try #require(
            PageState(wire: Self.posted(#"{"scrollTop":40,"folds":{}}"#)))
        #expect(state.anchor == nil)
        #expect(state.wire["anchor"] == nil)
    }

    /// A malformed anchor costs the anchor, never the whole state.
    @Test(arguments: [
        #"{"id":"e7","offset":1}"#,
        #"{"offset":1,"width":402}"#,
        #"{"id":"e7","offset":1,"width":0}"#,
        #"null"#,
    ])
    func aMalformedAnchorIsDropped(anchor: String) throws {
        let state = try #require(
            PageState(wire: Self.posted(#"{"scrollTop":40,"folds":{},"anchor":\#(anchor)}"#)))
        #expect(state.scrollTop == 40)
        #expect(state.anchor == nil)
    }

    @Test func aStateWithoutItsCoreFieldsIsRejected() {
        #expect(PageState(wire: Self.posted(#"{"folds":{}}"#)) == nil)
        #expect(PageState(wire: Self.posted(#"{"scrollTop":4}"#)) == nil)
    }

    /// The store keeps the anchor alongside the rest, and still refuses a
    /// nonsense `scrollTop`.
    @Test func theStoreKeepsTheAnchor() {
        let dir = TempSupportDir()
        let store = IssueStore(
            projectId: "p1", number: 41, client: FakeBayboClient(), supportDirectory: dir.url)
        let anchored = PageState(
            scrollTop: 10, folds: [:],
            anchor: PageState.ScrollAnchor(id: "e1", offset: 3, width: 951))
        store.rememberPageState(anchored)
        #expect(store.pageState == anchored)

        store.rememberPageState(PageState(scrollTop: -1, folds: [:]))
        #expect(store.pageState == anchored)
    }
}
