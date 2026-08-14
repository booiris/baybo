import Combine
import SwiftUI
import WebKit

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

    @StateObject private var store: SubagentReadStore
    @StateObject private var host: SubagentHost
    @ObservedObject private var lang = Lang.shared
    @State private var childrenOpen = false
    /// The REST half of the entry's gate — see `hasChildren`.
    @State private var childrenListed = false
    @Environment(\.dismiss) private var dismiss

    init(
        summary: ChatSubagentSummary, parentSessionId: String,
        client: any BayboClientProtocol = Baybo.client
    ) {
        self.summary = summary
        self.parentSessionId = parentSessionId
        self.client = client
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
                .opacity(host.contentVisible ? 1 : 0)
                .animation(.easeOut(duration: 0.15), value: host.contentVisible)

            header
        }
        .background(Theme.paper)
        // Hiding the navigation bar (custom chrome) also disables the
        // interactive pop, so the left-edge swipe has to be handed back the
        // same way `ChatScreen` does it. This screen is pushed inside the
        // sheet's own `NavigationStack`, so the host finds a navigation
        // controller in its parent chain and the recognizer takes effect.
        .background(
            PopGestureEnabler()
                .frame(width: 0, height: 0)
        )
        .sheet(isPresented: $childrenOpen) {
            SubagentSheet(sessionId: summary.sessionId, parentSessionId: parentSessionId)
                .presentationDetents([.large])
                .presentationDragIndicator(.hidden)
                .presentationBackground(Theme.paper)
                .presentationCornerRadius(Theme.radiusModal)
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
        .onAppear {
            store.startPollingIfLive()
            Task {
                guard
                    let list = try? await client.chatListSubagents(
                        sessionId: summary.sessionId, before: nil)
                else { return }
                // Seeds the sheet too, so opening it paints on the first frame.
                SubagentCache.shared.put(
                    sessionId: summary.sessionId, items: list.items,
                    hasMoreOlder: list.hasMoreOlder)
                childrenListed = !list.items.isEmpty
            }
        }
        .onDisappear { store.stopPolling() }
    }

    /// Shown only when this child delegated in turn — the parent entry's rule,
    /// for the parent entry's reason: most children never spawn, and an entry
    /// that always shows is one that mostly opens an empty sheet. Two sources
    /// OR'd, exactly as one level up: what this page's own rows show (zero
    /// network, correct offline) and one bounded list request on appear.
    private var hasChildren: Bool { host.subagentsPresent || childrenListed }

    private var header: some View {
        HStack(spacing: 12) {
            Button {
                // One call covers both levels: inside a `NavigationStack` this
                // pops to the list, and at the stack's root it dismisses the
                // sheet back to the conversation.
                dismiss()
            } label: {
                Image(systemName: "chevron.left")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 42, height: 42)
            }
            .glassSurface(interactive: true, in: .circle)
            .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

            // The errand alone. Status and duration already read on the row
            // that got here, and repeating them over the transcript that shows
            // the same work is noise; "read-only" is answered by the absence of
            // a composer, not by a label saying so.
            Text(
                verbatim: SubagentList.title(
                    task: summary.task, subagentType: summary.subagentType,
                    sessionId: summary.sessionId)
            )
            .font(Theme.mono(13))
            .foregroundStyle(Theme.ink)
            .lineLimit(1)

            Spacer(minLength: 0)

            // Gated exactly like the parent's, and for the same reason: most
            // children never delegate, and an entry that always shows is an
            // entry that mostly opens an empty sheet.
            if hasChildren {
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
                .transition(.scale(scale: 0.7).combined(with: .opacity))
            }
        }
        .animation(.easeOut(duration: 0.16), value: hasChildren)
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
    /// Mirrored off the bridge because the screen observes THIS object, not the
    /// bridge inside it: reading `host.bridge.contentVisible` compiles and even
    /// gives the right first value, but a nested `ObservableObject` republishes
    /// nothing through its owner — so the `true` that arrives with the page's
    /// `ready` never re-renders anything and the webview stays at `opacity(0)`
    /// forever. That is a fully-loaded, fully-synced transcript behind a
    /// transparent view: the page looks blank and nothing anywhere errors.
    /// `ChatScreen` avoids it by observing the bridge directly, which this
    /// screen cannot — its bridge is born with the host, inside a `StateObject`.
    @Published private(set) var contentVisible = false
    /// Mirrored for the same reason as `contentVisible` — a nested
    /// `ObservableObject` republishes nothing through its owner, and a gate
    /// read through one would latch without ever re-rendering the header.
    @Published private(set) var subagentsPresent = false

    private let host: TranscriptHost
    private var visibility: AnyCancellable?
    private var delegation: AnyCancellable?

    init(store: SubagentReadStore) {
        host = TranscriptHost(store: store)
        webView = host.webView
        bridge = host.bridge
        visibility = bridge.$contentVisible.sink { [weak self] visible in
            self?.contentVisible = visible
        }
        delegation = bridge.$subagentsPresent.sink { [weak self] present in
            self?.subagentsPresent = present
        }
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

