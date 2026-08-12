import SwiftUI

/// Full-text search over every conversation — the home shell's `.search` tab.
///
/// The tab carries `TabRole.search`, which on iOS 26 the system lifts out of the
/// glass pill and floats as its own trailing circle; that separated affordance
/// is the point, and it is why this is a tab rather than a pushed screen.
///
/// Server-backed: it calls `GET /v1/chat/search` through the FFI, which scopes
/// the query the same way app/web's panel does — hidden sessions stay lost,
/// archived ones stay archived, and cron workspaces are excluded server-side.
/// Search is one protocol implemented twice, so anything that differs between
/// the two clients is a bug on this side, not a different feature.
///
/// A result opens with `openSearchResult`, which appends to `chatPath` WITHOUT
/// forcing the Chats tab. The push lands on the outer NavigationStack wrapping
/// the whole TabView, so the conversation covers the shell and this tab stays
/// selected underneath it — backing out returns here with the query and results
/// still on screen.
struct SearchScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var index = SessionIndex.shared
    @State private var query = ""
    /// False for one frame on entry so the field can grow from the circle's
    /// footprint rather than appearing at full width.
    @State private var expanded = false
    @FocusState private var focused: Bool
    /// The querying half — debounce, staleness, the composition gate. Lives
    /// apart from this view so those rules are testable without a UI host.
    @StateObject private var model = SearchModel()

    /// Excerpts shown per conversation. The gateway sends up to 3; a phone shows
    /// 2, or a card runs ~7 lines and a screen holds three results.
    private static let excerptsPerCard = 2

    var body: some View {
        results
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            // The field sits where the TAB BAR was, not at the top: the native
            // bar hides on entry (`HomeTabView`) and this takes its place, so the
            // trailing search circle reads as stretching into a field. A
            // `safeAreaInset` rather than an overlay so the results list insets
            // itself and its last card is never parked under the field.
            .safeAreaInset(edge: .bottom, spacing: 0) { bottomBar }
            .background(Theme.paper)
        // Focus on ENTERING the tab, not once per mount: a TabView keeps its
        // pages alive, so `onAppear` also fires when the reader comes back from
        // a conversation — and raising the keyboard over the results they just
        // navigated back to is not what they asked for. Only when the field is
        // OURS; on iOS 26 the system raises its own as part of the morph.
        .onChange(of: appStore.homeTab, initial: true) { _, tab in
            guard tab == .search else {
                expanded = false
                return
            }
            withAnimation(Self.morph) { expanded = true }
            if query.isEmpty { focused = true }
        }
        .onDisappear { model.cancel() }
        .onChange(of: query) { _, new in model.update(query: new) }
    }

    // MARK: - Chrome

    /// The field, docked where the tab bar was, with the ✕ that leaves search.
    ///
    /// `expanded` drives the stretch: the field starts at the width of the tab
    /// bar's search circle, pinned to the trailing edge, and grows leftward to
    /// fill the bar's width. The native circle vanishes in the same frame the bar
    /// hides, so what the eye follows is one shape opening up.
    private var bottomBar: some View {
        HStack(spacing: expanded ? 10 : 0) {
            field
                .frame(maxWidth: expanded ? .infinity : Self.fieldHeight)
                // Park the COLLAPSED field against the trailing edge, so it
                // starts where the tab bar's search circle was and opens
                // LEFTWARD. Without this the HStack puts the narrow field at the
                // leading edge and it grows the wrong way, away from the control
                // it is supposed to be growing out of.
                .frame(maxWidth: .infinity, alignment: .trailing)

            Button {
                query = ""
                withAnimation(Self.morph) { expanded = false }
                appStore.exitSearch()
            } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 16, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    // Square on the field's height, so the two sit on one line
                    // instead of the circle towering over the pill.
                    .frame(width: Self.fieldHeight, height: Self.fieldHeight)
            }
            .glassSurface(tint: Theme.paper.opacity(0.25), interactive: true, in: .circle)
            .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
            // Zero-width while collapsed, so the field's trailing edge really is
            // the screen's — otherwise it starts 58pt inboard of the circle it
            // claims to be growing from.
            .frame(width: expanded ? Self.fieldHeight : 0)
            .accessibilityIdentifier("search.exit")
            .accessibilityLabel(Text(verbatim: lang.t("search.exit")))
            .opacity(expanded ? 1 : 0)
        }
        .padding(.horizontal, 20)
        .padding(.bottom, 8)
        .background(Theme.paper)
    }

    /// The composer's pill height. Doubles as the COLLAPSED width, so the field
    /// starts as a circle the size of the one it replaces and opens from there —
    /// and as the ✕'s size, so the two are one line rather than two heights.
    private static let fieldHeight: CGFloat = 48
    /// The stretch. A spring, because the native tab bar's own selection morph is
    /// one and a linear ramp beside it reads as a different app.
    private static let morph: Animation = {
        #if DEBUG
            // `-baybo-demo-slow-morph`: stretch it to 4s so the expansion can be
            // SAMPLED headlessly. A spring at 0.42s gives a screenshot harness
            // one usable frame if it is lucky; this gives it as many as it wants,
            // which is how the leftward direction was verified.
            if ProcessInfo.processInfo.arguments.contains("-baybo-demo-slow-morph") {
                return .linear(duration: 4)
            }
        #endif
        return .spring(response: 0.42, dampingFraction: 0.82)
    }()

    private var field: some View {
        HStack(spacing: 8) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(Theme.inkSoft)

            TextField(
                "", text: $query,
                prompt: Text(verbatim: lang.t("search.placeholder"))
                    .foregroundColor(Theme.inkSoft)
            )
            .font(Theme.mono(15))
            .foregroundStyle(Theme.ink)
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            .submitLabel(.search)
            .focused($focused)
            .accessibilityIdentifier("search.field")

            if !query.isEmpty {
                Button {
                    query = ""
                } label: {
                    Image(systemName: "xmark.circle.fill")
                        .font(.system(size: 15))
                        .foregroundStyle(Theme.inkSoft)
                }
                .accessibilityLabel(Text(verbatim: lang.t("search.clear")))
            }
        }
        .padding(.horizontal, 14)
        .frame(height: Self.fieldHeight)
        // Clip the CONTENT (before the glass, so the shadow is untouched): mid
        // stretch the pill is narrower than the row inside it, and without this
        // the placeholder and the clear button spill past its edges.
        .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
        // The composer's pill, verbatim: same glass tint, same 24pt radius, same
        // ambient shadow. Two fields in one app that both mean "type here" should
        // not be two different objects — and over blank white the untinted glass
        // is nearly invisible, which is why the shadow carries the boundary
        // rather than a hairline.
        .glassSurface(tint: Theme.paper.opacity(0.25), in: .rect(cornerRadius: 24))
        .shadow(color: Theme.ink.opacity(0.08), radius: 14, y: 4)
    }

    // MARK: - Results

    @ViewBuilder private var results: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 10) {
                switch model.phase {
                case .idle:
                    hint(lang.t("search.hint"))
                case .loading:
                    // Deliberately no spinner-over-blank: `scheduleSearch` keeps
                    // the previous results on screen while a new query is in
                    // flight, so this is reached only for the FIRST query.
                    ProgressView()
                        .frame(maxWidth: .infinity)
                        .padding(.top, 40)
                case .failed:
                    hint(lang.t("search.failed"))
                case .ok(let groups, let truncated):
                    if groups.isEmpty {
                        hint(lang.t("search.empty"))
                    } else {
                        ForEach(groups, id: \.sessionId) { group in
                            card(group)
                        }
                        if truncated {
                            hint(lang.t("search.truncated"))
                        }
                    }
                }
            }
            .padding(.horizontal, 20)
            .padding(.top, 12)
            .padding(.bottom, 16)
        }
        .scrollDismissesKeyboard(.interactively)
    }

    private func hint(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.mono(13))
            .foregroundStyle(Theme.inkSoft)
            .multilineTextAlignment(.center)
            .frame(maxWidth: .infinity)
            .padding(.top, 40)
    }

    /// One conversation's matches.
    ///
    /// Every excerpt is its own button: the anchored jump means each hit is a
    /// distinct destination, so a card-wide tap target would show the reader the
    /// line they wanted and then land them on a different one.
    private func card(_ group: ChatSearchGroup) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text(verbatim: title(for: group))
                    .font(Theme.mono(14, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                Spacer(minLength: 8)
                Text(verbatim: String(group.totalHits))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
            }

            ForEach(group.hits.prefix(Self.excerptsPerCard), id: \.ordinal) { hit in
                Button {
                    appStore.openSearchResult(sessionId: group.sessionId, ordinal: hit.ordinal)
                } label: {
                    excerpt(hit)
                }
                .buttonStyle(.plain)
                .accessibilityIdentifier("search.hit.\(group.sessionId).\(hit.ordinal)")
            }

            if group.totalHits > Int64(min(group.hits.count, Self.excerptsPerCard)) {
                let more = group.totalHits - Int64(min(group.hits.count, Self.excerptsPerCard))
                Text(verbatim: lang.t("search.more", String(more)))
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: Theme.radius, style: .continuous)
                .fill(Theme.ink.opacity(0.035))
        )
    }

    private func excerpt(_ hit: ChatSearchHit) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            highlighted(hit.text)
                .font(Theme.mono(12))
                .lineLimit(3)
                .multilineTextAlignment(.leading)
                .fixedSize(horizontal: false, vertical: true)
            Text(verbatim: hit.role.uppercased())
                .font(Theme.mono(9))
                .foregroundStyle(Theme.inkSoft)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.leading, 8)
        .overlay(alignment: .leading) {
            Rectangle()
                .fill(Theme.ink.opacity(0.18))
                .frame(width: 2)
        }
        .contentShape(Rectangle())
    }

    /// The matched terms, marked. `SearchSnippet` is the port of app/web's
    /// `searchSnippet.ts`, held to it by shared vectors — so what this bolds is
    /// what the other client bolds.
    private func highlighted(_ text: String) -> Text {
        SearchSnippet.snippet(text, query: query.trimmingCharacters(in: .whitespaces))
            .reduce(Text(verbatim: "")) { acc, segment in
                let piece = Text(verbatim: segment.text)
                    .foregroundColor(segment.match ? Theme.ink : Theme.inkSoft)
                return acc + (segment.match ? piece.bold() : piece)
            }
    }

    /// Prefer THIS device's row for the name, so one conversation is not called
    /// two different things in two places: the list falls back to a short
    /// snippet of the last user message before the title pass has run, and the
    /// search response carries no such field to fall back to.
    private func title(for group: ChatSearchGroup) -> String {
        if let row = index.rows.first(where: { $0.id == group.sessionId }) {
            let headline = SessionHeadline.text(title: row.title, userText: row.userText)
            if !headline.isEmpty { return headline }
        }
        if let title = group.sessionTitle, !title.isEmpty { return title }
        return lang.t("search.untitled")
    }

}
