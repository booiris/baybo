import SwiftUI

struct StagedStrip: View {
    let items: [StagedAttachment]
    let onRemove: (UUID) -> Void
    let onRetry: (UUID) -> Void

    private static let side: CGFloat = 64
    /// A file pill is the image thumbnail's height and wide enough for a
    /// middle-truncated name over its size line.
    private static let fileWidth: CGFloat = 176
    private static let corner: CGFloat = 10

    var body: some View { strip }

    private var strip: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(items) { item in
                    ZStack(alignment: .topTrailing) {
                        stagedTile(item)

                        Button {
                            onRemove(item.id)
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .font(.system(size: 16))
                                .foregroundStyle(Theme.ink)
                                .background(Circle().fill(Theme.paper))
                        }
                        .accessibilityLabel(Text(verbatim: Lang.shared.t("attach.remove")))
                        .offset(x: 6, y: -6)
                    }
                }
            }
            .padding(.horizontal, 18)
            .padding(.top, 6)
        }
    }

    @ViewBuilder
    private func stagedTile(_ item: StagedAttachment) -> some View {
        Group {
            switch item.preview {
            case .image(let image):
                Group {
                    if let image {
                        Image(uiImage: image)
                            .resizable()
                            .scaledToFill()
                    } else {
                        Theme.surface
                    }
                }
                .frame(width: Self.side, height: Self.side)
                .opacity(item.state.isPending ? 0.5 : 1)
                .overlay {
                    if item.state.isPending {
                        ProgressView()
                    } else if item.state.isError {
                        Image(systemName: "arrow.clockwise")
                            .font(.system(size: 18, weight: .light))
                            .foregroundStyle(Theme.err)
                    }
                }
            case .file(let name, let mime):
                fileTile(item, name: name, mime: mime)
            }
        }
        .clipShape(RoundedRectangle(cornerRadius: Self.corner))
        .overlay(
            RoundedRectangle(cornerRadius: Self.corner)
                .strokeBorder(item.state.isError ? Theme.err : Theme.line, lineWidth: 1)
        )
        .contentShape(RoundedRectangle(cornerRadius: Self.corner))
        .onTapGesture {
            guard item.state.isError else { return }
            onRetry(item.id)
        }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(Text(verbatim: tileLabel(item)))
        .accessibilityValue(Text(verbatim: metaText(item)))
    }

    private func fileTile(_ item: StagedAttachment, name: String, mime: String) -> some View {
        HStack(spacing: 8) {
            Group {
                if item.state.isPending {
                    ProgressView()
                } else {
                    Image(
                        systemName: item.state.isError
                            ? "arrow.clockwise" : StagedAttachment.glyph(forMime: mime)
                    )
                    .font(.system(size: 18, weight: .light))
                    .foregroundStyle(item.state.isError ? Theme.err : Theme.ink)
                }
            }
            .frame(width: 22)

            VStack(alignment: .leading, spacing: 3) {
                Text(verbatim: name)
                    .font(Theme.sys(13))
                    .foregroundStyle(Theme.ink)
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(verbatim: metaText(item))
                    .font(Theme.mono(11))
                    .foregroundStyle(item.state.isError ? Theme.err : Theme.inkSoft)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .frame(width: Self.fileWidth, height: Self.side, alignment: .leading)
        .background(Theme.surface)
    }

    private func tileLabel(_ item: StagedAttachment) -> String {
        if case .file(let name, _) = item.preview { return name }
        return Lang.shared.t("attach.stagedImage")
    }

    private func metaText(_ item: StagedAttachment) -> String {
        switch item.state {
        case .queued, .uploading:
            let sent = StagedAttachment.byteText(item.state.sentBytes)
            let total = StagedAttachment.byteText(UInt64(item.byteCount))
            return "\(sent) / \(total)"
        case .ready:
            return StagedAttachment.byteText(UInt64(item.byteCount))
        case .error:
            return Lang.shared.t("attach.retryUpload")
        }
    }
}
