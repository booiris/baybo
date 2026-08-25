import PhotosUI
import SwiftUI

/// The composer's inline `+`, and the anchor its panel blooms from.
///
/// The anchor report is the whole reason this is a type rather than four lines
/// in each dock: the button reports its frame in the DOCK's coordinate space —
/// not `.global` — because the panel is laid out in that same space, and two
/// frames measured in different containers disagreed by ~14pt, enough that the
/// top of a row was scrim and the gap under the panel fired a picker.
struct AttachButton: View {
    @ObservedObject var attach: AttachMenu
    /// Whether the clipboard is worth offering. Read ONCE per opening (the
    /// probe is not free), which is why it is passed rather than sampled here.
    let pasteReady: Bool

    /// 46×48: the glyph's own square plus the row's full height, so the tap
    /// target spans the pill's left end rather than a 22pt circle inside it.
    static let size = CGSize(width: 46, height: 48)

    var body: some View {
        Button {
            Haptics.tap()
            withAnimation(AttachMenuPanel.fade) {
                attach.toggle(pasteReady: pasteReady)
            }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 22, weight: .light))
                .foregroundStyle(Theme.ink)
                .frame(width: Self.size.width, height: Self.size.height)
                .contentShape(Rectangle())
        }
        .onGeometryChange(for: CGRect.self) { proxy in
            proxy.frame(in: .named(AttachMenuPanel.dockSpace))
        } action: { frame in
            attach.report(anchor: frame)
        }
        .accessibilityIdentifier("composer-attach")
        .accessibilityLabel(Text(verbatim: Lang.shared.t("attach.addImage")))
    }
}

extension View {
    /// The three pickers a panel row can ask for, answering into `staging`.
    ///
    /// A modifier rather than a view because two of them are presentations
    /// (`.photosPicker`, `.fileImporter`) that must hang off the dock that
    /// owns the panel, and the third is not a picker at all.
    func attachmentPickers(
        attach: AttachMenu, staging: ComposerStaging, photoPicks: Binding<[PhotosPickerItem]>
    ) -> some View {
        self
            .photosPicker(
                isPresented: AttachPickers.binding(attach, .photos),
                selection: photoPicks,
                maxSelectionCount: max(
                    1, ComposerStaging.maxStagedAttachments - staging.staged.count),
                matching: .images
            )
            // No type restriction: whatever the model can be handed, the user
            // can attach — the mime the extension implies is what decides
            // server-side whether it is readable at all.
            .fileImporter(
                isPresented: AttachPickers.binding(attach, .files),
                allowedContentTypes: [.data],
                allowsMultipleSelection: true
            ) { result in
                switch result {
                case .success(let urls):
                    staging.stage(files: urls)
                case .failure:
                    staging.notePickerFailed()
                }
            }
            .onChange(of: photoPicks.wrappedValue) { _, picks in
                guard !picks.isEmpty else { return }
                photoPicks.wrappedValue = []
                staging.stage(photos: picks)
            }
            // Paste is the one row with no picker to answer it, so the binding
            // above cannot serve it: that binding retires `attach.pick` on a
            // sheet's DISMISSAL, and a second tap on a row whose `pick` is
            // already set publishes no change at all — the row would work
            // exactly once. Clearing the request here, in the same turn, is
            // what keeps it live.
            .onChange(of: attach.pick) { _, pick in
                guard pick == .paste else { return }
                attach.pick = nil
                staging.stagePasteboard()
            }
    }
}

enum AttachPickers {
    /// The panel's pick, as the picker that answers it sees it. Each picker
    /// clears the request as it dismisses.
    static func binding(_ attach: AttachMenu, _ source: AttachSource) -> Binding<Bool> {
        Binding(
            get: { attach.pick == source },
            set: { presented in
                guard !presented, attach.pick == source else { return }
                attach.pick = nil
            })
    }
}
