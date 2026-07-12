import SwiftUI

/// The composer's approval prompt: a tool call is blocked until the user
/// answers. Native (not web) because the pending set is native — see
/// `ChatStore.pendingApprovals`.
///
/// It mounts INSIDE the composer dock, above the pill, so it rides the keyboard
/// with the dock and inflates the transcript's bottom inset (the webview never
/// scrolls under it — unlike the jump button, which deliberately floats).
///
/// Only the head of the queue is answerable; the rest are counted. A turn can
/// fire parallel tool calls and block on several at once, and a stack of cards
/// over a phone composer is a worse prompt than one clear question.
struct ApprovalCardView: View {
    let approval: PendingApproval
    /// How many prompts are waiting behind this one.
    let queued: Int
    let onDecide: (ApprovalDecision) -> Void

    @ObservedObject private var lang = Lang.shared
    /// Answering is one-shot: the card is dismissed optimistically, but a
    /// double-tap inside the same frame would fire twice.
    @State private var decided = false
    /// Natural height of the detail body, measured to decide whether it needs to
    /// become a scroller.
    @State private var detailHeight: CGFloat = 0

    /// Ceiling on the detail box (~9 lines of the 12pt mono). Past this the body
    /// scrolls: the card must stay a prompt over the composer, not a page.
    private static let maxDetailHeight: CGFloat = 140

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text(verbatim: String(format: lang.t("chat.approvalTitle"), approval.tool))
                    .font(Theme.mono(13, weight: .bold))
                    .foregroundStyle(Theme.ink)
                Spacer(minLength: 0)
                if queued > 0 {
                    Text(verbatim: String(format: lang.t("chat.approvalQueued"), queued))
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.inkSoft)
                }
            }

            if let description = approval.description, !description.isEmpty {
                Text(verbatim: description)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.inkSoft)
            }

            detail

            // Two answers only. The gate also accepts an "approve always" —
            // a session-wide grant for every resource the call touches — and
            // other clients offer it, but a phone is where a mis-tap is
            // likeliest and a standing grant is the one decision you can't take
            // back by paying attention next time. A verdict from elsewhere
            // still RENDERS here (the transcript labels it), it just can't be
            // given here.
            HStack(spacing: 8) {
                Button {
                    decide(.deny)
                } label: {
                    Text(verbatim: lang.t("chat.approvalDeny"))
                }
                .buttonStyle(CompactPillButtonStyle(fill: nil, color: Theme.err))

                Button {
                    decide(.approve)
                } label: {
                    Text(verbatim: lang.t("chat.approvalApprove"))
                }
                .buttonStyle(CompactPillButtonStyle(fill: Theme.ink, color: Theme.paper))
            }
        }
        .padding(14)
        .background(Theme.paper, in: RoundedRectangle(cornerRadius: Theme.radius))
        .overlay(
            RoundedRectangle(cornerRadius: Theme.radius)
                .strokeBorder(Theme.ink, lineWidth: 1)
        )
        .padding(.horizontal, 18)
        .transition(.move(edge: .bottom).combined(with: .opacity))
    }

    /// What the call actually wants to do — the ONE thing the user judges on.
    ///
    /// Never truncated. `ResourceAccess::ExecCommand` carries the raw command in
    /// full, and a shell line's tail is exactly where the teeth are
    /// (`… && docker system prune -af --volumes`): a clipped view reads as a
    /// harmless cleanup while the destructive half sits past the ellipsis. So a
    /// long body SCROLLS inside a bounded box instead — every character
    /// reachable, and the card still can't grow tall enough to push the
    /// transcript off the screen. Short bodies size naturally; only an
    /// overflowing one turns into a scroller (and wears the fade that says so).
    @ViewBuilder private var detail: some View {
        let overflows = detailHeight > Self.maxDetailHeight
        Group {
            if overflows {
                ScrollView { detailBody }
                    .frame(height: Self.maxDetailHeight)
                    // Bottom fade — the same "there is more past this edge" cue
                    // the header and composer veils use. The scroll indicator
                    // only shows up mid-gesture, which is too late to be told
                    // there was more to read.
                    .overlay(alignment: .bottom) {
                        LinearGradient(
                            colors: [Theme.paper.opacity(0), Theme.paper],
                            startPoint: .top,
                            endPoint: .bottom
                        )
                        .frame(height: 20)
                        .allowsHitTesting(false)
                    }
            } else {
                detailBody
            }
        }
    }

    private var detailBody: some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(approval.accesses, id: \.self) { access in
                Text(verbatim: access)
                    .font(Theme.mono(12))
                    .foregroundStyle(Theme.ink)
                    .fixedSize(horizontal: false, vertical: true)
            }
            // Shown ALONGSIDE the accesses, not only when they're missing: a
            // tool's declared resources can be coarser than its parameters (a
            // fetch declares a host, but the URL and body live here), and the
            // user would be judging the coarse version.
            if !approval.paramsPreview.isEmpty {
                Text(verbatim: approval.paramsPreview)
                    .font(Theme.mono(11))
                    .foregroundStyle(Theme.inkSoft)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { detailHeight = $0 }
    }

    private func decide(_ decision: ApprovalDecision) {
        guard !decided else { return }
        decided = true
        onDecide(decision)
    }
}

/// The card's own pill: shorter than `Theme`'s standard pills (which size for a
/// full-page CTA and read bulky stacked inside a card over the composer). Local
/// rather than a new Theme style — nothing else wants this proportion.
///
/// `fill: nil` draws the stroke-only variant. The 44pt floor is the HIG minimum
/// hit target and is the real lower bound here: the padding may shrink, the
/// tappable area may not — this is the one control in the app where a mis-tap
/// runs a command the user meant to refuse.
private struct CompactPillButtonStyle: ButtonStyle {
    let fill: Color?
    let color: Color

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(Theme.mono(14))
            .foregroundStyle(color)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, minHeight: 44)
            .background {
                if let fill {
                    Capsule().fill(fill)
                }
            }
            // Stroke-only pills don't hit-test their interior without a shape.
            .contentShape(Capsule())
            .overlay(fill == nil ? Capsule().strokeBorder(color, lineWidth: 1) : nil)
            .opacity(configuration.isPressed ? 0.7 : 1)
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
    }
}
