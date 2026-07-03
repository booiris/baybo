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
            case .chat(let sessionId):
                ChatScreen(sessionId: sessionId)
                    .id(sessionId) // a new session gets a fresh store/webview
            }
        }
        .sheet(isPresented: $store.scanPresented) {
            ScanView()
        }
    }
}
