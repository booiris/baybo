import SwiftUI

@main
struct BayboApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var store = AppStore()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(store)
                .preferredColorScheme(.light) // the design system is light-only
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active {
                store.didBecomeActive()
            }
        }
    }
}
