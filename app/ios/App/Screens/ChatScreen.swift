import SwiftUI

/// The chat screen: transcript webview filling the space, native header veil
/// pinned over its top, native composer docked below. Because the composer is
/// native, the keyboard never pans web content — SwiftUI's safe-area handling
/// moves the dock and the webview resizes; the web bundle's ResizeObserver
/// holds the newest edge through that resize.
struct ChatScreen: View {
    @ObservedObject private var store: ChatStore
    @StateObject private var bridge: TranscriptBridge
    @Environment(\.dismiss) private var dismiss

    init(store: ChatStore) {
        _store = ObservedObject(wrappedValue: store)
        _bridge = StateObject(wrappedValue: TranscriptBridge(store: store))
    }

    var body: some View {
        ZStack(alignment: .top) {
            // Full-bleed on BOTH vertical edges, keyboard included: a webview
            // whose frame tracks the keyboard can't animate with it (the web
            // process relayouts once, asynchronously, at the final size — the
            // transcript would sit still and snap at the end). The frame stays
            // fixed; the thread instead pads its bottom by the measured
            // composer/keyboard obstruction fed over the bridge, and animates
            // that padding web-side so content slides with the keyboard.
            TranscriptWebView(bridge: bridge)
                .ignoresSafeArea(.all, edges: [.top, .bottom])
                // Fade the transcript in once it has painted (bridge `shown`),
                // rather than popping the content in as the screen slides on.
                // Opacity 0 shows the paper background behind it, seamless with
                // the slide-in; the content emerges as it settles.
                .opacity(bridge.contentVisible ? 1 : 0)
                .animation(.easeOut(duration: 0.3), value: bridge.contentVisible)

            ChatHeaderView(connState: store.connState) {
                dismiss()
            }
        }
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .safeAreaInset(edge: .bottom, spacing: 0) {
            VStack(spacing: 12) {
                if bridge.jumpVisible {
                    Button {
                        bridge.jumpToLatest()
                    } label: {
                        Image(systemName: "arrow.down")
                            .font(.system(size: 17, weight: .medium))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 44, height: 44)
                    }
                    .glassEffect(.regular.interactive(), in: .circle)
                    .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.jumpToLatest")))
                    .transition(.scale(scale: 0.7).combined(with: .opacity))
                }
                ComposerView(store: store)
                    .onGeometryChange(for: CGFloat.self) { proxy in
                        proxy.frame(in: .global).minY
                    } action: { minY in
                        // The composer's own geometry is the one signal that
                        // tracks BOTH the keyboard it rides and its own growth
                        // (notice line, staged strip, multiline field). The
                        // bridge converts to the covered strip against the
                        // WINDOW bottom. Measured on the COMPOSER, not the
                        // wrapping stack: the jump button popping in must
                        // never inflate the web-side inset.
                        bridge.setComposerTop(minY)
                    }
            }
            .animation(.easeOut(duration: 0.16), value: bridge.jumpVisible)
        }
        .background(Theme.paper)
        .onAppear {
            bridge.attach()
            store.connectIfNeeded()
            #if DEBUG
                store.startDemoFramesIfRequested()
                bridge.startDemoJumpIfRequested()
                startDemoBackIfRequested()
            #endif
        }
        .onDisappear {
            bridge.detach()
        }
    }

    #if DEBUG
        /// `-baybo-demo-back`: pop to the chat list 6s in — the back function's
        /// headless round trip (pair with `-baybo-open-chat`; screenshot the
        /// chat at ~3s and the list at ~8s). Exercises the programmatic pop,
        /// not the gesture physics — those need a device/manual pass.
        private func startDemoBackIfRequested() {
            guard ProcessInfo.processInfo.arguments.contains("-baybo-demo-back") else { return }
            Task { @MainActor in
                try? await Task.sleep(for: .seconds(6))
                dismiss()
            }
        }
    #endif
}

/// The paper-veil header: a translucent white gradient holding solid through
/// the status bar and easing out to clear at the connection-status capsule's
/// bottom edge, with the centered status capsule flanked by the glass back
/// button. The veil ignores touches so scrolls beneath it reach the thread.
struct ChatHeaderView: View {
    let connState: ChatStore.ConnState
    let onBack: () -> Void

    private static let veilPeakAlpha = 0.8
    private static let barHeight: CGFloat = 46
    /// Solid → clear smoothstep the veil fades through, below its solid
    /// status-bar zone (the composer veil's grammar, mirrored to the top).
    private static let rampAlphas: [Double] = [1.0, 0.9, 0.65, 0.35, 0.1, 0.0]

    var body: some View {
        ZStack {
            // Centered connection status — a liquid-glass capsule.
            HStack(spacing: 6) {
                Circle()
                    .fill(dotFill)
                    .overlay(
                        Circle().strokeBorder(
                            Theme.inkSoft, lineWidth: connState == .connecting ? 1 : 0)
                    )
                    .frame(width: 8, height: 8)
                Text(label)
                    .font(Theme.mono(13))
                    .foregroundStyle(connState == .offline ? Theme.err : Theme.inkSoft)
            }
            .padding(.horizontal, 18)
            .frame(height: 42)
            .glassEffect(.regular, in: Capsule())

            // The flanking glass back circle (logout moved to the chat list's
            // header — leaving the account lives one level up now). Semibold
            // glyph so the ink reads as solid black over the bright glass.
            HStack {
                Button(action: onBack) {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassEffect(.regular.interactive(), in: .circle)
                .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.back")))

                Spacer()
            }
        }
        .padding(.horizontal, 24)
        .frame(height: Self.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    /// White paper veil: one fade that floods up over the status bar (so its
    /// coordinate space is status-bar + bar) and holds solid through the
    /// status bar, then smoothsteps to clear at the bar's bottom — i.e. the
    /// connection-status capsule's bottom edge. Nothing below the bar. The
    /// gradient fill itself carries `ignoresSafeArea(.top)`; nesting the flood
    /// inside a `.background` clips it (the parent doesn't ignore the inset).
    private var veil: some View {
        LinearGradient(stops: Self.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    /// Solid through the status bar (~`solidFraction` of the flooded height on
    /// this device), then the smoothstep tail to clear at the bar's bottom.
    /// Shared with the chat list's header so the two screens' veils can't
    /// drift apart.
    static var veilStops: [Gradient.Stop] {
        let solidFraction: CGFloat = 0.55
        var stops: [Gradient.Stop] = [
            .init(color: Theme.paper.opacity(veilPeakAlpha), location: 0)
        ]
        for (idx, alpha) in rampAlphas.enumerated() {
            let frac = solidFraction + (1 - solidFraction) * CGFloat(idx) / CGFloat(rampAlphas.count - 1)
            stops.append(.init(color: Theme.paper.opacity(alpha * veilPeakAlpha), location: frac))
        }
        return stops
    }

    private var dotFill: Color {
        switch connState {
        case .draft: return Theme.ink
        case .connected: return Theme.ink
        case .connecting: return .clear
        case .offline: return Theme.err
        }
    }

    private var label: String {
        switch connState {
        case .draft: return Lang.shared.t("chat.draft")
        case .connected: return Lang.shared.t("chat.connected")
        case .connecting: return Lang.shared.t("chat.connecting")
        case .offline: return Lang.shared.t("chat.offline")
        }
    }
}
