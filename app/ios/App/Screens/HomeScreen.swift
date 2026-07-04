import SwiftUI

/// The bound app's home: a NATIVE iOS 26 `TabView` (Liquid Glass tab bar) over
/// four sections — Agents · Works · Chats · Settings. Only Chats and Settings
/// have real screens; Agents/Works are placeholders. The chat push lives on the
/// OUTER `NavigationStack` in `RootView` that WRAPS this TabView, so a pushed
/// `ChatScreen` covers the whole shell (tab bar included) and both slide back
/// together on pop — an inner-stack `.toolbar(.hidden, for: .tabBar)` instead
/// pops the bar back in abruptly after the transition.
///
/// Selection binds to `AppStore.homeTab`; compose / push-tap routing force
/// `.chats`. Monochrome: `.tint(Theme.ink)` colours the SELECTED item's glyph +
/// label (the HIG blesses a monochromatic tab bar); the selection capsule is
/// the system Liquid Glass material (no public API recolours it, and none is
/// wanted — neutral glass is on-brand). Compose lives in the Chats header's
/// top-right, not the bar — the native tab bar is for navigation, not actions.
struct HomeTabView: View {
    @EnvironmentObject private var store: AppStore
    @ObservedObject private var lang = Lang.shared

    var body: some View {
        TabView(selection: $store.homeTab) {
            ForEach(AppStore.HomeTab.allCases, id: \.self) { tab in
                Tab(lang.t(tab.labelKey), systemImage: tab.icon, value: tab) {
                    content(for: tab)
                }
            }
        }
        .tint(Theme.ink)
        #if DEBUG
            .task { await demoTabCycleIfRequested() }
        #endif
    }

    @ViewBuilder private func content(for tab: AppStore.HomeTab) -> some View {
        switch tab {
        case .chats:
            // The chat list draws its own header; a tapped row pushes
            // `ChatScreen` on the OUTER NavigationStack (which wraps this whole
            // TabView in `RootView`), so the detail covers the tab bar and both
            // reveal together on pop. Hiding the bar via `.toolbar(.hidden, for:
            // .tabBar)` from an inner stack instead pops it back in abruptly
            // AFTER the transition (the "bar missing then appears" glitch).
            ChatListScreen()
        case .settings:
            section { SettingsScreen() }
        case .agents, .works:
            section { PlaceholderScreen(icon: tab.icon, titleKey: tab.labelKey) }
        }
    }

    /// Non-chat sections: content under the shared wordmark header.
    private func section<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        ZStack(alignment: .top) {
            content()
            HomeHeaderView()
        }
        .background(Theme.paper)
    }

    #if DEBUG
        /// `-baybo-demo-tabs`: cycle the selection so the native Liquid Glass
        /// switch is recordable headlessly (`simctl io recordVideo` + ffmpeg).
        private func demoTabCycleIfRequested() async {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-tabs") else { return }
            let order: [AppStore.HomeTab] = [.chats, .agents, .settings, .works, .agents, .chats]
            while !Task.isCancelled {
                for tab in order {
                    try? await Task.sleep(for: .milliseconds(1000))
                    store.homeTab = tab
                }
            }
        }
    #endif
}

/// Presentation for the tab sections — kept beside the shell that draws them,
/// apart from `AppStore.HomeTab` (which is pure navigation state).
extension AppStore.HomeTab {
    /// A line-weight SF Symbol tuned for the tab bar.
    var icon: String {
        switch self {
        case .agents: return "sparkles"
        case .works: return "square.grid.2x2"
        case .chats: return "bubble.left.and.bubble.right"
        case .settings: return "gearshape"
        }
    }

    var labelKey: String {
        switch self {
        case .agents: return "home.tab.agents"
        case .works: return "home.tab.works"
        case .chats: return "home.tab.chats"
        case .settings: return "home.tab.settings"
        }
    }
}

/// The shared paper-veil header: the centered wordmark, optionally flanked by a
/// glass compose circle on the trailing edge (Chats only). Reuses the chat
/// header's veil so the screens' fades can't drift apart. A transient
/// compose-failure line hangs under it.
struct HomeHeaderView: View {
    var notice: String? = nil
    var onCompose: (() -> Void)? = nil

    private static let barHeight: CGFloat = 46

    var body: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(verbatim: "Baybo")
                    .font(Theme.mono(17))
                    .textCase(.uppercase)
                    .kerning(5)
                    .padding(.leading, 5)
                    .foregroundStyle(Theme.ink)

                if let onCompose {
                    HStack {
                        Spacer()
                        Button(action: onCompose) {
                            Image(systemName: "square.and.pencil")
                                .font(.system(size: 16, weight: .medium))
                                .foregroundStyle(Theme.ink)
                                .frame(width: 45, height: 45)
                        }
                        .glassEffect(.regular.interactive(), in: .circle)
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("list.newChat")))
                    }
                }
            }
            .padding(.horizontal, 24)
            .frame(height: Self.barHeight)

            if let notice {
                Text(verbatim: notice)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.err)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
            }
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }
}
