import Combine
import SwiftUI
import WebKit

/// Which run a sheet is showing. A run is addressed by board, card and
/// attempt — never by session id, because an attempt that never started has
/// no session and is still a row somebody wants to read.
struct ProjectRunRoute: Identifiable, Hashable {
    let projectId: String
    let number: Int64
    let attempt: Int64
    let sessionId: String
    let status: RunStatus

    var id: String { "\(projectId)#\(number)/\(attempt)" }

    static func == (lhs: ProjectRunRoute, rhs: ProjectRunRoute) -> Bool { lhs.id == rhs.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

/// One run's transcript, over the card that started it.
///
/// The same webview and the same React tree the chat uses: a run IS a session,
/// and its rows ARE chat rows. What is different is everything around them —
/// there is no composer, no outbox and no mirror — which is what
/// `ProjectRunReadStore` is for.
struct ProjectRunSheet: View {
    let route: ProjectRunRoute
    /// Stopping is the card's, not the sheet's: this raises it and the card
    /// runs the confirm, so there is one Stop with one confirmation rather
    /// than two that can disagree.
    let onStop: () -> Void

    @StateObject private var store: ProjectRunReadStore
    @StateObject private var host: ProjectRunHost
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    init(route: ProjectRunRoute, onStop: @escaping () -> Void) {
        self.route = route
        self.onStop = onStop
        // One instance, held twice — the view observes the STORE (a nested
        // `ObservableObject` republishes nothing through its owner, so a gate
        // read through the host would latch without re-rendering), and the
        // host exists to own the webview's lifetime. `SubagentScreen`'s scar.
        let store = ProjectRunReadStore(
            projectId: route.projectId, number: route.number, attempt: route.attempt,
            sessionId: route.sessionId, status: route.status)
        _store = StateObject(wrappedValue: store)
        _host = StateObject(wrappedValue: ProjectRunHost(store: store))
    }

    var body: some View {
        NavigationStack {
            ZStack(alignment: .top) {
                TranscriptWebView(webView: host.webView)
                    .ignoresSafeArea(.all, edges: [.top, .bottom])
                    .opacity(host.contentVisible ? 1 : 0)
                    .animation(.easeOut(duration: 0.15), value: host.contentVisible)
                header
            }
            .background(Theme.paper)
            .toolbar(.hidden, for: .navigationBar)
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
        ZStack {
            VStack(spacing: 1) {
                Text(verbatim: lang.t("run.title", "#\(route.number)", "\(route.attempt)"))
                    .font(Theme.mono(15))
                    .foregroundStyle(Theme.ink)
                Text(verbatim: lang.t("run.status.\(IssueWire.word(store.status))"))
                    .font(Theme.mono(10))
                    .textCase(.uppercase)
                    .kerning(0.8)
                    .foregroundStyle(store.status == .failed ? Theme.err : Theme.inkSoft)
            }
            HStack(spacing: 6) {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.down")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityIdentifier("run-close")
                .accessibilityLabel(Text(verbatim: lang.t("run.close")))
                Spacer()
                if ProjectRunReadStore.isLive(store.status) {
                    Button {
                        Haptics.tap()
                        dismiss()
                        onStop()
                    } label: {
                        Image(systemName: "stop.fill")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassSurface(interactive: true, in: .circle)
                    .accessibilityIdentifier("run-stop")
                    .accessibilityLabel(Text(verbatim: lang.t("issue.stop")))
                }
            }
        }
        .padding(.horizontal, 20)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) {
            LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .top)
                .allowsHitTesting(false)
        }
    }
}

/// Owns the run page's webview for the sheet's lifetime, and mirrors the
/// bridge's gates.
///
/// The mirroring is load-bearing and not tidiness: the view observes THIS
/// object, and a nested `ObservableObject` republishes nothing through its
/// owner — so `host.bridge.contentVisible` would give the right first value,
/// latch to `true` on the page's `ready`, and re-render nothing. That is a
/// fully-loaded transcript behind a transparent view: blank page, no error
/// anywhere. `SubagentHost` carries the same comment for the same reason.
@MainActor
final class ProjectRunHost: ObservableObject {
    let webView: WKWebView
    let bridge: TranscriptBridge
    @Published private(set) var contentVisible = false

    private let host: TranscriptHost
    private var visibility: AnyCancellable?

    init(store: ProjectRunReadStore) {
        host = TranscriptHost(store: store)
        webView = host.webView
        bridge = host.bridge
        visibility = bridge.$contentVisible.sink { [weak self] visible in
            self?.contentVisible = visible
        }
    }

    deinit {
        // Also stops audio started from this page: `AudioPlayerCenter` is a
        // process-wide singleton holding ONE weak bridge, so a track left
        // playing here would leave the card and the engine disagreeing about
        // what is playing.
        MainActor.assumeIsolated {
            AudioPlayerCenter.shared.stop()
            host.teardown()
        }
    }
}
