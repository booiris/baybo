import Foundation
import Testing

@testable import Baybo

/// The cross-end gate for the composer hint.
///
/// What a comment does besides being recorded is decided by
/// `crates/project/src/comments.rs::comment_delivery` and is **not exposed
/// over REST** — a composer has to say what sending will do while the text is
/// still being typed. That forces a copy per client, and there are three:
/// the Rust rule, `app/web`'s `timelineModel.commentHint`, and
/// `CommentHint.swift`. Nothing makes them agree except tests, and two of the
/// three stay green while the other drifts.
///
/// So the web is the reference and this suite holds the port to it, over the
/// SAME file `app/web`'s own vitest suite asserts against — one fixture, two
/// readers, the arrangement `SearchSnippetVectorTests` already established. A
/// second copy of the fixture would be a new drift surface inside the gate
/// built to close one.
///
/// The file is read off disk rather than bundled: `#filePath` is this file's
/// own source path, so the walk to the repo root holds wherever the checkout
/// lives and there is no resource to remember to add to the target.
///
/// Going red after `pnpm --filter baybo-web gen:comment-hint-vectors` is the
/// gate working — the rules moved on the reference side and this port has not
/// been brought along.
@Suite struct CommentHintVectorTests {
    private struct Vectors: Decodable {
        let comment: [CommentVector]
        let mention: [MentionVector]
    }

    private struct Issue: Decodable {
        let status: String?
        let assignee: String?
        let cancelled_at_ms: Int64?
        let blocked_reason: String?
    }

    private struct Run: Decodable { let status: String }
    private struct Agent: Decodable {
        let id: String
        let handle: String
    }

    private struct CommentVector: Decodable {
        let name: String
        let issue: Issue
        let runs: [Run]
        let team: [Agent]
        let hint: String
    }

    private struct MentionVector: Decodable {
        let name: String
        let issue: Issue
        let draft: String
        let team: [Agent]
        let hint: String?
    }

    private static var vectorsURL: URL {
        // Tests/ -> app/ios/ -> app/ -> repo root
        URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("app/web/src/pages/projects/commentHintVectors.json")
    }

    private func loadVectors() throws -> Vectors {
        let url = Self.vectorsURL
        let data = try #require(
            try? Data(contentsOf: url),
            """
            Shared hint vectors not found at \(url.path).
            Regenerate with: pnpm --filter baybo-web gen:comment-hint-vectors
            """
        )
        return try JSONDecoder().decode(Vectors.self, from: data)
    }

    /// The status word the fixture carries is the wire's, so the mapping under
    /// test is the same one the FFI performs.
    private func status(_ word: String?) -> IssueStatus {
        switch word {
        case "backlog": .backlog
        case "todo": .todo
        case "in_progress": .inProgress
        case "review": .review
        case "done": .done
        default: .unknown
        }
    }

    private func runStatus(_ word: String) -> RunStatus {
        switch word {
        case "held": .held
        case "queued": .queued
        case "running": .running
        case "done": .done
        case "failed": .failed
        case "cancelled": .cancelled
        default: .unknown
        }
    }

    /// The live run is the one holding the card's slot. The web finds it with
    /// `unsettledRun`; the fixture carries statuses only, so the same rule is
    /// applied here rather than assumed.
    private func liveRunStatus(_ runs: [Run]) -> RunStatus? {
        runs.map { runStatus($0.status) }
            .first { $0 == .held || $0 == .queued || $0 == .running }
    }

    @Test func theCommentHintMatchesTheReferenceByteForByte() throws {
        let vectors = try loadVectors()
        // A canary: a regen that produced a thinner file would leave this
        // suite green over almost nothing.
        #expect(vectors.comment.count >= 16)

        for vector in vectors.comment {
            let team = vector.team.map { agent in
                TeamMemberInfo(
                    id: agent.id, handle: agent.handle, name: agent.handle, description: "",
                    avatarBlobId: nil, framework: "baybo", llm: nil, model: nil,
                    reasoningEffort: nil, lead: false, hiredBy: nil, createdAtMs: 0)
            }
            let handle = vector.issue.assignee.map {
                CommentHint.handle(forAgent: $0, in: team)
            }
            let actual = CommentHint.text(
                status: status(vector.issue.status),
                assigneeHandle: handle,
                cancelled: vector.issue.cancelled_at_ms != nil,
                blockedReason: vector.issue.blocked_reason,
                liveRunStatus: liveRunStatus(vector.runs)
            )
            #expect(actual == vector.hint, "\(vector.name)")
        }
    }

    @Test func theMentionHintMatchesTheReferenceByteForByte() throws {
        let vectors = try loadVectors()
        #expect(vectors.mention.count >= 10)
        // Both outcomes are represented, or the suite asserts nothing about
        // the sentence it exists to pin.
        #expect(vectors.mention.contains { $0.hint == nil })
        #expect(vectors.mention.contains { $0.hint != nil })

        for vector in vectors.mention {
            let actual = CommentHint.mention(
                assigneeHandle: vector.issue.assignee,
                blockedReason: vector.issue.blocked_reason,
                draft: vector.draft,
                teamHandles: vector.team.map(\.handle)
            )
            #expect(actual == vector.hint, "\(vector.name)")
        }
    }
}
