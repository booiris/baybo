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
            // `.background`, not `!= .active`: suspension always passes through
            // active → inactive → background, but a notification banner, Control
            // Centre, an incoming call, Face ID and the app-switcher peek only
            // reach `.inactive` and bounce back — the sockets are untouched there,
            // and throwing away warm legs would just make the next call redial.
            //
            // Synchronous by design (no `Task`): the epoch bump has to land before
            // anything spawned later in this scene-phase turn can take a leg the
            // suspend is about to kill.
            if phase == .background {
                Baybo.client.relayInvalidateApiLegs()
            }
        }
    }
}
