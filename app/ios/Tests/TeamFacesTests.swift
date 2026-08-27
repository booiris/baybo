import Testing

@testable import Baybo

@MainActor
struct TeamFacesTests {
    @Test func aTeamThatOverflowsLeavesRoomForTheCounter() {
        let drawn = TeamFaces.facesDrawn(of: TeamFaces.maxFaces + 4)
        #expect(drawn == TeamFaces.maxFaces)
        #expect(TeamFaces.maxFaces + 4 - drawn == 4)
    }

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
