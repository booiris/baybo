import PhotosUI
import SwiftUI

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
            get: {
                MainActor.assumeIsolated { attach.pick == source }
            },
            set: { presented in
                MainActor.assumeIsolated {
                    guard !presented, attach.pick == source else { return }
                    attach.pick = nil
                }
            })
    }
}
