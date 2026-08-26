import SwiftUI
import WebKit

/// The chat screen: transcript webview filling the space, native header veil
/// pinned over its top, native composer docked below. Because the composer is
/// native, the keyboard never pans web content — SwiftUI's safe-area handling
/// moves the dock and the webview resizes; the web bundle's ResizeObserver
/// holds the newest edge through that resize.
struct ChatScreen: View {
    @ObservedObject private var store: ChatStore
    @ObservedObject private var bridge: TranscriptBridge
    private let host: TranscriptHost
    private let webView: WKWebView
    /// Only for the one-shot search jump (`takePendingJump`) — the conversation
    /// itself is driven by `store`.
    @EnvironmentObject private var appStore: AppStore
    @Environment(\.dismiss) private var dismiss
    /// The hand-rolled model menu (`ModelMenuPanel`) — state lives here
    /// because the panel overlays the TRANSCRIPT, not just the header bar.
    @State private var modelMenuOpen = false
    /// The composer's attach panel (`AttachMenuPanel`), here for the same
    /// reason — it floats over the transcript AND over the dock's own rows, so
    /// it is presented in two layers this screen owns; the `+` it blooms from
    /// is measured by the composer rather than fixed.
    @StateObject private var attach = AttachMenu()
    /// The header's message index (`MessageIndexSheet`).
    @State private var messageIndexOpen = false
    /// The header's subagent list (`SubagentSheet`).
    @State private var subagentsOpen = false
    /// The row a sheet tap picked. The jump runs from the sheet content's
    /// `.onDisappear`, not from the tap: that is deterministic against the
    /// dismissal where a guessed delay is not, so the ring blooms on a clear
    /// screen instead of behind the sliding sheet.
    @State private var pendingJump: String?
    /// How the native chrome crosses with an expanding / collapsing HTML
    /// preview. It is composited ABOVE the webview, so a hard cut shows: on the
    /// way IN it vanished a beat before the box had grown over the thread
    /// (three raw frames of headerless, composerless conversation), and on the
    /// way OUT it reappeared ON TOP of a still-full-screen agent page — the back
    /// button and the composer pill drawn over someone's article. Matches
    /// `--html-preview-morph-ms` in the transcript's styles.css; nothing breaks
    /// if the two drift, it just stops crossing cleanly.
    private static let chromeFade = Animation.easeInOut(duration: 0.26)

    init(host: TranscriptHost, store: ChatStore) {
        _store = ObservedObject(wrappedValue: store)
        _bridge = ObservedObject(wrappedValue: host.bridge)
        self.host = host
        webView = host.webView
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
            TranscriptWebView(webView: webView)
                .ignoresSafeArea(.all, edges: [.top, .bottom])
                // Fade the transcript in once it has painted (bridge `shown`),
                // rather than popping the content in as the screen slides on.
                .opacity(bridge.contentVisible ? 1 : 0)
                .animation(.easeOut(duration: 0.15), value: bridge.contentVisible)

            Group {
                ChatHeaderView(
                    store: store, bridge: bridge, catalog: .shared, menuOpen: $modelMenuOpen,
                    indexOpen: $messageIndexOpen, subagentsOpen: $subagentsOpen
                ) {
                    dismiss()
                }

                if modelMenuOpen {
                    ModelMenuPanel(store: store, catalog: .shared, isPresented: $modelMenuOpen)
                        .zIndex(2)
                }

                // Only the attach panel's SCRIM lands here — the panel itself rides
                // with the dock (below), which is the one layer that can stack above
                // the dock's own content. `AttachMenuScrim` explains the split.
                if attach.isOpen {
                    AttachMenuScrim(isPresented: $attach.isOpen)
                        .zIndex(2)
                }
            }
            .opacity(bridge.htmlPreviewMaximized ? 0 : 1)
            .allowsHitTesting(!bridge.htmlPreviewMaximized)
            .accessibilityHidden(bridge.htmlPreviewMaximized)
            .animation(Self.chromeFade, value: bridge.htmlPreviewMaximized)
        }
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        // While a full-screen HTML preview covers the conversation it borrows
        // that edge: swiping out of the preview must leave the PREVIEW, not the
        // chat behind it. Native decides the release and the page draws the
        // travel, since the box lives in the webview.
        .background(
            PopGestureEnabler(
                edgeOverride: EdgeSwipeOverride(
                    active: bridge.htmlPreviewMaximized,
                    begin: { bridge.htmlPreviewDragBegin() },
                    move: { bridge.htmlPreviewDragMove($0) },
                    end: { bridge.htmlPreviewDragEnd(dismiss: $0) }
                )
            )
            .frame(width: 0, height: 0)
        )
        // Everything the dock's own chain has to get right — the panel floating
        // above its top edge, the collapse that must neither clip nor resize —
        // lives in `ComposerDock`, which is store-free so `ComposerDockTests`
        // can render it. Only the CONTENT is assembled here.
        .safeAreaInset(edge: .bottom, spacing: 0) {
            ComposerDock(collapsed: bridge.htmlPreviewMaximized) {
                ComposerView(store: store, attach: attach)
                    .onGeometryChange(for: CGFloat.self) { proxy in
                        proxy.frame(in: .global).minY
                    } action: { minY in
                        // The composer's own geometry is the one signal that
                        // tracks BOTH the keyboard it rides and its own growth
                        // (notice line, staged strip, multiline field). The
                        // bridge converts to the covered strip against the
                        // WINDOW bottom.
                        bridge.setComposerTop(minY)
                    }
                    // An overlay, so the disc costs the dock no height: the
                    // attach panel hangs off this content's top edge, and a
                    // disc in the stack pushed the panel up by 56pt.
                    .jumpToLatestDisc(
                        visible: bridge.jumpVisible,
                        label: Lang.shared.t("chat.jumpToLatest"), identifier: "chat-jump"
                    ) {
                        bridge.jumpToLatest()
                    }
            } panel: {
                if attach.isOpen {
                    AttachMenuPanel(
                        anchor: attach.anchor, sources: attach.sources,
                        isPresented: $attach.isOpen
                    ) { source in
                        attach.pick = source
                    }
                }
            }
            .animation(Self.chromeFade, value: bridge.htmlPreviewMaximized)

        }
        .background(Theme.paper)
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
        // ONE sheet: the child transcript is PUSHED inside it, so the detail
        // slides in over the list instead of waiting for it to collapse, and
        // backing out is a native pop straight back to the list.
        //
        // A `.sheet`, NOT a `fullScreenCover`: a cover fires this screen's
        // `onDisappear`, which detaches the parent transcript's bridge for as
        // long as it is up (`AppStore.chatPath`'s didSet says the same of the
        // image viewer). Reading a subagent that ran for half an hour would
        // overflow the parent's offscreen frame buffer and force a full
        // re-sync on the way back.
        .sheet(isPresented: $subagentsOpen) {
            SubagentSheet(sessionId: store.sessionId, parentSessionId: store.sessionId)
                .presentationDetents([.large])
                .presentationDragIndicator(.hidden)
                .presentationBackground(Theme.paper)
                .presentationCornerRadius(Theme.radiusModal)
        }
        .sheet(isPresented: $messageIndexOpen) {
            MessageIndexSheet(bridge: bridge) { rowId in
                pendingJump = rowId
                messageIndexOpen = false
            }
            .presentationDetents([.fraction(0.55), .large])
            .presentationDragIndicator(.hidden)
            .presentationBackground(Theme.paper)
            .presentationCornerRadius(Theme.radiusModal)
            .onDisappear {
                guard let rowId = pendingJump else { return }
                pendingJump = nil
                bridge.jumpToMessage(rowId)
            }
        }
        // One floating panel at a time — two scrims stack, and the one behind
        // strands its own dismiss. Only this direction is reachable: the attach
        // scrim covers the header, while the model panel's runs UNDER the dock
        // (`.safeAreaInset` composites above it), so the `+` stays live there.
        // The dismissal is wrapped like every other one of that panel's (the
        // header's index button makes the same move): bare, it popped out
        // instantly under a scrim that was still fading in.
        .onChange(of: attach.isOpen) { _, open in
            guard open, modelMenuOpen else { return }
            withAnimation(.easeOut(duration: 0.15)) { modelMenuOpen = false }
        }
        .onChange(of: bridge.htmlPreviewMaximized) { _, maximized in
            guard maximized else { return }
            modelMenuOpen = false
            attach.isOpen = false
            messageIndexOpen = false
            subagentsOpen = false
        }
        .onAppear {
            host.bridge.retarget(to: store)
            store.connectIfNeeded()
            ModelCatalog.shared.refreshIfNeeded()
            store.refreshModelPin()
            SessionIndex.shared.enterSession(store.sessionId)
            // The header entry's second source: the web side can only see the
            // rows it has loaded, and a spawn older than the current window is
            // invisible to it. One bounded request per open covers that; the
            // two are OR-ed, so an offline open still gets the web verdict.
            Task {
                guard
                    let list = try? await Baybo.client.chatListSubagents(
                        sessionId: store.sessionId, before: nil)
                else { return }
                // Keep the rows, not just the verdict: the sheet seeds from
                // this, so opening it paints on the first frame instead of
                // paying a second round trip for what was already fetched.
                SubagentCache.shared.put(
                    sessionId: store.sessionId, items: list.items,
                    hasMoreOlder: list.hasMoreOlder)
                if !list.items.isEmpty { bridge.noteSubagentsPresent() }
            }
            // A search result routed here: park the transcript on the matched
            // message. Consumed on APPEAR rather than issued at the call site, so
            // it survives a `TranscriptHost` rebuilt between the tap and this
            // screen mounting — the queued JS would have gone with the old host.
            // `takePendingJump` removes on read, so a later re-appear (a cover
            // dismissing) can't re-park a reader who has moved on.
            if let ordinal = appStore.takePendingJump(store.sessionId) {
                bridge.jumpToOrdinal(ordinal)
            }
            // Where the responder chain's paste hook (`AppDelegate`) sends an
            // image. Registered on the SCREEN, not on the composer: every
            // `fullScreenCover` over the chat tears the dock's `.safeAreaInset`
            // down and puts it straight back, and this screen is what stays.
            ComposerPasteTarget.shared.attach(store.staging)
            #if DEBUG
                store.startDemoFramesIfRequested()
                store.startDemoAttachmentsIfRequested()
                store.startDemoImagesIfRequested()
                store.startDemoApprovalIfRequested()
                store.startDemoHtmlIfRequested()
                store.startDemoSwitchIfRequested()
                store.startDemoIndexIfRequested()
                bridge.startDemoJumpIfRequested()
                startDemoBackIfRequested()
            #endif
        }
        .onDisappear {
            host.bridge.detachCurrent(store)
            SessionIndex.shared.leaveSession(store.sessionId)
            ComposerPasteTarget.shared.detach(store.staging)
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
/// the status bar and easing out to clear at the bar's bottom edge. On the
/// ink side: the glass back circle and the model pill on the LEFT (the pill
/// toggles the hand-rolled `ModelMenuPanel` the screen overlays); on the
/// trailing edge the message-index circle (opening `MessageIndexSheet`), and —
/// ONLY while the leg is offline — a red `wifi.slash` in a glass circle
/// immediately to ITS left; every healthy state shows nothing there. The veil
/// ignores touches so scrolls beneath it reach the thread.
struct ChatHeaderView: View {
    @ObservedObject var store: ChatStore
    @ObservedObject var bridge: TranscriptBridge
    @ObservedObject var catalog: ModelCatalog
    @Binding var menuOpen: Bool
    @Binding var indexOpen: Bool
    @Binding var subagentsOpen: Bool
    let onBack: () -> Void

    private static let veilPeakAlpha = 0.8
    /// Shared with `ArchivedScreen`, which reuses this header grammar.
    static let barHeight: CGFloat = 46
    /// Solid → clear smoothstep the veil fades through, below its solid
    /// status-bar zone (the composer veil's grammar, mirrored to the top).
    private static let rampAlphas: [Double] = [1.0, 0.9, 0.65, 0.35, 0.1, 0.0]
    /// Cap on the pill's LABEL TEXT so a long model id can't outgrow a narrow
    /// screen. Applied to the string, not via `.frame(maxWidth:)` — a frame
    /// cap makes the Menu label greedy (it expands to the cap even for short
    /// names) where the pill should hug its content; the menu rows still show
    /// the full name.
    private static let modelLabelMaxChars = 8

    var body: some View {
        HStack(spacing: 12) {
            // The glass back circle (logout moved to the chat list's header —
            // leaving the account lives one level up now). Semibold glyph so
            // the ink reads as solid black over the bright glass.
            Button(action: onBack) {
                Image(systemName: "chevron.left")
                    .font(.system(size: 18, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .frame(width: 42, height: 42)
            }
            .glassSurface(interactive: true, in: .circle)
            .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.back")))

            // The model pill renders only once the catalog has entries —
            // before the first successful fetch (and offline) there is
            // nothing to offer, and an unpinned session runs the default
            // whether or not the pill says so.
            if !catalog.models.isEmpty {
                modelPill
            }

            Spacer()

            // Connectivity keeps NO indicator in every healthy state; only a
            // SUSTAINED outage surfaces (`legDown` — debounced, because the
            // raw state oscillates through the retry loop), clearing itself
            // the moment a dial lands.
            if store.legDown {
                offlineIcon
            }

            // BOTH persistent controls come LAST, after the outage branch.
            // `Spacer()` absorbs every width change, so only the trailing side
            // holds still; declared inboard of the alarm, either of these would
            // slide 54pt under the blanket `legDown` animation every time the
            // network drops. A persistent control must not move — the transient
            // alarm inserts to their left.
            if bridge.subagentsPresent {
                subagentsButton
            }
            indexButton
        }
        .padding(.horizontal, 24)
        .frame(height: Self.barHeight)
        .frame(maxWidth: .infinity)
        // Keep the bar an accessibility CONTAINER. DO NOT delete this as
        // obsolete now that the index button is permanent: what it defends
        // against is the bar being left with ONE focusable child, which SwiftUI
        // then collapses the bar into — the child inheriting the bar's frame
        // after the veil's `ignoresSafeArea` has stretched it over the status
        // bar. An offline session used to show neither the model pill nor the
        // index button, leaving just the back chevron, which then reported
        // (0, 0, 402, 108) instead of its own 42pt circle. Touching the GLYPH
        // still worked, so nothing looked broken by hand — but a tap aimed at
        // the element's CENTRE, which is what XCUITest does, landed on empty
        // header and silently did nothing. That is how three back-chain UI
        // tests died without a single line of navigation code changing. A
        // permanent index button removes today's trigger; the next conditional
        // child re-arms it.
        .accessibilityElement(children: .contain)
        .background(alignment: .top) { veil }
        .animation(.easeOut(duration: 0.15), value: store.legDown)
        // On the STACK, not on the button: an `.animation(_:value:)` inside the
        // view being inserted has no previous value to compare, so it would pop
        // in — and under the blanket animation above, on the wrong beat.
        .animation(.easeOut(duration: 0.16), value: bridge.subagentsPresent)
    }

    /// The model capsule: the effective MODEL id (the pinned entry's, or the
    /// default entry's when unpinned). Tapping toggles the hand-rolled
    /// `ModelMenuPanel` (providers → models + thinking level → levels) that
    /// `ChatScreen` overlays under this pill. A pick applies from the
    /// session's NEXT turn (gateway contract), so no confirmation step.
    private var modelPill: some View {
        Button {
            Haptics.tap()
            withAnimation(.easeOut(duration: 0.15)) { menuOpen.toggle() }
        } label: {
            Text(verbatim: modelLabel)
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
                .lineLimit(1)
                .padding(.horizontal, 16)
                .frame(height: 42)
        }
        .glassSurface(interactive: true, in: Capsule())
        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.model")))
        // The a11y label hides the pill's visible text, so surface it as the
        // element's VALUE — the FULL id, not the ellipsized display string:
        // VoiceOver reads "Model, gpt-5.5" and the UI smoke asserts a pick
        // landed through it.
        .accessibilityValue(Text(verbatim: modelName))
    }

    /// The one state with a header indicator: the leg has been down for a
    /// sustained stretch. The universal no-network glyph, in the error ink.
    private var offlineIcon: some View {
        Image(systemName: "wifi.slash")
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(Theme.err)
            .frame(width: 42, height: 42)
            .glassSurface(in: .circle)
            .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.offline")))
            .transition(.scale(scale: 0.6).combined(with: .opacity))
    }

    /// Opens the subagent list over the transcript. Deliberately STATIC — no
    /// pulse, no count, no badge. Liveness lives inside the sheet: a foreground
    /// spawn could be derived locally but a BACKGROUND one could not (its tool
    /// call acks immediately), so an animated header would be confidently wrong
    /// half the time.
    private var subagentsButton: some View {
        Button {
            Haptics.tap()
            // The model menu is a bare Bool with no mutual exclusion; left open
            // it strands its scrim behind the sheet.
            if menuOpen {
                withAnimation(.easeOut(duration: 0.15)) { menuOpen = false }
            }
            subagentsOpen = true
        } label: {
            Image(systemName: "point.3.connected.trianglepath.dotted")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.ink)
                .frame(width: 42, height: 42)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.subagents")))
        .transition(.scale(scale: 0.7).combined(with: .opacity))
    }

    /// Opens the message index over the transcript. PERMANENT — it no longer
    /// waits for a thread long enough to be worth indexing, because a control
    /// that comes and goes costs the reader more than an empty sheet does.
    /// Deliberately BADGELESS: an ink capsule carrying a number already means
    /// "unread" one screen up, so the count ships as the element's VALUE
    /// against a constant label — the model pill's split, which is what the UI
    /// smokes drive.
    private var indexButton: some View {
        Button {
            Haptics.tap()
            // The model menu is a bare Bool with no mutual exclusion; left
            // open it strands its scrim behind the sheet.
            if menuOpen {
                withAnimation(.easeOut(duration: 0.15)) { menuOpen = false }
            }
            bridge.requestOutlineHere()
            indexOpen = true
        } label: {
            Image(systemName: "text.alignright")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.ink)
                .frame(width: 42, height: 42)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.messageIndex")))
        .accessibilityValue(
            Text(
                verbatim: MessageIndexSheet.countLabel(
                    bridge.outline.count, hasMore: bridge.outlineHasMoreOlder))
        )
    }

    /// The effective MODEL id the pill names: the pinned model within the
    /// pinned entry, the effective entry's default model when no model is
    /// pinned — or the raw pin strings when the catalog no longer has the
    /// entry (stale but honest). Always the best-known value, never a
    /// placeholder: an offline open must keep naming the model (mirror-fed),
    /// even while the pin read hasn't confirmed it yet.
    private var modelName: String {
        if let model = store.modelPinModel { return model }
        return catalog.effectiveEntry(pin: store.modelPin)?.model
            ?? store.modelPin ?? catalog.defaultName ?? ""
    }

    /// What the pill reads at rest: `modelName`, ellipsized
    /// (`modelLabelMaxChars`).
    private var modelLabel: String {
        let name = modelName
        guard name.count > Self.modelLabelMaxChars else { return name }
        return String(name.prefix(Self.modelLabelMaxChars)) + "…"
    }

    /// White paper veil: one fade that floods up over the status bar (so its
    /// coordinate space is status-bar + bar) and holds solid through the
    /// status bar, then smoothsteps to clear at the bar's bottom — i.e. the
    /// model pill's bottom edge. Nothing below the bar. The gradient fill
    /// itself carries `ignoresSafeArea(.top)`; nesting the flood inside a
    /// `.background` clips it (the parent doesn't ignore the inset).
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
}
