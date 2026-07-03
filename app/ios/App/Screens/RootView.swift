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
                    switch store.landingView {
                    case .menu:
                        LandingView()
                    case .direct:
                        DirectLoginView()
                    }
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
