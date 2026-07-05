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
                        .navigationDestination(for: String.self) { sessionId in
                            ChatScreen(store: store.chatStore(for: sessionId))
                                .id(sessionId)  // a new session gets a fresh webview
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
}
