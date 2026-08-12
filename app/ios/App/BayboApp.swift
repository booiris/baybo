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
                // Composer spools a kill, a crash or a jetsam left behind: a
                // strip's own tiles reclaim theirs, but only while the process
                // that staged them lives. Launch is the one moment nothing of
                // this run can be staged yet.
                .task { await StagedAttachment.sweepAbandonedSpools() }
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active {
                store.didBecomeActive()
            } else {
                // `!= .active`, unlike the leg teardown below: a composer draft
                // costs a few hundred bytes to check-point and the phases that
                // bounce back (a banner, Control Centre, the app-switcher peek)
                // are also the ones a user force-quits from. Flushing on
                // `.background` alone would lose the last keystrokes of every
                // message abandoned that way.
                store.willLeaveForeground()
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
