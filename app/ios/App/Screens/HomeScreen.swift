import SwiftUI

/// The bound app's home: a NATIVE iOS 26 `TabView` (Liquid Glass tab bar) over
/// four sections — Agents · Projects · Chats · Settings. Only Chats and Settings
/// have real screens; Agents/Projects are placeholders. The chat push lives on the
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
        case .deck:
            section { DeckScreen() }
        case .projects:
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
            let order: [AppStore.HomeTab] = [.chats, .deck, .settings, .projects, .deck, .chats]
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
        case .deck: return "rectangle.stack"
        case .projects: return "square.stack.3d.up"
        case .chats: return "message"
        case .settings: return "gearshape"
        }
    }

    var labelKey: String {
        switch self {
        case .deck: return "home.tab.deck"
        case .projects: return "home.tab.projects"
        case .chats: return "home.tab.chats"
        case .settings: return "home.tab.settings"
        }
    }
}

/// The shared paper-veil header: the centered wordmark, optionally flanked by a
/// glass compose circle on the trailing edge and a glass ☰ menu circle on the
/// leading edge (both Chats only). Reuses the chat header's veil so the
/// screens' fades can't drift apart. A transient compose-failure line hangs
/// under it.
struct HomeHeaderView: View {
    var notice: String? = nil
    var onCompose: (() -> Void)? = nil
    /// Chats only: the ☰ menu's entries — push the archived list, and the live
    /// scheduled jobs. The menu renders iff `onArchived` is set; both entries
    /// belong to the same Chats-only surface.
    var onArchived: (() -> Void)? = nil
    var onCronJobs: (() -> Void)? = nil
    /// Chats only: pull-to-refresh feedback rendered BESIDE the wordmark (as an
    /// overlay, so it never shifts it). Shown only while the refresh runs — after
    /// the pull rebounds — never during the drag.
    var isRefreshing: Bool = false

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
                    .overlay(alignment: .trailing) {
                        RefreshRing(isRefreshing: isRefreshing)
                            .offset(x: 18)
                    }

                if let onArchived {
                    HStack {
                        Menu {
                            Button(action: onArchived) {
                                Label(
                                    Lang.shared.t("list.menuArchived"),
                                    systemImage: "archivebox")
                            }
                            if let onCronJobs {
                                Button(action: onCronJobs) {
                                    Label(
                                        Lang.shared.t("list.menuCronJobs"),
                                        systemImage: "alarm")
                                }
                            }
                        } label: {
                            Image(systemName: "line.3.horizontal")
                                .font(.system(size: 16, weight: .medium))
                                .foregroundStyle(Theme.ink)
                                .frame(width: 45, height: 45)
                        }
                        .glassEffect(.regular.interactive(), in: .circle)
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("list.menu")))
                        Spacer()
                    }
                }

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

/// The pull-to-refresh indicator beside the wordmark: a rotating open ring
/// (not the system spokes). It appears ONLY once the refresh is running — after
/// the pull is released and the list rebounds — never during the drag itself.
/// The spin only runs while refreshing so an idle list isn't animating.
private struct RefreshRing: View {
    var isRefreshing: Bool
    @State private var angle: Double = 0

    var body: some View {
        Circle()
            .trim(from: 0, to: 0.72)
            .stroke(Theme.inkSoft, style: StrokeStyle(lineWidth: 1, lineCap: .round))
            .frame(width: 10, height: 10)
            .rotationEffect(.degrees(angle))
            .opacity(isRefreshing ? 1 : 0)
            .animation(.easeOut(duration: 0.2), value: isRefreshing)
            .onChange(of: isRefreshing) { _, on in spin(on) }
            .accessibilityLabel(Text(verbatim: Lang.shared.t("list.refresh")))
            .accessibilityHidden(!isRefreshing)
    }

    private func spin(_ on: Bool) {
        if on {
            angle = 0
            withAnimation(.linear(duration: 0.8).repeatForever(autoreverses: false)) {
                angle = 360
            }
        } else {
            withAnimation(.easeOut(duration: 0.15)) { angle = 0 }
        }
    }
}
