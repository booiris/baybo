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
                // The chat list is home; a session rides the path. Both screens
                // draw their own chrome, so the system bar stays hidden — the
                // interactive pop gesture that hiding disables is re-enabled by
                // ChatScreen's PopGestureEnabler.
                NavigationStack(path: $store.chatPath) {
                    ChatListScreen()
                        .toolbar(.hidden, for: .navigationBar)
                        .navigationDestination(for: String.self) { sessionId in
                            ChatScreen(store: store.chatStore(for: sessionId))
                                .id(sessionId) // a new session gets a fresh webview
                                .toolbar(.hidden, for: .navigationBar)
                                .navigationBarBackButtonHidden(true)
                        }
                }
            }
        }
        .sheet(isPresented: $store.scanPresented) {
            ScanView()
        }
    }
}
