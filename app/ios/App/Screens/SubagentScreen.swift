import SwiftUI
import WebKit

/// A subagent child's read-only transcript, and the recursive stack it lives in.
///
/// It renders in its OWN `WKWebView`, built when the browser opens and torn
/// down when it closes, so the singleton `TranscriptHost` keeps serving the
/// parent conversation untouched. That is not a first: `DeckHost` is a
/// permanent second webview and `ImageViewer`'s `SvgImageWebView` already lives
/// in a cover over the live transcript. The "one transcript webview" rule is a
/// LATENCY decision (cold-booting the bundle per chat push), not a memory one.
struct SubagentBrowser: View {
    let root: ChatSubagentSummary
    /// The conversation that spawned `root` — where its status is re-read.
    let parentSessionId: String
    var client: any BayboClientProtocol = Baybo.client
    let onClose: () -> Void

    @State private var path: [SubagentRoute] = []

    var body: some View {
        NavigationStack(path: $path) {
            SubagentScreen(
                summary: root, parentSessionId: parentSessionId, client: client,
                onClose: onClose
            )
            .toolbar(.hidden, for: .navigationBar)
            .navigationDestination(for: SubagentRoute.self) { route in
                SubagentScreen(
                    summary: route.summary, parentSessionId: route.parentSessionId,
                    client: client, onClose: onClose
                )
                .toolbar(.hidden, for: .navigationBar)
                .navigationBarBackButtonHidden(true)
            }
        }
        .environment(\.subagentPath, $path)
    }
}

/// One level of the drill-down. A child and the parent it was listed under
/// travel together: a child's own status can only be re-read from its parent's
/// listing, because nothing rewrites a child's session row after creation.
struct SubagentRoute: Hashable {
    let summary: ChatSubagentSummary
    let parentSessionId: String

    static func == (lhs: SubagentRoute, rhs: SubagentRoute) -> Bool {
        lhs.summary.sessionId == rhs.summary.sessionId
            && lhs.parentSessionId == rhs.parentSessionId
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(summary.sessionId)
        hasher.combine(parentSessionId)
    }
}

struct SubagentScreen: View {
    let summary: ChatSubagentSummary
    let parentSessionId: String
    let client: any BayboClientProtocol
    let onClose: () -> Void

    @StateObject private var store: SubagentReadStore
    @StateObject private var host: SubagentHost
    @ObservedObject private var lang = Lang.shared
    @State private var childrenOpen = false
    @State private var pendingChild: ChatSubagentSummary?
    @Environment(\.dismiss) private var dismiss
    @Environment(\.subagentPath) private var path

    init(
        summary: ChatSubagentSummary, parentSessionId: String,
        client: any BayboClientProtocol, onClose: @escaping () -> Void
    ) {
        self.summary = summary
        self.parentSessionId = parentSessionId
        self.client = client
        self.onClose = onClose
        // One instance, held twice: the view observes the STORE (a nested
        // `ObservableObject` republishes nothing through its owner, so a
        // binding reached via the host would never update the view), while the
        // host exists to own the webview's lifetime.
        let store = SubagentReadStore(
            sessionId: summary.sessionId, parentSessionId: parentSessionId,
            status: summary.status, client: client)
        _store = StateObject(wrappedValue: store)
        _host = StateObject(wrappedValue: SubagentHost(store: store))
    }

    var body: some View {
        ZStack(alignment: .top) {
            TranscriptWebView(webView: host.webView)
                .ignoresSafeArea(.all, edges: [.top, .bottom])
                .opacity(host.bridge.contentVisible ? 1 : 0)
                .animation(.easeOut(duration: 0.15), value: host.bridge.contentVisible)

            header
        }
        .background(Theme.paper)
        .sheet(isPresented: $childrenOpen) {
            SubagentSheet(sessionId: summary.sessionId, client: client) { picked in
                pendingChild = picked
                childrenOpen = false
            }
            .presentationDetents([.fraction(0.55), .large])
            .presentationDragIndicator(.hidden)
            .presentationBackground(Theme.paper)
            .presentationCornerRadius(Theme.radiusModal)
            .onDisappear {
                guard let picked = pendingChild else { return }
                pendingChild = nil
                path.wrappedValue.append(
                    SubagentRoute(summary: picked, parentSessionId: summary.sessionId))
            }
        }
        .sheet(item: $store.filePreview) { preview in
            FilePreviewSheet(url: preview.url) { store.filePreview = nil }
        }
        .sheet(item: $store.fileShare) { share in
            ShareSheet(url: share.url)
        }
        .fullScreenCover(item: $store.viewedImage) { viewed in
            ImageViewer(content: viewed.content, url: viewed.url) { store.viewedImage = nil }
        }
        .fullScreenCover(item: $store.videoPlayback) { playback in
            VideoPlayerScreen(url: playback.url)
        }
        .onAppear { store.startPollingIfLive() }
        .onDisappear { store.stopPolling() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            Button {
                // The deepest screen closes the whole browser; a pushed one
                // pops back to the level above.
                if path.wrappedValue.isEmpty { onClose() } else { dismiss() }
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 42, height: 42)
            }
            .glassSurface(interactive: true, in: .circle)
            .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

            VStack(alignment: .leading, spacing: 2) {
                Text(
                    verbatim: SubagentList.title(
                        task: summary.task, subagentType: summary.subagentType,
                        sessionId: summary.sessionId)
                )
                .font(Theme.mono(13))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                HStack(spacing: 6) {
                    Text(verbatim: lang.t(SubagentList.statusKey(store.status)))
                    Text(verbatim: lang.t("subagent.readOnly"))
                }
                .font(Theme.mono(10))
                .foregroundStyle(Theme.inkSoft)
            }

            Spacer(minLength: 0)

            Button {
                Haptics.tap()
                childrenOpen = true
            } label: {
                Image(systemName: "point.3.connected.trianglepath.dotted")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 42, height: 42)
            }
            .glassSurface(interactive: true, in: .circle)
            .accessibilityLabel(Text(verbatim: lang.t("chat.subagents")))
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        // Same reason as the chat header's: keep the bar a CONTAINER so SwiftUI
        // cannot collapse it into a lone focusable child and hand that child the
        // bar's whole (status-bar-inflated) frame.
        .accessibilityElement(children: .contain)
        .background(alignment: .top) {
            LinearGradient(
                stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom
            )
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
        }
    }
}

/// Owns the child's webview for exactly as long as the screen is up. A class
/// rather than plain `@State` so `teardown()` runs on the way out: this webview
/// is transient, unlike the app-wide transcript host.
@MainActor
final class SubagentHost: ObservableObject {
    let webView: WKWebView
    let bridge: TranscriptBridge
    private let host: TranscriptHost

    init(store: SubagentReadStore) {
        host = TranscriptHost(store: store)
        webView = host.webView
        bridge = host.bridge
    }

    deinit {
        // Also stops audio started from this page: `AudioPlayerCenter` is a
        // process-wide singleton holding ONE weak bridge, so a track left
        // playing here would leave the parent's card and the engine disagreeing
        // about what is playing.
        MainActor.assumeIsolated {
            AudioPlayerCenter.shared.stop()
            host.teardown()
        }
    }
}

private struct SubagentPathKey: EnvironmentKey {
    static let defaultValue: Binding<[SubagentRoute]> = .constant([])
}

extension EnvironmentValues {
    var subagentPath: Binding<[SubagentRoute]> {
        get { self[SubagentPathKey.self] }
        set { self[SubagentPathKey.self] = newValue }
    }
}
