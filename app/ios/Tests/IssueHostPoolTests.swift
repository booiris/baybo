import Foundation
import Testing

@testable import Baybo

@Suite struct IssueHostPoolTests {
    @Test func aGrandchildReusesTheGrandparentsSlot() {
        var plan = IssueHostPoolPlan()
        let parent = UUID()
        let child = UUID()
        let grandchild = UUID()

        #expect(plan.open(parent) == 0)
        _ = plan.didAppear(parent)
        #expect(plan.open(child) == 1)
        _ = plan.didAppear(child)
        #expect(plan.open(grandchild) == 0)
        _ = plan.didAppear(grandchild)

        #expect(plan.slots == [grandchild, child])
    }

    @Test func completingAPopRestoresThePageUnderTheNewTop() {
        var plan = IssueHostPoolPlan()
        let parent = UUID()
        let child = UUID()
        let grandchild = UUID()

        _ = plan.open(parent)
        _ = plan.didAppear(parent)
        _ = plan.open(child)
        _ = plan.didAppear(child)
        _ = plan.open(grandchild)
        _ = plan.didAppear(grandchild)

        let assignments = plan.didAppear(child)
        #expect(assignments.contains { $0.slot == 0 && $0.id == parent })
        #expect(plan.slots == [parent, child])

        _ = plan.close(grandchild)
        #expect(
            plan.slots == [parent, child],
            "the outgoing visit must not clear a slot already restored to its parent")
    }

    @Test func aDirectMultiLevelPopAlsoProtectsTheVisibleSlot() {
        var plan = IssueHostPoolPlan()
        let visits = (0..<5).map { _ in UUID() }

        for visit in visits {
            _ = plan.open(visit)
            _ = plan.didAppear(visit)
        }
        _ = plan.didAppear(visits[0])

        #expect(plan.slots[0] == visits[0])
        let next = UUID()
        #expect(
            plan.open(next) == 1,
            "a new push must reuse the covered slot, never the page currently on screen")
    }
}
