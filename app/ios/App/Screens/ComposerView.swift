import PhotosUI
import SwiftUI

/// The native composer dock: autosizing field, image staging via PhotosPicker,
/// in-field send. Interaction contract preserved from the web composer:
/// * staged picks count as a draft, so an attachment-only send works with an
///   empty field (the field also expands over the mic slot whenever a draft
///   exists);
/// * send is gated while any staged item is uploading (`waitingUpload` notice);
/// * picks over 100 MiB are rejected up front (`tooLarge`), matching the
///   gateway's blob cap.
struct ComposerView: View {
    @ObservedObject var store: ChatStore
    @ObservedObject private var lang = Lang.shared
    @State private var text = ""
    @State private var staged: [StagedAttachment] = []
    @State private var pickerItem: PhotosPickerItem?
    @FocusState private var focused: Bool

    private var hasDraft: Bool {
        !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !staged.isEmpty
    }

    var body: some View {
        VStack(spacing: 8) {
            if let notice = store.notice {
                HStack {
                    Text(verbatim: notice)
                        .font(Theme.mono(12))
                        .foregroundStyle(Theme.err)
                    Spacer()
                    Button {
                        store.notice = nil
                    } label: {
                        Image(systemName: "xmark")
                            .font(.system(size: 11))
                            .foregroundStyle(Theme.inkSoft)
                    }
                }
                .padding(.horizontal, 6)
            }

            if !staged.isEmpty {
                stagedStrip
            }

            HStack(alignment: .bottom, spacing: 8) {
                PhotosPicker(selection: $pickerItem, matching: .images) {
                    Image(systemName: "paperclip")
                        .font(.system(size: 18))
                        .foregroundStyle(Theme.ink)
                        .frame(width: 40, height: 40)
                }
                .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.addImage")))

                HStack(alignment: .bottom, spacing: 4) {
                    TextField(lang.t("chat.placeholder"), text: $text, axis: .vertical)
                        .lineLimit(1...6)
                        .font(.system(size: 16))
                        .focused($focused)
                        .padding(.leading, 14)
                        .padding(.vertical, 10)

                    Button {
                        send()
                    } label: {
                        Image(systemName: "arrow.up.circle.fill")
                            .font(.system(size: 26))
                            .foregroundStyle(hasDraft ? Theme.ink : Theme.line)
                    }
                    .disabled(!hasDraft)
                    .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.send")))
                    .padding(.trailing, 6)
                    .padding(.bottom, 6)
                }
                .background(
                    RoundedRectangle(cornerRadius: 22)
                        .fill(Theme.paper)
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 22)
                        .strokeBorder(Theme.line, lineWidth: 1)
                )

                // The mic placeholder collapses whenever a draft exists (no
                // capture wired yet, mirroring the web composer).
                if !hasDraft {
                    Image(systemName: "mic")
                        .font(.system(size: 18))
                        .foregroundStyle(Theme.inkSoft)
                        .frame(width: 40, height: 40)
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.voice")))
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.top, 8)
        .padding(.bottom, 6)
        .background(Theme.paper)
        .onChange(of: pickerItem) { _, item in
            guard let item else { return }
            pickerItem = nil
            stage(item)
        }
    }

    private var stagedStrip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(staged) { item in
                    ZStack(alignment: .topTrailing) {
                        Image(uiImage: item.thumbnail)
                            .resizable()
                            .scaledToFill()
                            .frame(width: 64, height: 64)
                            .clipShape(RoundedRectangle(cornerRadius: 10))
                            .overlay(
                                RoundedRectangle(cornerRadius: 10)
                                    .strokeBorder(
                                        item.state.isError ? Theme.err : Theme.line, lineWidth: 1)
                            )
                            .opacity(item.state.isUploading ? 0.5 : 1)

                        if item.state.isUploading {
                            ProgressView()
                                .frame(width: 64, height: 64)
                        }

                        Button {
                            staged.removeAll { $0.id == item.id }
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 16))
                                .foregroundStyle(Theme.ink)
                                .background(Circle().fill(Theme.paper))
                        }
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("chat.remove")))
                        .offset(x: 6, y: -6)
                    }
                }
            }
            .padding(.horizontal, 6)
            .padding(.top, 6)
        }
    }

    private func stage(_ item: PhotosPickerItem) {
        Task {
            let data: Data
            do {
                guard let loaded = try await item.loadTransferable(type: Data.self) else {
                    store.notice = Lang.shared.t("chat.attachFailed")
                    return
                }
                data = loaded
            } catch {
                store.notice = String(
                    format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error))
                return
            }
            guard data.count <= ChatStore.maxAttachmentBytes else {
                store.notice = Lang.shared.t("chat.tooLarge")
                return
            }
            guard let image = UIImage(data: data) else {
                store.notice = Lang.shared.t("chat.attachFailed")
                return
            }
            let stagedItem = StagedAttachment(thumbnail: image, byteCount: data.count)
            staged.append(stagedItem)
            do {
                let mime = Self.sniffMime(data)
                let blobId = try await Baybo.client.blobUploadBytes(bytes: data, mimeType: mime)
                update(stagedItem.id) { $0.state = .ready(blobId: blobId, mime: mime) }
            } catch {
                update(stagedItem.id) { $0.state = .error }
                store.notice = String(
                    format: Lang.shared.t("chat.sendFailed"), bayboErrorText(error))
            }
        }
    }

    private func update(_ id: UUID, _ mutate: (inout StagedAttachment) -> Void) {
        guard let idx = staged.firstIndex(where: { $0.id == id }) else { return }
        mutate(&staged[idx])
    }

    private func send() {
        if staged.contains(where: { $0.state.isUploading }) {
            store.notice = Lang.shared.t("chat.waitingUpload")
            return
        }
        let body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        let refs: [AttachmentRef] = staged.compactMap { item in
            guard case .ready(let blobId, let mime) = item.state else { return nil }
            return AttachmentRef(
                kind: .image, blobId: blobId, mimeType: mime,
                size: UInt32(clamping: item.byteCount), filename: nil)
        }
        guard !body.isEmpty || !refs.isEmpty else { return }
        store.notice = nil
        store.send(text: body, attachments: refs)
        text = ""
        staged.removeAll()
    }

    private static func sniffMime(_ data: Data) -> String {
        if data.starts(with: [0xFF, 0xD8, 0xFF]) { return "image/jpeg" }
        if data.starts(with: [0x89, 0x50, 0x4E, 0x47]) { return "image/png" }
        if data.starts(with: [0x47, 0x49, 0x46]) { return "image/gif" }
        if data.count > 11, data[8...11] == Data([0x57, 0x45, 0x42, 0x50]) {
            return "image/webp"
        }
        // HEIC picks land here; the gateway stores whatever mime is declared.
        return "application/octet-stream"
    }
}

struct StagedAttachment: Identifiable {
    enum State {
        case uploading
        case ready(blobId: String, mime: String)
        case error

        var isUploading: Bool {
            if case .uploading = self { return true }
            return false
        }

        var isError: Bool {
            if case .error = self { return true }
            return false
        }
    }

    let id = UUID()
    let thumbnail: UIImage
    let byteCount: Int
    var state: State = .uploading
}
