import SwiftUI

/// One card, pushed over its board.
///
/// `ChatScreen`'s layering, applied to a different page: a native header, a
/// full-page webview, a native dock, and the dock's top edge streamed to the
/// web side as its bottom inset. The webview never resizes — that is the whole
/// reason the inset is streamed rather than the frame animated, and it is why
/// the keyboard can ride up without the page reflowing under it.
struct ProjectIssueScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @StateObject private var store: IssueStore
    @Environment(\.dismiss) private var dismiss

    let projectId: String
    let number: Int64

    /// Created with the store, torn down with the screen. One card, one
    /// webview — see `IssueHost`.
    @State private var host: IssueHost?
    @State private var confirmingStop = false
    @State private var picking: String?

    init(projectId: String, number: Int64) {
        self.projectId = projectId
        self.number = number
        _store = StateObject(wrappedValue: IssueStore(projectId: projectId, number: number))
    }

    var body: some View {
        ZStack(alignment: .top) {
            page
            header
            if let error = store.writeError { errorBanner(error) }
        }
        .background(Theme.paper)
        // The card page owns its own bottom padding from the streamed inset;
        // letting the shell ride the keyboard would move the webview's frame
        // underneath it and reflow the whole card.
        .ignoresSafeArea(.keyboard)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .safeAreaInset(edge: .bottom, spacing: 0) { dock }
        .task {
            if host == nil {
                let created = IssueHost(store: store)
                created.bridge.onOpenRun = { _ in
                    // The run transcript sheet lands in P6; until then the row
                    // is inert rather than absent, so the card's shape is the
                    // one it will keep.
                }
                created.bridge.onPick = { field in picking = field }
                host = created
                created.bridge.deliverInit(language: lang.current.lproj, bottomInset: 0)
            }
            await store.refresh()
        }
        .onChange(of: lang.current.lproj) { _, code in host?.bridge.setLanguage(code) }
        .onChange(of: store.editing) { _, active in host?.bridge.setEditing(active) }
        .onDisappear {
            if let host { host.teardown(store: store) }
        }
        .confirmationDialog(
            lang.t("issue.stopTitle"), isPresented: $confirmingStop, titleVisibility: .visible
        ) {
            Button(lang.t("issue.stopConfirm"), role: .destructive) { store.stopRun() }
            Button(lang.t("common.cancel"), role: .cancel) {}
        } message: {
            Text(verbatim: lang.t("issue.stopExplain"))
        }
    }

    @ViewBuilder private var page: some View {
        if let host {
            IssueWebView(host: host)
                .ignoresSafeArea(.container, edges: .bottom)
        } else {
            Color.clear
        }
    }

    private var dock: some View {
        IssueDock(store: store)
            // The dock's own top edge IS the page's bottom obstruction. Read
            // in window coordinates and streamed on every settle, so a
            // keyboard sliding up moves the page's padding rather than its
            // frame.
            .onGeometryChange(for: CGFloat.self) { proxy in
                proxy.frame(in: .global).minY
            } action: { _, minY in
                let screenHeight = UIScreen.main.bounds.height
                host?.bridge.setBottomInset(max(0, Int(screenHeight - minY)))
            }
    }

    // MARK: - Header

    private var header: some View {
        ZStack {
            Text(verbatim: "#\(number)")
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)

            HStack(spacing: 6) {
                Button { dismiss() } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("board.back")))
                Spacer()
                if store.liveRun != nil {
                    Button {
                        Haptics.tap()
                        confirmingStop = true
                    } label: {
                        Image(systemName: "stop.fill")
                            .font(.system(size: 13, weight: .semibold))
                            .foregroundStyle(Theme.ink)
                            .frame(width: 42, height: 42)
                    }
                    .glassSurface(interactive: true, in: .circle)
                    .accessibilityIdentifier("issue-stop")
                    .accessibilityLabel(Text(verbatim: lang.t("issue.stop")))
                }
                menu
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) {
            LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea(edges: .top)
                .allowsHitTesting(false)
        }
    }

    private var menu: some View {
        Menu {
            Button {
                Haptics.tap()
                store.editing = true
            } label: {
                Label(lang.t("issue.editDescription"), systemImage: "pencil")
            }
            .disabled(store.issue == nil)
            if store.issue?.lastRunFailed == true {
                Button {
                    Haptics.tap()
                    store.retryRun()
                } label: {
                    Label(lang.t("board.runAgain"), systemImage: "arrow.clockwise")
                }
            }
        } label: {
            Image(systemName: "ellipsis")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.ink)
                .frame(width: 42, height: 42)
        }
        .glassSurface(interactive: true, in: .circle)
        .accessibilityIdentifier("issue-menu")
        .accessibilityLabel(Text(verbatim: lang.t("list.menu")))
    }

    /// The server's own sentence — never paraphrased, for the reason the
    /// board's banner says it: the refusals name which ceiling, which block,
    /// which card holds the slot.
    private func errorBanner(_ message: String) -> some View {
        VStack {
            Spacer()
            HStack(spacing: 12) {
                Text(verbatim: message)
                    .font(Theme.sys(12.5))
                    .foregroundStyle(Theme.paper)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 0)
                Button { store.clearWriteError() } label: {
                    Image(systemName: "xmark")
                        .font(.system(size: 11, weight: .semibold))
                        .foregroundStyle(Theme.paper)
                        .frame(width: 40, height: 40)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
            .padding(.leading, 18)
            .padding(.trailing, 2)
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: 14, style: .continuous).fill(Theme.err))
            .padding(.horizontal, 16)
            .padding(.bottom, 100)
            .accessibilityIdentifier("issue-write-error")
        }
    }
}
