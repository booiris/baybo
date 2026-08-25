import Testing

@testable import Baybo

/// The row of faces a board draws for its team.
///
/// It used to take `prefix(5)` and drop the rest: a team of six drew as a team
/// of five, and nothing on screen admitted the other one existed. The tests
/// here pin what replaced that — a cap that COUNTS its remainder, and a counter
/// that never stands for a single face.
@MainActor
struct TeamFacesTests {
    /// The whole point: whatever the row leaves out, it says so.
    @Test func aTeamThatOverflowsLeavesRoomForTheCounter() {
        let drawn = TeamFaces.facesDrawn(of: TeamFaces.maxFaces + 4)
        #expect(drawn == TeamFaces.maxFaces)
        #expect(TeamFaces.maxFaces + 4 - drawn == 4)
    }

    /// A `+1` occupies exactly the width of the face it replaced, so drawing it
    /// would trade a teammate for the news that a teammate exists.
    @Test func oneOverIsDrawnRatherThanCounted() {
        #expect(TeamFaces.facesDrawn(of: TeamFaces.maxFaces + 1) == TeamFaces.maxFaces + 1)
        #expect(TeamFaces.facesDrawn(of: TeamFaces.maxFaces + 2) == TeamFaces.maxFaces)
    }

    @Test func aTeamThatFitsIsDrawnWhole() {
        for count in 0...TeamFaces.maxFaces {
            #expect(TeamFaces.facesDrawn(of: count) == count)
        }
    }
}
