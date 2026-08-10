import SwiftUI
import UIKit

/// The rename editor: `ConfirmDialog`'s floating paper plate with a field where
/// its body copy sits — bold mono title, the input, the practiced
/// Cancel | commit pill row.
///
/// Hand-rolled for the reasons the confirm is (a stock presentation left its
/// `isPresented` latched true inside the TabView shell, and only an app-root
/// overlay dims and hit-blocks the Liquid Glass tab bar), plus one of its own:
/// it is the single dialog here that raises a keyboard, and the card — not the
/// shell dimmed behind the scrim — is what has to move for it. So the whole
/// overlay opts out of SwiftUI's automatic avoidance and lifts itself, and the
/// list surfaces it floats over opt out too (`.ignoresSafeArea(.keyboard)` on
/// `HomeTabView` / `ArchivedScreen` / `CronGroupScreen`) rather than sliding
/// under the scrim while the user types.
struct RenameDialog: View {
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @ObservedObject private var lang = Lang.shared
    @FocusState private var editing: Bool
    @State private var draft: String
    /// Live keyboard height in points, tracked here rather than through the safe
    /// area: see the type's note on why this overlay owns its own avoidance.
    @State private var keyboardHeight: CGFloat = 0

    /// What an untouched field means (`RenameTitle.toCommit`), snapshotted with
    /// the dialog by `AppStore.PendingRename`.
    let seed: String
    let onCancel: () -> Void
    /// The normalized title to store. Never called for an empty or unchanged
    /// field — that path is a plain dismiss.
    let onCommit: (String) -> Void

    init(seed: String, onCancel: @escaping () -> Void, onCommit: @escaping (String) -> Void) {
        self.seed = seed
        self.onCancel = onCancel
        self.onCommit = onCommit
        _draft = State(initialValue: seed)
    }

    /// Whether the field holds anything the server would accept. Only EMPTY
    /// disables the commit: an unchanged title leaves it live, and commits
    /// nothing — a button greyed out because the user has not typed yet reads as
    /// broken, where one that quietly closes reads as Cancel, which is what it is.
    private var committable: Bool {
        !RenameTitle.normalized(draft).isEmpty
    }

    var body: some View {
        ZStack {
            Theme.ink.opacity(0.35)
                .ignoresSafeArea()
                .contentShape(Rectangle())
                // No scrim-arming grace here, unlike `ConfirmDialog`: this
                // dialog opens from a context-menu row, and dismissing that menu
                // eats the touch that would have raced the entrance.
                .onTapGesture(perform: onCancel)
                .transition(.opacity)

            card
                // Centred in what the keyboard leaves, not on the screen: at
                // full height the card's foot lands within a few points of the
                // keyboard on a 6.3" phone, and under it on anything smaller.
                .padding(.bottom, keyboardHeight)
                .animation(.easeOut(duration: 0.25), value: keyboardHeight)
        }
        .ignoresSafeArea(.keyboard)
        .onReceive(
            NotificationCenter.default.publisher(
                for: UIResponder.keyboardWillChangeFrameNotification)
        ) { note in
            keyboardHeight = Self.overlap(of: note)
        }
        .onReceive(
            NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification)
        ) { _ in
            keyboardHeight = 0
        }
        .task {
            // A frame's grace: focusing during the entrance transition drops the
            // request on the floor often enough to be the thing users report as
            // "I had to tap the field".
            try? await Task.sleep(for: .milliseconds(120))
            editing = true
        }
    }

    /// How far the keyboard covers the window, in points. Read off the
    /// notification's end frame rather than assumed: the floating iPad keyboard
    /// and the undocked one report frames that overlap nothing.
    private static func overlap(of note: Notification) -> CGFloat {
        guard
            let frame = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect,
            let screen = UIApplication.shared.connectedScenes
                .compactMap({ $0 as? UIWindowScene }).first?.screen
        else { return 0 }
        return max(0, screen.bounds.maxY - frame.minY)
    }

    private var card: some View {
        VStack(spacing: 0) {
            Text(verbatim: lang.t("list.renameTitle"))
                .font(Theme.mono(16, weight: .bold))
                .foregroundStyle(Theme.ink)

            TextField(lang.t("list.renamePlaceholder"), text: $draft)
                .font(Theme.mono(15))
                .foregroundStyle(Theme.ink)
                .textInputAutocapitalization(.sentences)
                .autocorrectionDisabled()
                .submitLabel(.done)
                .focused($editing)
                .onSubmit(commit)
                // The server's cap, enforced as the user types rather than at
                // the commit: a title silently shortened on send is a rename the
                // user did not make.
                .onChange(of: draft) { _, typed in
                    let capped = RenameTitle.cap(typed)
                    if capped != typed { draft = capped }
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(RoundedRectangle(cornerRadius: Theme.radius).fill(Theme.paper))
                .overlay(
                    RoundedRectangle(cornerRadius: Theme.radius)
                        .strokeBorder(editing ? Theme.ink : Theme.line, lineWidth: 1)
                )
                .padding(.top, 16)

            HStack(spacing: 12) {
                Button(action: onCancel) {
                    Text(verbatim: lang.t("common.cancel"))
                }
                .buttonStyle(FilledPillButtonStyle())

                Button(action: commit) {
                    Text(verbatim: lang.t("common.save"))
                }
                .buttonStyle(OutlinePillButtonStyle(color: Theme.ink))
                .disabled(!committable)
                .opacity(committable ? 1 : 0.35)
            }
            .padding(.top, 20)
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

    private func commit() {
        guard committable else { return }
        Haptics.tap()
        guard let title = RenameTitle.toCommit(draft: draft, seed: seed) else {
            onCancel()
            return
        }
        onCommit(title)
    }
}
