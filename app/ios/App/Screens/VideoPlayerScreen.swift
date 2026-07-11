import AVKit
import SwiftUI

/// Full-screen player for a downloaded chat video: AVKit's transport controls
/// on a black field, with the viewer chrome's close disc top-left — an
/// embedded `AVPlayerViewController` has no Done button (AVKit only adds one
/// to presentations it owns). Playback starts on appear; the audio session is
/// `.playback`, so the ringer switch can't mute it (chat audio was stopped
/// before this presented — see `ChatStore.playVideo`).
struct VideoPlayerScreen: View {
    let onClose: () -> Void
    /// The materialised file under its real name — the share sheet's item
    /// (mirrors the image viewer's top-right share).
    private let url: URL
    @State private var player: AVPlayer
    @State private var sharing = false

    init(url: URL, onClose: @escaping () -> Void) {
        self.onClose = onClose
        self.url = url
        _player = State(initialValue: AVPlayer(url: url))
    }

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            VideoPlayerController(player: player)
                .ignoresSafeArea()
            VStack {
                HStack {
                    ViewerChromeButton(symbol: "xmark", action: onClose)
                    Spacer()
                    ViewerChromeButton(symbol: "square.and.arrow.up") { sharing = true }
                }
                .padding(.horizontal, 16)
                .padding(.top, 8)
                Spacer()
            }
        }
        .statusBarHidden(true)
        .sheet(isPresented: $sharing) {
            ShareSheet(url: url)
        }
        .onAppear {
            let session = AVAudioSession.sharedInstance()
            try? session.setCategory(.playback, mode: .moviePlayback)
            try? session.setActive(true)
            player.play()
        }
        .onDisappear {
            player.pause()
            try? AVAudioSession.sharedInstance().setActive(
                false, options: .notifyOthersOnDeactivation)
        }
    }
}

private struct VideoPlayerController: UIViewControllerRepresentable {
    let player: AVPlayer

    func makeUIViewController(context _: Context) -> AVPlayerViewController {
        let controller = AVPlayerViewController()
        controller.player = player
        // Chat audio's Now Playing entry is managed by AudioPlayerCenter; a
        // transient video must not leave a stale one behind.
        controller.updatesNowPlayingInfoCenter = false
        return controller
    }

    func updateUIViewController(_: AVPlayerViewController, context _: Context) {}
}
