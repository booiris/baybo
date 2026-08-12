import SwiftUI
import UIKit

/// The hand-rolled stop-and-decide confirm — the pairing screen's grammar
/// folded onto one floating paper plate: bold mono title, soft mono
/// hint, the practiced Cancel | commit pill row. Ink dims the field (never
/// black, never blur — glass stays rationed to the composer surfaces); the
/// commit is a single outline pill in the destructive red.
///
/// Hand-rolled on purpose: the stock `.confirmationDialog` left its
/// `isPresented` binding latched true after a scrim dismiss inside the
/// TabView shell, deadening the trigger. Here the presenting flag is ours, the
/// scrim tap writes it false explicitly, and the overlay mounts at the app
/// root so it covers — and hit-blocks — the Liquid Glass tab bar.
struct ConfirmDialog: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject private var lang = Lang.shared
    /// Scrim-cancel arms only after the entrance settles: a fast second tap
    /// lands where the trigger was — scrim, once the dialog is presenting —
    /// and would cancel a dialog the user never saw (haptic fires, nothing
    /// appears, the trigger reads as dead). The card's buttons are never gated.
    @State private var scrimArmed = false
    @State private var pulse = false

    private static let scrimGraceMs = 400

    let titleKey: String
    let bodyKey: String
    /// Substituted into `bodyKey`'s single `%@` when the copy names something the
    /// user must see before committing (a cron group's delete counts the
    /// execution records it will clear). Nil for a body that takes no argument.
    var bodyArg: String?
    let commitKey: String
    let onCancel: () -> Void
    let onConfirm: () -> Void

    /// Presentation curves for the flip sites (`withAnimation` at the toggle,
    /// not `.animation(value:)` — enter and exit deliberately differ: a
    /// critically damped settle in, a quick fade out, no scale recoil).
    static var enterMotion: Animation {
        UIAccessibility.isReduceMotionEnabled
            ? .easeOut(duration: 0.15)
            : .spring(response: 0.34, dampingFraction: 1)
    }

    static var exitMotion: Animation {
        .easeIn(duration: UIAccessibility.isReduceMotionEnabled ? 0.12 : 0.14)
    }

    var body: some View {
        ZStack {
            Theme.ink.opacity(0.35)
                .ignoresSafeArea()
                .contentShape(Rectangle())
                .onTapGesture {
                    if scrimArmed {
                        onCancel()
                    } else {
                        acknowledgeEarlyTap()
                    }
                }
                .transition(.opacity)

            card
        }
        .task {
            try? await Task.sleep(for: .milliseconds(Self.scrimGraceMs))
            scrimArmed = true
        }
    }

    /// An unarmed scrim tap is deliberately not a cancel, but it must not be
    /// silent either: a light haptic acknowledges the touch, and a brief card
    /// pulse points at the button row (the pulse alone would leave the Reduce
    /// Motion cohort with the exact dead-trigger feel this dialog exists to
    /// prevent — haptics are not motion, so the tick stays).
    private func acknowledgeEarlyTap() {
        Haptics.tap()
        guard !reduceMotion, !pulse else { return }
        withAnimation(.spring(response: 0.18, dampingFraction: 0.5)) { pulse = true }
        Task {
            try? await Task.sleep(for: .milliseconds(160))
            withAnimation(.easeOut(duration: 0.12)) { pulse = false }
        }
    }

    private var card: some View {
        VStack(spacing: 0) {
            Text(verbatim: lang.t(titleKey))
                .font(Theme.mono(16, weight: .bold))
                .foregroundStyle(Theme.ink)

            Text(verbatim: bodyArg.map { lang.t(bodyKey, $0) } ?? lang.t(bodyKey))
                .font(Theme.mono(14))
                .foregroundStyle(Theme.inkSoft)
                .lineSpacing(5)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .padding(.top, 10)

            HStack(spacing: 12) {
                // Filled ink = the safe default (HIG: never the destructive one).
                Button(action: onCancel) {
                    Text(verbatim: lang.t("common.cancel"))
                }
                .buttonStyle(FilledPillButtonStyle())

                Button {
                    Haptics.tap()
                    onConfirm()
                } label: {
                    Text(verbatim: lang.t(commitKey))
                }
                // Red is the destructive token, and this dialog is now the only
                // place the design system spends it: every remaining confirm
                // takes something away. The commit that does NOT — the
                // per-session resync — no longer asks at all, which is a better
                // answer than a red-less dialog teaching the field that red just
                // means "a dialog".
                .buttonStyle(OutlinePillButtonStyle(color: Theme.err))
            }
            .padding(.top, 22)
        }
        .padding(.init(top: 26, leading: 24, bottom: 20, trailing: 24))
        .background(
            Theme.paper,
            in: .rect(cornerRadius: Theme.radiusModal, style: .continuous)
        )
        .overlay(
            RoundedRectangle(cornerRadius: Theme.radiusModal, style: .continuous)
                .strokeBorder(Theme.line, lineWidth: 1)
        )
        .shadow(color: Theme.ink.opacity(0.16), radius: 32, y: 12)
        .shadow(color: Theme.ink.opacity(0.06), radius: 2, y: 1)
        .frame(maxWidth: 320)
        .padding(.horizontal, 36)
        // Optical center: the dimmed tab bar weights the bottom edge, so a
        // geometrically centered card reads low.
        .offset(y: -12)
        .scaleEffect(pulse ? 1.02 : 1)
        .accessibilityAddTraits(.isModal)
        .accessibilityAction(.escape, onCancel)
        .transition(
            reduceMotion
                ? .opacity
                : .asymmetric(
                    insertion: .opacity.combined(with: .scale(scale: 0.96)),
                    removal: .opacity)
        )
    }
}
