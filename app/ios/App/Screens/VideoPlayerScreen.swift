import AVKit
import SwiftUI

/// Full-screen player for a downloaded chat video: AVKit's transport controls
/// on a black field. At full-screen size the embedded `AVPlayerViewController`
/// renders its OWN close button (top-left) whose tap dismisses the hosting
/// cover — so the viewer chrome contributes only the share disc top-right; a
/// second ✕ would duplicate AVKit's. Playback starts on appear; the audio
/// session is `.playback`, so the ringer switch can't mute it (chat audio was
/// stopped before this presented — see `ChatStore.playVideo`).
struct VideoPlayerScreen: View {
    /// The materialised file under its real name — the share sheet's item
    /// (mirrors the image viewer's top-right share).
    private let url: URL
    @State private var player: AVPlayer
    @State private var sharing = false

    init(url: URL) {
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
