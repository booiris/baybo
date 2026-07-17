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
                    destructiveKey: "connected.logout",
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
                    destructiveKey: "list.delete",
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

            // A cron group's delete clears its execution records — never the job
            // — so the body says so, and counts them: the group is a view over
            // its fires, and "delete" hiding N conversations is exactly the kind
            // of blast radius a confirm exists to state out loud.
            if let pending = store.confirmDeleteCronGroup {
                ConfirmDialog(
                    titleKey: "list.deleteGroupConfirmTitle",
                    bodyKey: "list.deleteGroupConfirmBody",
                    bodyArg: String(pending.memberIds.count),
                    destructiveKey: "list.delete",
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

    private func dismissDeleteCronGroupConfirm() {
        withAnimation(ConfirmDialog.exitMotion) {
            store.confirmDeleteCronGroup = nil
        }
    }
}
