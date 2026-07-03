import SwiftUI

/// The chat screen: transcript webview filling the space, native header veil
/// pinned over its top, native composer docked below. Because the composer is
/// native, the keyboard never pans web content — SwiftUI's safe-area handling
/// moves the dock and the webview resizes; the web bundle's ResizeObserver
/// holds the newest edge through that resize.
struct ChatScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @StateObject private var store: ChatStore
    @StateObject private var bridge: TranscriptBridge
    @Environment(\.scenePhase) private var scenePhase
    @State private var confirmLogout = false

    init(sessionId: String) {
        let store = ChatStore(sessionId: sessionId)
        _store = StateObject(wrappedValue: store)
        _bridge = StateObject(wrappedValue: TranscriptBridge(store: store))
    }

    var body: some View {
        ZStack(alignment: .top) {
            TranscriptWebView(bridge: bridge)
                .ignoresSafeArea(edges: .top)

            ChatHeaderView(connState: store.connState) {
                confirmLogout = true
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ComposerView(store: store)
        }
        .background(Theme.paper)
        .onAppear {
            store.connect()
            #if DEBUG
                store.startDemoFramesIfRequested()
            #endif
        }
        .onDisappear {
            store.teardown()
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active {
                store.scheduleReconnect()
            }
        }
        .confirmationDialog(
            Text(verbatim: Lang.shared.t("connected.logoutConfirm")),
            isPresented: $confirmLogout, titleVisibility: .visible
        ) {
            Button(role: .destructive) {
                Task { await appStore.logout() }
            } label: {
                Text(verbatim: Lang.shared.t("connected.logout"))
            }
        }
    }
}

/// The paper-veil header: a translucent white gradient holding through the
/// status-bar + bar zone and easing out over a ramp (the smoothstep stops from
/// `native_header.rs`), with the connection dot + label and the logout button.
/// The veil ignores touches so scrolls in the ramp reach the thread.
struct ChatHeaderView: View {
    let connState: ChatStore.ConnState
    let onLogout: () -> Void

    /// The fade shape from the old native header: two full stops hold the solid
    /// zone, then a smoothstep tail (`FADE_ALPHAS` × 0.75 peak — a fraction
    /// stronger than the web veil because there is no blur underneath).
    private static let fadeAlphas: [Double] = [1.0, 1.0, 0.9, 0.65, 0.35, 0.1, 0.0]
    private static let veilPeakAlpha = 0.75
    private static let barHeight: CGFloat = 44
    private static let ramp: CGFloat = 36

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Circle()
                    .fill(dotFill)
                    .overlay(
                        Circle().strokeBorder(
                            Theme.inkSoft, lineWidth: connState == .connecting ? 1 : 0)
                    )
                    .frame(width: 9, height: 9)
                Text(label)
                    .font(Theme.mono(13))
                    .foregroundStyle(connState == .offline ? Theme.err : Theme.inkSoft)
                Spacer()
                Button(action: onLogout) {
                    Image(systemName: "rectangle.portrait.and.arrow.right")
                        .font(.system(size: 16))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 44, height: 44)
                }
            }
            .padding(.leading, 20)
            .frame(height: Self.barHeight)
        }
        .frame(maxWidth: .infinity, alignment: .top)
        .background(alignment: .top) {
            LinearGradient(
                stops: gradientStops,
                startPoint: .top,
                endPoint: .bottom
            )
            .frame(height: Self.barHeight + Self.ramp)
            .padding(.top, -200) // extend the solid zone up under the status bar
            .padding(.bottom, -Self.ramp)
            .allowsHitTesting(false)
            .ignoresSafeArea(edges: .top)
        }
    }

    private var gradientStops: [Gradient.Stop] {
        // Solid through the bar, smoothstep tail across the ramp.
        let total = Self.barHeight + Self.ramp
        let solid = Self.barHeight / total
        let tailFractions: [CGFloat] = [0.2, 0.4, 0.6, 0.8, 1.0]
        var stops: [Gradient.Stop] = [
            .init(color: .white.opacity(Self.veilPeakAlpha), location: 0),
            .init(color: .white.opacity(Self.veilPeakAlpha), location: solid),
        ]
        for (alpha, fraction) in zip(Self.fadeAlphas.dropFirst(2), tailFractions) {
            stops.append(
                .init(
                    color: .white.opacity(alpha * Self.veilPeakAlpha),
                    location: solid + fraction * (1 - solid)
                ))
        }
        return stops
    }

    private var dotFill: Color {
        switch connState {
        case .connected: return Theme.ink
        case .connecting: return .clear
        case .offline: return Theme.err
        }
    }

    private var label: String {
        switch connState {
        case .connected: return Lang.shared.t("chat.connected")
        case .connecting: return Lang.shared.t("chat.connecting")
        case .offline: return Lang.shared.t("chat.offline")
        }
    }
}
