import SwiftUI

struct RootView: View {
    @EnvironmentObject private var store: AppStore

    var body: some View {
        ZStack {
            Theme.paper.ignoresSafeArea()
            switch store.route {
            case .launching:
                ProgressView()
                    .tint(Theme.inkSoft)
            case .landing:
                if store.challenge != nil {
                    PairConfirmView()
                } else {
                    // Menu vs the direct form swap INSIDE the landing hero
                    // (the wordmark stays), mirroring the web.
                    LandingView()
                }
            case .home:
                // Home is the native TabView shell wrapped in ONE NavigationStack:
                // a pushed ChatScreen covers the whole shell (tab bar included),
                // so backing out reveals the bar together with the pop transition
                // instead of popping it back in afterward. The system nav bar
                // stays hidden (custom chrome); ChatScreen's PopGestureEnabler
                // re-enables the edge-swipe pop that hiding disables.
                NavigationStack(path: $store.chatPath) {
                    HomeTabView()
                        .toolbar(.hidden, for: .navigationBar)
                        .navigationDestination(for: AppStore.ChatRoute.self) { route in
                            Group {
                                switch route {
                                case .session(let sessionId):
                                    ChatScreen(
                                        host: store.transcriptHost(for: sessionId),
                                        store: store.chatStore(for: sessionId)
                                    )
                                    // The cross-session remount contract of the
                                    // shared webview depends on this keying.
                                    .id(sessionId)
                                case .archived:
                                    ArchivedScreen()
                                case .cronGroup(let jobId):
                                    CronGroupScreen(jobId: jobId)
                                case .cronJobs:
                                    CronJobsScreen()
                                case .deckRecycle:
                                    DeckRecycleScreen()
                                case .projectBoard(let projectId):
                                    ProjectBoardScreen(projectId: projectId, store: store.projectsStore)
                                case .projectIssue(let issue):
                                    ProjectIssueScreen(visit: store.issueVisit(for: issue))
                                case .newProject:
                                    NewProjectScreen()
                                case .newIssue(let projectId, let status):
                                    NewIssueScreen(
                                        projectId: projectId, initialStatus: status)
                                }
                            }
                            .toolbar(.hidden, for: .navigationBar)
                            .navigationBarBackButtonHidden(true)
                        }
                }
            }

            // The logout confirm mounts HERE, above the NavigationStack: the
            // only layer that dims and hit-blocks the whole shell — Liquid
            // Glass tab bar included — so a tab switch can't orphan it.
            if store.confirmLogout {
                ConfirmDialog(
                    titleKey: "connected.logout",
                    bodyKey: "connected.logoutConfirm",
                    commitKey: "connected.logout",
                    onCancel: dismissLogoutConfirm,
                    onConfirm: {
                        dismissLogoutConfirm()
                        Task { await store.logout() }
                    }
                )
                .zIndex(1)
            }

            // The swipe-delete confirm shares the logout confirm's hosting: the
            // app root is the one layer that dims and hit-blocks the whole shell.
            if let sessionId = store.confirmDeleteSession {
                ConfirmDialog(
                    titleKey: "list.deleteConfirmTitle",
                    bodyKey: "list.deleteConfirmBody",
                    commitKey: "list.delete",
                    onCancel: dismissDeleteConfirm,
                    onConfirm: {
                        dismissDeleteConfirm()
                        withAnimation {
                            store.requestDelete(sessionId)
                        }
                    }
                )
                .zIndex(1)
            }

            if let pending = store.confirmCancelIssue {
                ConfirmDialog(
                    titleKey: "board.cancelConfirmTitle",
                    bodyKey: "board.cancelConfirmBody",
                    bodyArg: String(pending.number),
                    commitKey: "board.cancelIssue",
                    onCancel: dismissCancelIssueConfirm,
                    onConfirm: {
                        dismissCancelIssueConfirm()
                        Task {
                            await store.projectsStore.setCancelled(
                                board: pending.projectId, issue: pending.number, true)
                        }
                    }
                )
                .zIndex(1)
            }

            // The rename editor shares that hosting for the same two reasons,
            // and needs it for a third: it is the one dialog here that raises a
            // keyboard, and mounting it at the root is what lets it own the
            // avoidance instead of the shell sliding under the scrim.
            if let pending = store.renameSession {
                RenameDialog(
                    seed: pending.seed,
                    onCancel: dismissRename,
                    onCommit: { title in
                        dismissRename()
                        store.requestRename(pending.sessionId, title: title)
                    }
                )
                // Keyed by session: reopening the editor on another row must
                // rebuild the field, not carry the last row's draft into it.
                .id(pending.sessionId)
                .zIndex(1)
            }

            // Deleting the JOB is the mirror of deleting its group, so the body
            // states the opposite blast radius: the schedule stops, the history
            // stays. It names the job, because the list is the only place these
            // are told apart and this dialog covers it.
            if let pending = store.confirmDeleteCronJob {
                ConfirmDialog(
                    titleKey: "cronJobs.deleteConfirmTitle",
                    bodyKey: "cronJobs.deleteConfirmBody",
                    bodyArg: pending.name,
                    commitKey: "list.delete",
                    onCancel: dismissDeleteCronJobConfirm,
                    onConfirm: {
                        dismissDeleteCronJobConfirm()
                        withAnimation {
                            store.requestDeleteCronJob(pending.jobId)
                        }
                    }
                )
                .zIndex(1)
            }

            // A cron group's delete clears its execution records — never the job
            // — so the body says so, and counts them: the group is a view over
            // its fires, and "delete" hiding N conversations is exactly the kind
            // of blast radius a confirm exists to state out loud.
            if let pending = store.confirmDeleteCronGroup {
                ConfirmDialog(
                    titleKey: "list.deleteGroupConfirmTitle",
                    bodyKey: "list.deleteGroupConfirmBody",
                    bodyArg: String(pending.memberIds.count),
                    commitKey: "list.delete",
                    onCancel: dismissDeleteCronGroupConfirm,
                    onConfirm: {
                        dismissDeleteCronGroupConfirm()
                        withAnimation {
                            store.requestDeleteGroup(pending.memberIds)
                        }
                    }
                )
                .zIndex(1)
            }
        }
        .sheet(isPresented: $store.scanPresented) {
            ScanView()
        }
    }

    private func dismissLogoutConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmLogout = false
        }
    }

    private func dismissDeleteConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmDeleteSession = nil
        }
    }

    private func dismissCancelIssueConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmCancelIssue = nil
        }
    }

    private func dismissRename() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.renameSession = nil
        }
    }

    private func dismissDeleteCronJobConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmDeleteCronJob = nil
        }
    }

    private func dismissDeleteCronGroupConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmDeleteCronGroup = nil
        }
    }
}
