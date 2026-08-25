#if DEBUG

    import Foundation

    /// `-baybo-demo-projects`: a canned set of boards so the Projects tab
    /// renders headlessly with no gateway to fetch from.
    ///
    /// Seeded into memory only — never persisted, and every refresh is
    /// short-circuited rather than answered, so a later plain launch on the same
    /// simulator inherits nothing (`ModelCatalog`'s rule, for the same reason).
    ///
    /// The shape is chosen to make every branch of the cards root paintable in
    /// one screenshot: a board that wants you (all three attention kinds at
    /// once), a board that is merely busy, a board doing nothing, and an
    /// archived one so the archived toggle exists to press.
    extension ProjectsStore {
        static let demoArg = "-baybo-demo-projects"
        /// The board `-baybo-demo-board` opens: the one with something in
        /// every stage and all four Waiting kinds.
        static let demoBoardId = "p-rglide"

        static var demoRequested: Bool {
            ProcessInfo.processInfo.arguments.contains(demoArg)
        }

        /// A fixed "now" the fixture hangs its ages off. Timestamps of `0`
        /// render as `20689d`, which is not a card anybody has ever seen.
        private static var nowMs: Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

        func seedDemo() {
            let projects: [ProjectInfo] = [
                Self.demoProject(
                    "p-rglide", name: "rglide", description: "the relay glide rewrite",
                    budgetUsd: 40),
                Self.demoProject(
                    "p-atlas", name: "atlas", description: "docs + site", budgetUsd: 8),
                Self.demoProject("p-scratch", name: "scratch", description: "", budgetUsd: nil),
                Self.demoProject(
                    "p-old", name: "harness", description: "retired", budgetUsd: nil,
                    archivedAtMs: 1),
            ]
            let attention: [String: ProjectAttention] = [
                "p-rglide": ProjectAttention(
                    projectId: "p-rglide", name: "rglide", approvals: 2, failed: 1, unread: 3),
                "p-atlas": ProjectAttention(
                    projectId: "p-atlas", name: "atlas", approvals: 0, failed: 0, unread: 1),
            ]
            let activity: [String: ProjectActivity] = [
                // Over its ceiling on tokens but not on dollars — the two meters
                // disagree on purpose, which is the case a single bar hides.
                "p-rglide": ProjectActivity(
                    projectId: "p-rglide", working: 3, burnMicros: 31_400_000,
                    burnTokens: 2_100_000),
                "p-atlas": ProjectActivity(
                    projectId: "p-atlas", working: 1, burnMicros: 7_800_000, burnTokens: 40_000),
            ]
            var boards: [String: Board] = [:]
            boards["p-rglide"] = Board(
                issues: [
                    Self.demoIssue(
                        41, "p-rglide", title: "the dial loop drops its subscription",
                        status: .inProgress, priority: .urgent, assignee: "a-dev",
                        unread: 2, approvalPending: true),
                    Self.demoIssue(
                        42, "p-rglide", title: "keepalive should feed liveness, not the timer",
                        status: .inProgress, priority: .high, assignee: "a-dev2",
                        lastRunFailed: true, pinned: true),
                    Self.demoIssue(
                        43, "p-rglide", title: "write the connection doc", status: .todo,
                        priority: .medium, assignee: nil, unread: 1),
                    Self.demoIssue(
                        44, "p-rglide", title: "retire the old pump tee", status: .todo,
                        priority: .low, assignee: nil),
                    Self.demoIssue(
                        40, "p-rglide", title: "NACK a send with no live leg", status: .review,
                        priority: .high, assignee: "a-dev"),
                    Self.demoIssue(
                        39, "p-rglide", title: "fence the disconnect generation", status: .done,
                        priority: .high, assignee: "a-dev"),
                    Self.demoIssue(
                        38, "p-rglide", title: "blocked on the relay token format",
                        status: .todo, priority: .medium, assignee: "a-lead",
                        blockedReason: "needs the relay token format"),
                ] + Self.demoQueue(),
                runs: [
                    Self.demoRun(41, agent: "a-dev", status: .running, trigger: .promoted),
                    Self.demoRun(42, agent: "a-dev2", status: .queued, trigger: .retry),
                    Self.demoRun(43, agent: "a-lead", status: .running, trigger: .started),
                ],
                // Eight, which is two past what the face row draws: the row
                // caps at `TeamFaces.maxFaces` and counts the rest, and a
                // fixture that fits leaves both the counter and the truncation
                // it replaced unpaintable. `dev-*` against `docs-*` also forces
                // `AgentMonogram` to widen the whole set.
                team: [
                    Self.demoMember("a-lead", "lead", lead: true),
                    Self.demoMember("a-dev", "dev-1"),
                    Self.demoMember("a-dev2", "dev-2"),
                    Self.demoMember("a-doc", "docs-1"),
                    Self.demoMember("a-dev3", "dev-3"),
                    Self.demoMember("a-doc2", "docs-2"),
                    Self.demoMember("a-qa", "qa-1"),
                    Self.demoMember("a-rev", "reviewer-1"),
                ],
                fetchedAtMs: 1)
            boards["p-atlas"] = Board(
                issues: [
                    Self.demoIssue(
                        7, "p-atlas", title: "the landing copy", status: .inProgress,
                        priority: .medium, assignee: "a-doc", unread: 1)
                ],
                runs: [Self.demoRun(7, agent: "a-doc", status: .running, trigger: .promoted)],
                team: [Self.demoMember("a-doc", "docs-1", lead: true)],
                fetchedAtMs: 1)

            installDemo(
                projects: projects, attention: attention, activity: activity,
                boards: boards,
                approvalPrompts: [
                    Self.demoBoardId: [
                        41: [
                            IssueApprovalPrompt(
                                callId: "call-1", tool: "exec_command",
                                summary: "cargo test -p baybo-relay",
                                askedBy: "dev-1", askedAtMs: Self.nowMs - 120_000)
                        ]
                    ]
                ])
        }

        private static func demoProject(
            _ id: String, name: String, description: String, budgetUsd: Double?,
            archivedAtMs: Int64? = nil
        ) -> ProjectInfo {
            ProjectInfo(
                id: id, name: name, description: description, workdir: "/work/\(name)",
                dailyBudgetMicros: budgetUsd.map { Int64($0 * 1_000_000) },
                dailyBudgetTokens: name == "rglide" ? 1_000_000 : nil,
                maxParallelIssueRuns: 3,
                // The board the demo opens merges; the others do not, so one
                // launch paints both halves of the auto-merge hint.
                agentsMayMerge: id == Self.demoBoardId, archivedAtMs: archivedAtMs,
                createdAtMs: 0, updatedAtMs: 0)
        }

        /// Todo's tail: enough cards that the stage does not fit on a phone.
        ///
        /// A fixture where every stage fits leaves the whole SCROLLING half of
        /// this screen unpaintable — the pinned stage bar, the band headers
        /// passing under it, pull-to-refresh. Deliberately plain: these are
        /// here to have height, and anything interesting on them would compete
        /// with the cards above that are here to show a state.
        private static func demoQueue() -> [IssueInfo] {
            [
                "drop the retired pump tee", "name the fence generations",
                "one door for the send gate", "fold the ack judgement in",
                "stop logging the token", "measure the first-frame gap",
                "delete the old relay shim",
            ].enumerated().map { index, title in
                demoIssue(
                    Int64(45 + index), "p-rglide", title: title, status: .todo,
                    priority: index.isMultiple(of: 3) ? .medium : .low, assignee: nil)
            }
        }

        private static func demoIssue(
            _ number: Int64, _ projectId: String, title: String, status: IssueStatus,
            priority: IssuePriority, assignee: String?, unread: Int64 = 0,
            lastRunFailed: Bool = false, approvalPending: Bool = false,
            blockedReason: String? = nil, pinned: Bool = false
        ) -> IssueInfo {
            IssueInfo(
                number: number, projectId: projectId, title: title, description: "",
                attachments: [], status: status, priority: priority, assignee: assignee,
                position: number, pinned: pinned, branch: nil, blockedReason: blockedReason,
                parent: nil, filedFrom: nil, stage: 0,
                subIssues: number == 41 ? SubIssueProgress(done: 2, total: 5) : nil,
                unread: unread, lastRunFailed: lastRunFailed, approvalPending: approvalPending,
                openedByAgent: number == 38, cancelledAtMs: nil,
                createdAtMs: nowMs - 86_400_000,
                // Spread the ages so the row's age column shows a range rather
                // than one number repeated down the list.
                updatedAtMs: nowMs - Int64(number % 7 + 1) * 900_000)
        }

        private static func demoRun(
            _ number: Int64, agent: String, status: RunStatus, trigger: RunTrigger
        ) -> IssueRunInfo {
            IssueRunInfo(
                number: number, attempt: 1, agentId: agent, status: status, trigger: trigger,
                sessionId: "s-\(number)",
                error: status == .failed ? "the sandbox exited 137" : nil,
                createdAtMs: nowMs - 900_000,
                // Roughly 12 minutes of elapsed, so the run word carries a
                // duration rather than a suspicious `0s`.
                startedAtMs: Int64(Date().timeIntervalSince1970 * 1000) - 720_000,
                settledAtMs: nil, costMicros: nil, inputTokens: nil, outputTokens: nil)
        }

        private static func demoMember(_ id: String, _ handle: String, lead: Bool = false)
            -> TeamMemberInfo
        {
            // Two carry a picture, so one screenshot shows both paths: an
            // uploaded avatar and the monogram an agent without one falls back
            // to.
            let avatar: String? =
                switch handle {
                case "lead": "\(AgentAvatars.demoPrefix)1f6f5b"
                case "dev-1": "\(AgentAvatars.demoPrefix)c2703a"
                default: nil
                }
            return TeamMemberInfo(
                id: id, handle: handle, name: handle, description: "", avatarBlobId: avatar,
                framework: "baybo", llm: "claude", model: "claude-sonnet-5",
                reasoningEffort: nil, lead: lead, hiredBy: nil, createdAtMs: 0)
        }
    }

#endif
