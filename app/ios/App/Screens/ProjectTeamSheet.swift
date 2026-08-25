import SwiftUI

/// Who is on a board.
///
/// A sheet rather than a pushed screen: it is a reference, not a place — you
/// open it to see who `@docs-1` is or to change what an agent runs on, and
/// then you are back on the board. A push would put it in the back chain
/// between the board and a card.
struct ProjectTeamSheet: View {
    let projectId: String
    var client: any BayboClientProtocol = Baybo.client

    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss
    @State private var team: [TeamMemberInfo] = []
    @State private var loaded = false
    /// The last read failed. Kept apart from "empty" for the reason the
    /// subagent sheet keeps them apart: an older gateway 404s this route, and
    /// answering that with "this board has no team" is a lie about the board
    /// rather than a fact about the connection.
    @State private var failed = false
    @State private var profile: TeamMemberInfo?
    @State private var removing: TeamMemberInfo?
    @State private var error: String?

    private var monograms: [String: String] { AgentMonogram.map(for: team) }

    var body: some View {
        VStack(spacing: 0) {
            titleRow
            Rectangle().fill(Theme.lineStrong).frame(height: 1)
            content
        }
        .background(Theme.paper)
        .task { await load() }
        .sheet(item: $profile) { member in
            AgentProfileSheet(member: member, projectId: projectId, client: client) {
                Task { await load() }
            }
            .presentationDetents([.medium, .large])
            .presentationDragIndicator(.visible)
            .presentationBackground(Theme.paper)
        }
        .confirmationDialog(
            lang.t("team.removeTitle", removing.map { "@\($0.handle)" } ?? ""),
            isPresented: Binding(get: { removing != nil }, set: { if !$0 { removing = nil } }),
            titleVisibility: .visible
        ) {
            Button(lang.t("team.remove"), role: .destructive) {
                if let member = removing { remove(member) }
            }
            Button(lang.t("common.cancel"), role: .cancel) { removing = nil }
        } message: {
            Text(verbatim: lang.t("team.removeExplain"))
        }
    }

    private var titleRow: some View {
        ZStack {
            Text(verbatim: lang.t("team.title"))
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
            HStack {
                Spacer()
                Button(lang.t("common.done")) { dismiss() }
                    .buttonStyle(LinkButtonStyle())
            }
        }
        .padding(.horizontal, 16)
        .frame(height: 52)
    }

    @ViewBuilder private var content: some View {
        if let error {
            Text(verbatim: error)
                .font(Theme.sys(12.5))
                .foregroundStyle(Theme.err)
                .padding(20)
        } else if !loaded {
            ProgressView().tint(Theme.inkSoft).frame(maxWidth: .infinity, maxHeight: .infinity)
        } else if failed {
            Text(verbatim: lang.t("team.failed"))
                .font(Theme.sys(13))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
                .padding(30)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            List {
                ForEach(team, id: \.id) { member in
                    Button {
                        Haptics.tap()
                        profile = member
                    } label: {
                        row(member)
                    }
                    .buttonStyle(.plain)
                    .listRowInsets(EdgeInsets(top: 0, leading: 16, bottom: 0, trailing: 16))
                    .listRowBackground(Theme.paper)
                    .accessibilityIdentifier("team-row-\(member.handle)")
                    .swipeActions(edge: .trailing, allowsFullSwipe: false) {
                        // The lead is the coordinator every board has and none
                        // may remove — the server refuses it, so the swipe does
                        // not offer it rather than offering it and being told no.
                        if !member.lead {
                            Button(role: .destructive) { removing = member } label: {
                                Label(lang.t("team.remove"), systemImage: "person.badge.minus")
                            }
                        }
                    }
                }
            }
            .listStyle(.plain)
            .scrollContentBackground(.hidden)
        }
    }

    private func row(_ member: TeamMemberInfo) -> some View {
        HStack(spacing: 10) {
            AgentFace(
                handle: member.handle, monogram: monograms[member.id],
                avatarBlobId: member.avatarBlobId, lead: member.lead)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(verbatim: "@\(member.handle)")
                        .font(Theme.sys(14, weight: .medium))
                        .foregroundStyle(Theme.ink)
                    if member.lead {
                        Text(verbatim: lang.t("team.lead"))
                            .font(Theme.mono(9))
                            .textCase(.uppercase)
                            .kerning(0.8)
                            .foregroundStyle(Theme.inkSoft)
                            .padding(.horizontal, 5)
                            .frame(height: 15)
                            .overlay(Capsule().strokeBorder(Theme.lineStrong, lineWidth: 1))
                    }
                }
                Text(verbatim: Self.subtitle(member))
                    .font(Theme.mono(10.5))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(1)
            }
            Spacer(minLength: 6)
            Image(systemName: "chevron.right")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(Theme.lineStrong)
        }
        .frame(minHeight: 56)
        .contentShape(Rectangle())
    }

    /// What this teammate runs on, or what it is. The framework is worth
    /// saying: only a `baybo` teammate can host a card's session, so only one
    /// can be assigned — an operator wondering why `@codex-1` is missing from
    /// the assignee picker finds the answer here.
    static func subtitle(_ member: TeamMemberInfo) -> String {
        var parts: [String] = []
        if member.framework != "baybo" { parts.append(member.framework) }
        if let model = member.model {
            parts.append(model)
        } else if let llm = member.llm {
            parts.append(llm)
        }
        if !member.description.isEmpty { parts.append(member.description) }
        return parts.joined(separator: " · ")
    }

    private func load() async {
        do {
            team = try await client.projectTeam(projectId: projectId)
            failed = false
        } catch {
            failed = true
        }
        loaded = true
    }

    private func remove(_ member: TeamMemberInfo) {
        removing = nil
        Task {
            do {
                try await client.projectRemoveAgent(projectId: projectId, agentId: member.id)
                await load()
            } catch {
                // The server's own sentence: it names whether the refusal was
                // the lead or a run in flight, and those want different things
                // done about them.
                self.error = ProjectsStore.message(from: error)
            }
        }
    }
}

/// The record already carries an `id` (the agent's profile id), so the
/// conformance is a declaration and nothing else — a computed `id` here would
/// be a second answer to which agent this is.
extension TeamMemberInfo: @retroactive Identifiable {}
