import XCTest

@testable import Baybo

/// The search screen's querying rules, driven with no UI and no gateway.
///
/// All three of these are cheap to get wrong and invisible when they are: a
/// missing debounce only shows up as tunnel load, a missing staleness guard only
/// shows up when the network reorders two answers, and the composition gate only
/// shows up on a Chinese keyboard.
@MainActor
final class SearchModelTests: XCTestCase {
    /// Short enough to keep the suite fast, long enough that a burst inside it is
    /// unambiguous.
    private let debounce: Duration = .milliseconds(40)

    private func model(
        _ client: FakeBayboClient,
        composing: @escaping () -> Bool = { false }
    ) -> SearchModel {
        SearchModel(client: client, debounce: debounce, isComposing: composing)
    }

    private func settle(_ multiplier: Double = 4) async {
        try? await Task.sleep(for: .milliseconds(Int(40 * multiplier)))
    }

    private func groups(_ model: SearchModel) -> [ChatSearchGroup]? {
        if case .ok(let groups, _) = model.phase { return groups }
        return nil
    }

    private func isIdle(_ model: SearchModel) -> Bool {
        if case .idle = model.phase { return true }
        return false
    }

    private func isFailed(_ model: SearchModel) -> Bool {
        if case .failed = model.phase { return true }
        return false
    }

    private func results(_ sessionId: String) -> ChatSearchResults {
        ChatSearchResults(
            groups: [
                ChatSearchGroup(
                    sessionId: sessionId, sessionTitle: nil,
                    hits: [
                        ChatSearchHit(
                            ordinal: 7, role: "user", text: "hit",
                            createdAt: "2026-08-12T00:00:00Z", supersededBy: nil)
                    ],
                    totalHits: 1)
            ],
            truncated: false)
    }

    /// One Han character is a legitimate query the index can answer and a useless
    /// one to answer — it matches nearly every conversation.
    func testAQueryShorterThanTheMinimumNeverReachesTheGateway() async {
        let client = FakeBayboClient()
        let model = model(client)

        model.update(query: "数")
        await settle()

        XCTAssertEqual(client.searchCalls, [])
        XCTAssertTrue(isIdle(model))
    }

    func testWhitespaceIsNotLength() async {
        let client = FakeBayboClient()
        let model = model(client)

        model.update(query: "  a   ")
        await settle()

        XCTAssertEqual(client.searchCalls, [])
        XCTAssertTrue(isIdle(model))
    }

    func testATypingBurstCollapsesIntoOneRequestForTheLastQuery() async {
        let client = FakeBayboClient()
        let model = model(client)

        for text in ["da", "dat", "data", "datab"] {
            model.update(query: text)
        }
        await settle()

        XCTAssertEqual(client.searchCalls, ["datab"])
    }

    /// The query is sent trimmed — a trailing space is not part of what the user
    /// means, and the server ANDs on whitespace.
    func testTheQueryIsTrimmedBeforeItIsSent() async {
        let client = FakeBayboClient()
        let model = model(client)

        model.update(query: "  数据库  ")
        await settle()

        XCTAssertEqual(client.searchCalls, ["数据库"])
    }

    /// A Chinese keyboard puts the uncommitted pinyin in the binding on the way
    /// to 数据. Searching it costs a tunnel round trip and flashes "no matches"
    /// against a query nobody typed.
    func testAnOpenCompositionIsNotSearched() async {
        let client = FakeBayboClient()
        var composing = true
        let model = model(client, composing: { composing })

        model.update(query: "shuju")
        await settle()
        XCTAssertEqual(client.searchCalls, [], "uncommitted pinyin must not be sent")

        // Choosing the candidate commits the text and fires another change.
        composing = false
        model.update(query: "数据")
        await settle()
        XCTAssertEqual(client.searchCalls, ["数据"])
    }

    /// Cancelling the in-flight task cannot un-send a request that is already
    /// awaiting its answer, and the relay leg can reorder two of them. The last
    /// query issued is the only one allowed to write the phase.
    func testALateAnswerForASupersededQueryIsDiscarded() async {
        let client = FakeBayboClient()
        client.stubSearch("slow", with: results("stale-session"))
        client.stubSearch("fast", with: results("fresh-session"))
        let model = model(client)

        model.update(query: "slow")
        // Let the debounce elapse so "slow" is genuinely in flight, then supersede
        // it before its answer can land.
        await settle(0.5)
        model.update(query: "fast")
        await settle()

        XCTAssertEqual(groups(model)?.map(\.sessionId), ["fresh-session"])
    }

    func testAFailureForASupersededQueryDoesNotClobberTheNewerResults() async {
        let client = FakeBayboClient()
        let model = model(client)

        client.failSearch(with: NSError(domain: "test", code: 1))
        model.update(query: "boom")
        await settle()
        XCTAssertTrue(isFailed(model))

        // A later query that succeeds must recover the view.
        let ok = FakeBayboClient()
        ok.stubSearch("fine", with: results("s1"))
        let recovered = self.model(ok)
        recovered.update(query: "fine")
        await settle()
        XCTAssertEqual(groups(recovered)?.map(\.sessionId), ["s1"])
    }

    /// On a relay leg the next answer can be seconds away. Blanking the list
    /// meanwhile reads as "your results vanished", so the previous ones stay up
    /// until the new ones land.
    func testResultsStayOnScreenWhileTheNextQueryIsInFlight() async {
        let client = FakeBayboClient()
        client.stubSearch("first", with: results("s1"))
        let model = model(client)

        model.update(query: "first")
        await settle()
        XCTAssertEqual(groups(model)?.map(\.sessionId), ["s1"])

        model.update(query: "second")
        // Mid-debounce: the request has not even been issued yet.
        XCTAssertEqual(
            groups(model)?.map(\.sessionId), ["s1"],
            "the old results must survive until the new ones arrive")
    }

    /// Clearing the field returns to the hint, and must not leave a stale result
    /// set behind it.
    func testClearingTheFieldReturnsToIdle() async {
        let client = FakeBayboClient()
        client.stubSearch("data", with: results("s1"))
        let model = model(client)

        model.update(query: "data")
        await settle()
        XCTAssertNotNil(groups(model))

        model.update(query: "")
        XCTAssertTrue(isIdle(model))
    }
}
