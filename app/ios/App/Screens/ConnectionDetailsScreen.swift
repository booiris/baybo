import SwiftUI

struct ConnectionDetailsScreen: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var connection = ConnectionStore.shared
    @StateObject private var diagnostics = ConnectionDiagnosticsStore(client: Baybo.client)

    @State private var controlsHeight: CGFloat = 0
    private let pagePadding: CGFloat = 24
    private let sectionSpacing: CGFloat = 24

    var body: some View {
        GeometryReader { geometry in
            ScrollView {
                VStack(alignment: .leading, spacing: sectionSpacing) {
                    VStack(alignment: .leading, spacing: sectionSpacing) {
                        VStack(spacing: 0) {
                            detailRow(
                                lang.t("settings.connection"),
                                ConnectionLabels.carrierLabel(connection.status?.carrier ?? .relay))
                            if let probe = connection.status?.lastProbe {
                                ForEach(probe.tiers.indices, id: \.self) { index in
                                    let tier = probe.tiers[index]
                                    Divider().overlay(Theme.line)
                                    detailRow(
                                        ConnectionLabels.tierLabel(tier.tier),
                                        ConnectionLabels.outcomeLabel(tier.outcome))
                                }
                            }
                        }

                        Toggle(isOn: Binding(get: { diagnostics.enabled }, set: diagnostics.setEnabled)) {
                            Text(verbatim: lang.t("connection.diagnostics"))
                                .font(Theme.mono(15))
                        }
                        .tint(Theme.ink)
                        .accessibilityIdentifier("connection.diagnostics")
                    }
                    .onGeometryChange(for: CGFloat.self) {
                        $0.size.height
                    } action: {
                        controlsHeight = $0
                    }

                    if diagnostics.enabled || !diagnostics.entries.isEmpty {
                        ConnectionLogConsole(store: diagnostics)
                            .frame(
                                height: max(
                                    240,
                                    geometry.size.height - controlsHeight
                                        - pagePadding * 2 - sectionSpacing))
                    }
                }
                .padding(pagePadding)
            }
            .scrollBounceBehavior(.basedOnSize)
        }
        .background(Theme.paper)
        .foregroundStyle(Theme.ink)
        .safeAreaInset(edge: .top, spacing: 0) { header }
        .tint(Theme.ink)
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task(id: diagnostics.enabled) {
            guard diagnostics.enabled else { return }
            while !Task.isCancelled {
                diagnostics.poll()
                do { try await Task.sleep(for: .milliseconds(250)) } catch { return }
            }
        }
        .onDisappear { diagnostics.stop() }
    }

    private var header: some View {
        ZStack {
            Text(verbatim: lang.t("connection.details"))
                .font(Theme.mono(16))
            HStack {
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("connection.back")))
                .accessibilityIdentifier("connection.back")
                Spacer()
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .background(Theme.paper)
    }

    private func detailRow(_ title: String, _ value: String) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 16) {
            Text(verbatim: title).font(Theme.mono(14))
            Spacer(minLength: 8)
            Text(verbatim: value)
                .font(Theme.mono(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.trailing)
        }
        .padding(.vertical, 14)
    }
}

private struct ConnectionLogConsole: View {
    @ObservedObject var store: ConnectionDiagnosticsStore
    @ObservedObject private var lang = Lang.shared
    @State private var followsTail = true
    @State private var nearBottom = true
    @State private var scrollPhase: ScrollPhase = .idle
    private let bottomTolerance: CGFloat = 20
    @State private var copied = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 18) {
                Text(verbatim: lang.t(store.enabled ? "connection.live" : "connection.stopped"))
                    .foregroundStyle(Theme.inkSoft)
                Spacer()
                Button {
                    UIPasteboard.general.string = store.text
                    copied = true
                } label: {
                    Text(verbatim: lang.t(copied ? "connection.copied" : "connection.copy"))
                }
                .disabled(store.entries.isEmpty)
                Button {
                    store.clear()
                    copied = false
                } label: {
                    Text(verbatim: lang.t("connection.clear"))
                }
            }
            .font(Theme.mono(12))

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 8) {
                        if store.entries.isEmpty {
                            Text(verbatim: lang.t("connection.waiting"))
                                .foregroundStyle(Theme.inkSoft)
                        }
                        ForEach(store.entries, id: \.sequence) { entry in
                            Text(styledLine(entry))
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .textSelection(.enabled)
                        }
                        Color.clear.frame(height: 1).id("tail")
                    }
                    .font(.system(size: 11, design: .monospaced))
                    .padding(14)
                }
                .frame(maxHeight: .infinity)
                .background(Theme.surface, in: RoundedRectangle(cornerRadius: Theme.radius))
                .overlay(RoundedRectangle(cornerRadius: Theme.radius).stroke(Theme.line))
                .accessibilityIdentifier("connection.logConsole")
                .onScrollGeometryChange(for: Bool.self) { geometry in
                    geometry.contentSize.height - geometry.visibleRect.maxY <= bottomTolerance
                } action: { _, atBottom in
                    nearBottom = atBottom
                    if atBottom {
                        followsTail = true
                    } else if scrollPhase == .interacting || scrollPhase == .decelerating {
                        followsTail = false
                    }
                }
                .onScrollPhaseChange { _, phase in
                    scrollPhase = phase
                    if phase == .idle { followsTail = nearBottom }
                }
                .onChange(of: store.entries.last?.sequence) { _, _ in
                    copied = false
                    if followsTail { proxy.scrollTo("tail", anchor: .bottom) }
                }
                Button {
                    followsTail = true
                    proxy.scrollTo("tail", anchor: .bottom)
                } label: {
                    Label(lang.t("connection.latest"), systemImage: "arrow.down")
                        .font(Theme.mono(12))
                        .frame(minHeight: 32)
                }
                .accessibilityIdentifier("connection.followLatest")
                .opacity(followsTail ? 0 : 1)
                .allowsHitTesting(!followsTail)
                .accessibilityHidden(followsTail)
            }
        }
        .buttonStyle(.plain)
    }

    private func styledLine(_ entry: ConnectionLogEntry) -> AttributedString {
        var timestamp = AttributedString(ConnectionDiagnosticsStore.timestamp(entry) + " ")
        timestamp.foregroundColor = Theme.inkSoft
        var stage = AttributedString("[" + ConnectionDiagnosticsStore.stage(entry.stage) + "] ")
        stage.foregroundColor = stageColor(entry.stage)
        var message = AttributedString(entry.message)
        message.foregroundColor = Theme.ink
        return timestamp + stage + message
    }

    private func stageColor(_ stage: ConnectionLogStage) -> Color {
        switch stage {
        case .lifecycle: return .indigo
        case .network, .probe: return .blue
        case .relay, .rendezvous: return .purple
        case .quic, .chat: return .teal
        }
    }

}
