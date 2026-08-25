import Combine
import SwiftUI

/// The bound app's home: a NATIVE `TabView` (Liquid Glass bar on iOS 26+, the
/// classic system bar on 18–25) over
/// five sections — Deck · Projects · Chats · Settings · Search. The chat push
/// lives on the
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
    /// Only for the Chats badge — the same rows the app icon counts.
    @ObservedObject private var index = SessionIndex.shared
    /// The Projects badge, pulled forward rather than read through `store`.
    ///
    /// `AppStore.projectsStore` is a nested `ObservableObject`, so its changes
    /// do NOT republish `AppStore` and would leave this badge frozen at
    /// whatever it read on first paint — a bug the demo fixture cannot show,
    /// because that seeds before the first render. `@ObservedObject` is not
    /// available for it (the instance comes from the environment), and making
    /// `AppStore` forward the whole `objectWillChange` would repaint the chat
    /// list on every board fetch. One published Int is the narrow version.
    @State private var projectsWaiting = 0

    var body: some View {
        TabView(selection: searchAwareSelection) {
            ForEach(AppStore.HomeTab.allCases, id: \.self) { tab in
                // `role` is what separates search from the rest: on iOS 26 the
                // system lifts a `.search` tab OUT of the glass pill and floats
                // it as its own trailing circle, which is the shape this bar is
                // meant to have. On 18–25 the role is honoured as an ordinary
                // tab item, so nothing needs a version branch here.
                Tab(lang.t(tab.labelKey), systemImage: tab.icon, value: tab, role: tab.role) {
                    content(for: tab)
                }
                // A count rather than a dot, and the same shape on both tabs
                // that carry one. It reads as a promise the press can keep:
                // Chats opens a list whose rows each discharge part of it, and
                // Projects opens the cards, whose rows carry the same per-board
                // numbers. Nothing pushes a board, so this only moves while the
                // app is foreground — which is the honest state of the feature.
                .badge(badge(for: tab))
            }
        }
        .tint(Theme.ink)
        // The shell must not ride the keyboard. Two things type into it and
        // NEITHER wants that: the rename editor floats at the app root and owns
        // its own avoidance (without this opt-out the whole shell — the glass tab
        // bar first — slides up behind its scrim), and the search tab pins its
        // field to the top, where the keyboard cannot reach it. Search's RESULTS
        // do run under the keyboard, which is what a scroll view is for, and it
        // dismisses interactively. The chat composer is out of scope entirely: it
        // lives inside a pushed `ChatScreen` that covers this whole view.
        .ignoresSafeArea(.keyboard)
        // A `Published` publisher replays its current value on subscribe, so
        // this also seeds the badge rather than only tracking it from here on.
        .onReceive(store.projectsStore.$attention) { attention in
            projectsWaiting = attention.values.reduce(0) {
                $0 + Int($1.approvals + $1.failed + $1.unread)
            }
        }
        #if DEBUG
            .task { await demoTabCycleIfRequested() }
        #endif
    }

    /// The tab selection, with ONE special case: entering or leaving search
    /// switches without animation.
    ///
    /// The native bar's hide/show is animated, and for the length of that
    /// animation it is on screen at the same time as the search field replacing
    /// it — the four-tab pill and its search circle fading out under a field
    /// already opening. Disabling the transaction removes the window entirely:
    /// the bar is gone by the time the field appears.
    ///
    /// Scoped to the search edges, so every other tab switch keeps the system's
    /// Liquid Glass selection morph.
    private var searchAwareSelection: Binding<AppStore.HomeTab> {
        Binding(
            get: { store.homeTab },
            set: { next in
                guard next == .search || store.homeTab == .search else {
                    store.homeTab = next
                    return
                }
                var instant = Transaction()
                instant.disablesAnimations = true
                withTransaction(instant) { store.homeTab = next }
            })
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
            // New project lives in the header, beside the wordmark — the same
            // slot Chats puts compose in. It was a dashed card at the FOOT of
            // the list, which put the one thing you cannot reach any other way
            // behind however many boards you happen to have.
            section(
                action: {
                    Haptics.tap()
                    store.openNewProject()
                }, icon: "plus", labelKey: "projects.new"
            ) {
                ProjectsScreen(store: store.projectsStore)
            }
        case .search:
            // No `section` wrapper: this screen's field IS its header, and the
            // shared wordmark stacked above a search field is one header too many.
            //
            // NOT `.searchable`, and NOT the iOS 26 "tab bar morphs into a
            // search field" treatment — that needs a navigation bar to host the
            // field, and THIS SHELL HAS NONE ANYWHERE.
            //
            // `RootView` applies `.toolbar(.hidden, for: .navigationBar)` to
            // `HomeTabView`, and that propagates into nested stacks. Measured on
            // 26.5: `.searchable` on the tab content, on an inner NavigationStack
            // with the bar hidden, with the bar forced `.visible`, and on the
            // TabView itself — the field appeared in the accessibility tree in
            // none of them. The decisive probe was `.navigationTitle` on that
            // inner stack: it did not render either, so the bar itself never
            // exists to be relocated from.
            //
            // Reaching the native morph therefore means dropping the shell-wide
            // hide and hiding per destination instead. That is a `RootView`
            // refactor touching every pushed screen's chrome, not a modifier on
            // this tab. It has nothing to do with the deployment target:
            // `#available(iOS 26.0, *)` was TRUE in every one of those runs.
            // `.toolbar(.hidden, for: .tabBar)` belongs on the tab's CONTENT,
            // never on the TabView — on the TabView it silently does nothing,
            // and the bar merely sat UNDER the keyboard with ~37pt of itself
            // protruding below it (measured: keyboard ends at y=816, the bar runs
            // to y=853). That protruding strip was the dark band under the
            // keyboard. `isHittable` cannot catch this — the keyboard covering
            // the bar makes it unhittable whether or not it is hidden.
            SearchScreen()
                .toolbar(.hidden, for: .tabBar)
        }
    }


    /// What a tab's badge counts, or `0` for no badge.
    ///
    /// Chats reuses the very number the app icon already carries, so the two
    /// cannot disagree by construction. Projects sums what every live board is
    /// waiting on — approvals, failed runs, unread — which is exactly the set
    /// `/projects/attention` reports and deliberately excludes runs the daily
    /// ceiling is holding: a hold is a standing condition, not an event, and a
    /// badge that cannot be cleared by acting is the one this feature already
    /// got a complaint about on the web.
    private func badge(for tab: AppStore.HomeTab) -> Int {
        switch tab {
        case .chats: BadgeCenter.total(index.rows)
        case .projects: projectsWaiting
        case .deck, .settings, .search: 0
        }
    }

    /// Non-chat sections: content under the shared wordmark header.
    private func section<Content: View>(
        action: (() -> Void)? = nil, icon: String = "plus", labelKey: String = "",
        @ViewBuilder _ content: () -> Content
    ) -> some View {
        ZStack(alignment: .top) {
            content()
            HomeHeaderView(onAction: action, actionIcon: icon, actionLabelKey: labelKey)
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
        // The system supplies the glyph for a `.search` role tab; naming it
        // here keeps the `Tab` initializer uniform and matches what it draws.
        case .search: return "magnifyingglass"
        }
    }

    /// `.search` for the search tab, `nil` for the rest. The role is the whole
    /// reason that one renders detached from the pill.
    var role: TabRole? {
        self == .search ? .search : nil
    }

    var labelKey: String {
        switch self {
        case .deck: return "home.tab.deck"
        case .projects: return "home.tab.projects"
        case .chats: return "home.tab.chats"
        case .settings: return "home.tab.settings"
        case .search: return "search.title"
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
    /// The trailing glass circle: this section's ONE action. Chats mints a
    /// conversation, Projects a board. The glyph and label ride along rather
    /// than being hardcoded here — the button is the header's, what it does is
    /// the section's.
    var onAction: (() -> Void)? = nil
    var actionIcon: String = "square.and.pencil"
    var actionLabelKey: String = "list.newChat"
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
                        .glassSurface(interactive: true, in: .circle)
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("list.menu")))
                        Spacer()
                    }
                }

                if let onAction {
                    HStack {
                        Spacer()
                        Button(action: onAction) {
                            Image(systemName: actionIcon)
                                .font(.system(size: 16, weight: .medium))
                                .foregroundStyle(Theme.ink)
                                .frame(width: 45, height: 45)
                        }
                        .glassSurface(interactive: true, in: .circle)
                        .accessibilityIdentifier("header-action")
                        .accessibilityLabel(Text(verbatim: Lang.shared.t(actionLabelKey)))
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
