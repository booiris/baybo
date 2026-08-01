import SwiftUI

/// The owner's live scheduled jobs, pushed on the outer NavigationStack from the
/// Chats header's ☰ menu — `ArchivedScreen`'s shape (covers the tab bar, pops
/// with the edge swipe), over a different kind of thing.
///
/// **Jobs, not fires.** Everywhere else in this app a cron job appears only as
/// the *conversations it produced* — a **cron group**, which is a view over its
/// fires (`docs/cron-groups.md`). That means a job that has not fired yet is
/// invisible: no fire, no session, no row. This screen is the schedule itself,
/// so a job created five minutes ago for next Monday shows up here and nowhere
/// else. Tapping one pushes its group, which for such a job is legitimately
/// empty — the schedule exists, the history does not yet.
///
/// **Fetched, never mirrored.** Unlike the chat list (`SessionIndex`, a
/// persisted registry that must paint cold on launch), this is a menu-depth
/// reference view: it loads on appear and shows its work. Persisting it would
/// buy a faster second open in exchange for a cache that can disagree with the
/// gateway about what is scheduled — a bad trade for a screen whose entire job
/// is to answer "what is set up right now".
struct CronJobsScreen: View {
    @EnvironmentObject private var appStore: AppStore
    @ObservedObject private var lang = Lang.shared
    @Environment(\.dismiss) private var dismiss

    /// Whether the FIRST load failed. A later refresh that fails keeps the rows
    /// it has and says so on the notice line instead — a pull-to-refresh with no
    /// signal must not empty the list it just showed.
    @State private var failed = false

    var body: some View {
        ZStack(alignment: .top) {
            Group {
                if let jobs = appStore.cronJobs {
                    if jobs.isEmpty {
                        message(lang.t("cronJobs.empty"))
                    } else {
                        jobList(jobs)
                    }
                } else if failed {
                    message(lang.t("cronJobs.failed"))
                } else {
                    ProgressView().tint(Theme.inkSoft)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)

            header
        }
        .background(Theme.paper)
        // The system nav bar is hidden (custom chrome), which also disables the
        // interactive pop — this presence-only host re-enables the edge swipe.
        .background(PopGestureEnabler().frame(width: 0, height: 0))
        .task { await load() }
    }

    /// Back chevron + centered title over the shared paper veil — `ArchivedScreen`'s
    /// header verbatim.
    private var header: some View {
        VStack(spacing: 6) {
            ZStack {
                Text(verbatim: lang.t("cronJobs.title"))
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

            if let notice = appStore.cronJobNotice {
                Text(verbatim: notice)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.err)
                    .multilineTextAlignment(.center)
                    .padding(.horizontal, 24)
            }
        }
        .frame(maxWidth: .infinity)
        .background(alignment: .top) { veil }
    }

    private var veil: some View {
        LinearGradient(stops: ChatHeaderView.veilStops, startPoint: .top, endPoint: .bottom)
            .ignoresSafeArea(edges: .top)
            .allowsHitTesting(false)
    }

    private func jobList(_ jobs: [CronJobSummary]) -> some View {
        List {
            ForEach(jobs, id: \.id) { job in
                Button {
                    appStore.openCronGroup(job.id)
                } label: {
                    CronJobRowView(job: job, langCode: lang.current.lproj)
                }
                .listRowBackground(Theme.paper)
                .listRowSeparatorTint(Theme.line)
                .listRowInsets(EdgeInsets(top: 0, leading: 24, bottom: 0, trailing: 24))
                // Not a full swipe: delete is destructive, and pause is a state
                // flip on a thing that runs unattended — both want the tap.
                .swipeActions(edge: .trailing, allowsFullSwipe: false) {
                    let paused = job.status == .disabled
                    Button {
                        Haptics.tap()
                        appStore.requestCronPaused(job.id, paused: !paused)
                    } label: {
                        Label(
                            lang.t(paused ? "cronJobs.resume" : "cronJobs.pause"),
                            systemImage: paused ? "play.circle" : "pause.circle")
                    }
                    .tint(Theme.inkSoft)
                    // A built-in job is part of Baybo: pause is the way to
                    // stop it, and the server refuses DELETE outright — so
                    // offering one would only ever produce a failed request.
                    if !job.builtin {
                        Button(role: .destructive) {
                            appStore.promptDeleteCronJob(
                                job.id, name: CronJobRowView.headline(job))
                        } label: {
                            Label(lang.t("list.delete"), systemImage: "trash")
                        }
                        .tint(Theme.err)
                    }
                }
            }
        }
        .listStyle(.plain)
        .scrollContentBackground(.hidden)
        .contentMargins(.top, ChatListScreen.topContentMargin, for: .scrollContent)
        .scrollBounceBehavior(.always)
        .refreshable { await load() }
    }

    private func message(_ text: String) -> some View {
        VStack {
            Spacer()
            Text(verbatim: text)
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 40)
            Spacer()
        }
        .frame(maxWidth: .infinity)
    }

    /// A failed reload keeps the rows it already has: a pull-to-refresh with no
    /// signal must not empty the list it just showed. Only a first load with
    /// nothing to fall back on gets the failure page.
    private func load() async {
        do {
            try await appStore.loadCronJobs()
            failed = false
            appStore.cronJobNotice = nil
        } catch {
            if appStore.cronJobs == nil {
                failed = true
            } else {
                appStore.cronJobNotice = lang.t("cronJobs.failed")
            }
        }
    }
}

extension CronJobSummary {
    /// A copy with a new run state. UniFFI records are immutable (`let` fields),
    /// so an optimistic flip mints one rather than mutating in place.
    func with(status: CronJobStatus, nextTriggerAt: String?) -> CronJobSummary {
        CronJobSummary(
            id: id,
            title: title,
            prompt: prompt,
            expr: expr,
            timezone: timezone,
            status: status,
            nextTriggerAt: nextTriggerAt,
            lastTriggeredAt: lastTriggeredAt,
            builtin: builtin)
    }
}

/// One scheduled job, in the chat list's row grammar: the `alarm` glyph + a bold
/// name, a grey line saying when it runs, and a right column saying what happens
/// next.
///
/// It does NOT go through `ChatRowBody`, which every chat-list row shares. That
/// body is built for a conversation — `lastActive` + `unread` — and its time
/// column literally clamps `min(date, now)`, because a message cannot arrive in
/// the future. A job's whole point is that its next run IS in the future, so
/// reusing it would render "now" on every row. Same look, different thing.
struct CronJobRowView: View {
    let job: CronJobSummary
    /// The app language's locale identifier, so the relative time can't drift
    /// from the chrome language.
    let langCode: String

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 5) {
                    // The same glyph the cron GROUP row wears in the chat list —
                    // one visual idea for "scheduled", whichever side of it the
                    // user is looking at (the fires, or the schedule).
                    Image(systemName: "alarm")
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(Theme.ink)
                        .accessibilityHidden(true)
                    Text(verbatim: Self.headline(job))
                        .font(Theme.sys(15, weight: .bold))
                        .foregroundStyle(Theme.ink)
                        .lineLimit(1)
                        .truncationMode(.tail)
                }
                Text(verbatim: Self.scheduleLabel(job, locale: langCode))
                    .font(Theme.sys(14))
                    .foregroundStyle(Theme.inkSoft)
                    .lineLimit(2)
                    .lineSpacing(3)
                    .truncationMode(.tail)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            Text(verbatim: Self.nextLabel(job, locale: langCode))
                .font(Theme.sys(11))
                .foregroundStyle(Theme.inkSoft)
                .fixedSize(horizontal: true, vertical: false)
        }
        .frame(minHeight: 52)
        .padding(.vertical, 15)
        .contentShape(Rectangle())
    }

    /// The job's name — falling back to the prompt for a row minted before
    /// `title` existed, the same choice the web table and the CLI make
    /// (`display_title`). A nameless row would otherwise be an alarm glyph and a
    /// void.
    static func headline(_ job: CronJobSummary) -> String {
        job.title.isEmpty ? job.prompt : job.title
    }

    /// When it runs. The timezone rides along because it is not decoration:
    /// `0 9 * * *` says nothing without whose 9am, and a phone that travels is
    /// exactly where that bites.
    ///
    /// Words where we can manage them, the raw expression where we cannot —
    /// `CronExpression` never guesses (and note its day-of-week trap: `1-5` is
    /// Sunday..Thursday here, and it says so).
    static func scheduleLabel(_ job: CronJobSummary, locale: String) -> String {
        "\(CronExpression.humanize(job.expr, lproj: locale)) · \(job.timezone)"
    }

    /// The right column: what happens NEXT, which is only a time when a time is
    /// actually coming. A paused job and a spent one-shot both have no next
    /// trigger, and saying so is the whole reason they are distinguishable from
    /// a live job in a read-only list that offers no way to ask.
    static func nextLabel(_ job: CronJobSummary, locale: String) -> String {
        switch job.status {
        case .disabled:
            return Lang.shared.t("cronJobs.paused")
        case .executed:
            return Lang.shared.t("cronJobs.done")
        case .enabled:
            guard let next = job.nextTriggerAt, let date = Self.parse(next) else { return "" }
            let formatter = RelativeDateTimeFormatter()
            formatter.locale = Locale(identifier: locale)
            formatter.unitsStyle = .abbreviated
            // Relative, not absolute: "in 3h" answers the question this list is
            // asked ("when does it next run"), in one column's width, without
            // making the reader do date arithmetic against today.
            return formatter.localizedString(for: date, relativeTo: Date())
        }
    }

    /// The gateway stamps RFC 3339 with fractional seconds on some fields and
    /// without on others, and `ISO8601DateFormatter` fails the variant it was
    /// not configured for — so try both rather than silently render nothing.
    static func parse(_ rfc3339: String) -> Date? {
        let withFraction = ISO8601DateFormatter()
        withFraction.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = withFraction.date(from: rfc3339) { return date }
        return ISO8601DateFormatter().date(from: rfc3339)
    }
}
