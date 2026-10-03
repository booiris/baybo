import SwiftUI

/// The gateway's configured LLM entries, pushed from the Settings Models row —
/// `CronJobsScreen`'s shape (covers the tab bar, pops with the edge swipe) over
/// a different kind of thing.
///
/// **Entries, not models.** A row is a `baybo.json` `llm` entry: a provider, a
/// credential and the set of models it may serve. The ✓ marks `default-llm` —
/// what a conversation with no pin of its own runs on. Tapping a row opens its
/// editor; setting the default happens THERE, not here, because promotion is
/// the one action on this surface whose blast radius is every device.
///
/// **Mirror-painted, then reconciled.** Unlike `CronJobsScreen` (fetched, never
/// mirrored) this list reads `ModelCatalog`, which paints from `models.json`
/// with zero network and reconciles once per run. That is the right trade here
/// and the wrong one there: a schedule must answer "what is set up right now",
/// while this list's whole job is to be openable on a phone with no signal.
struct LlmEntriesScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @ObservedObject private var catalog = ModelCatalog.shared
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if catalog.models.isEmpty {
                    message(lang.t("llm.empty"))
                } else {
                    entryList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            header
        }
        .background(Theme.paper)
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .onAppear { catalog.refreshIfNeeded() }
    }

    private var header: some View {
        ZStack {
            Text(verbatim: lang.t("llm.title"))
                .font(Theme.mono(16))
                .foregroundStyle(Theme.ink)

            HStack {
                Button {
                    dismiss()
                } label: {
                    Image(systemName: "chevron.left")
                        .font(.system(size: 18, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 42, height: 42)
                }
                .glassSurface(interactive: true, in: .circle)
                .accessibilityLabel(Text(verbatim: lang.t("chat.back")))

                Spacer()
            }
        }
        .padding(.horizontal, 24)
        .frame(height: ChatHeaderView.barHeight)
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    private var entryList: some View {
        ScrollView {
            VStack(spacing: 0) {
                ForEach(catalog.models, id: \.name) { entry in
                    entryRow(entry)
                    if entry.name != catalog.models.last?.name {
                        Rectangle().fill(Theme.line).frame(height: 1)
                    }
                }
            }
            .padding(.horizontal, 24)
            .padding(.top, ChatHeaderView.barHeight + 12)
            .padding(.bottom, 40)
        }
        .scrollContentBackground(.hidden)
    }

    private func entryRow(_ entry: LlmModelInfo) -> some View {
        Button {
            Haptics.tap()
            appStore.openLlmEntry(entry.name)
        } label: {
            HStack(spacing: 12) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(verbatim: entry.name)
                        .font(Theme.mono(15))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                    // One run of text, one separator throughout. Rendering the
                    // key state as a sibling `Text` put a bare space between it
                    // and the model, and `openai · gpt-5.5 no key` reads as a
                    // model called "gpt-5.5 no key".
                    //
                    // Deliberately NOT red either: `ollama` takes an optional
                    // key and `llamafile` none at all, so a healthy local entry
                    // sits here forever, and colouring it would train the eye to
                    // ignore the one hue this design reserves for state.
                    Text(verbatim: subtitle(entry))
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.inkSoft)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                Spacer(minLength: 8)
                if entry.name == catalog.defaultName {
                    Image(systemName: "checkmark")
                        .font(.system(size: 13, weight: .semibold))
                        .foregroundStyle(Theme.ink)
                }
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(Theme.line)
            }
            .padding(.vertical, 14)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("llm-entry-\(entry.name)")
        .accessibilityLabel(Text(verbatim: entry.name))
        .accessibilityValue(
            Text(verbatim: entry.name == catalog.defaultName ? lang.t("llm.isDefault") : ""))
    }

    /// `provider · model`, plus the key state when there isn't one — the two
    /// facts that decide whether this entry needs the operator's attention.
    private func subtitle(_ entry: LlmModelInfo) -> String {
        var parts = [entry.provider, entry.model]
        if !entry.apiKeyConfigured { parts.append(lang.t("llm.noKey")) }
        return parts.joined(separator: " · ")
    }

    private func message(_ text: String) -> some View {
        Text(verbatim: text)
            .font(Theme.sys(14))
            .foregroundStyle(Theme.inkSoft)
            .multilineTextAlignment(.center)
            .padding(.horizontal, 48)
    }
}
