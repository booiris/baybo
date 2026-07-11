import AVFoundation
import Foundation
import MediaPlayer

/// The app's ONE audio engine. Chat audio cards drive it over the bridge
/// (`audioToggle` / `audioSeek` / `queryAudioState`) and it mirrors playback
/// back as `audioState` pushes (2 Hz position ticks while playing).
///
/// Native rather than an in-webview `<audio>` on purpose: the bytes never
/// cross the bridge as base64, `.playback` means the ringer switch can't
/// silence it, and the track keeps playing when the user backs out of the
/// chat — with the `audio` background mode, through lock too, controllable
/// from Control Center (Now Playing + remote commands below).
///
/// One player app-wide: starting a track stops whatever else was playing and
/// tells the usurped card it `stopped`.
@MainActor
final class AudioPlayerCenter {
    static let shared = AudioPlayerCenter()

    /// Position-tick cadence. The card only renders m:ss, so anything faster
    /// than 2 Hz is wasted bridge evals.
    private static let tickInterval = CMTime(seconds: 0.5, preferredTimescale: 600)

    /// The single webview's bridge — refreshed on every card message, so pushes
    /// land in the live transcript. A tick for a track that keeps playing while
    /// another session is on screen simply finds no listener web-side.
    private weak var bridge: TranscriptBridge?
    private var player: AVPlayer?
    private var blobId: String?
    private var title = ""
    private var timeObserver: Any?
    private var endObserver: NSObjectProtocol?
    private var interruptionObserver: NSObjectProtocol?
    private var failedToPlayObserver: NSObjectProtocol?
    private var itemStatusObserver: NSKeyValueObservation?
    private var timeControlObserver: NSKeyValueObservation?
    /// The current track ran to its end (rewound, paused, card told `stopped`).
    /// The player stays loaded for an instant replay, but every state answer
    /// must keep saying `stopped` — a remounting card would otherwise resync to
    /// an engaged "paused @ 0:00" the live card never showed.
    private var ended = false
    private var remoteCommandsInstalled = false

    private init() {}

    /// Play/pause `blobId`; a different track usurps the current one.
    func toggle(blobId: String, url: URL, title: String, bridge: TranscriptBridge?) {
        self.bridge = bridge
        if blobId == self.blobId, let player {
            // `!= .paused` on purpose: right after play() the engine sits in
            // .waitingToPlayAtSpecifiedRate — the user's intent is "playing",
            // and a tap there must pause, not double-play.
            if player.timeControlStatus != .paused {
                player.pause()
            } else {
                ended = false
                activateSession()
                player.play()
            }
            pushState()
            updateNowPlaying()
            return
        }

        stopCurrent(notifyStopped: true)
        self.blobId = blobId
        self.title = title
        let item = AVPlayerItem(url: url)
        let player = AVPlayer(playerItem: item)
        self.player = player
        installObservers(item: item, player: player)
        installRemoteCommands()
        activateSession()
        player.play()
        pushState()
        updateNowPlaying()
    }

    func seek(blobId: String, position: Double, bridge: TranscriptBridge?) {
        self.bridge = bridge
        guard blobId == self.blobId, let player else { return }
        ended = false
        let seconds = max(0, position)
        // Optimistic mirror: the card commits its scrub on lift, and the fill
        // must not snap back to the pre-seek playhead for the round trip the
        // engine takes to actually land there.
        bridge?.audioState(
            blobId: blobId,
            state: isEngineActive ? "playing" : "paused",
            position: seconds,
            duration: currentDuration)
        let target = CMTime(seconds: seconds, preferredTimescale: 600)
        player.seek(to: target, toleranceBefore: .zero, toleranceAfter: .zero) { [weak self] _ in
            Task { @MainActor in
                self?.pushState()
                self?.updateNowPlaying()
            }
        }
    }

    /// A card's mount-time probe: answer with this track's live state, or a
    /// bare `stopped` when the player holds some other track (or none).
    func queryState(blobId: String, bridge: TranscriptBridge?) {
        self.bridge = bridge
        if blobId == self.blobId {
            pushState()
        } else {
            bridge?.audioState(blobId: blobId, state: "stopped", position: 0, duration: 0)
        }
    }

    /// Kill playback outright — logout teardown, or a video about to take the
    /// audio session (two engines over one session just fight).
    func stop() {
        stopCurrent(notifyStopped: true)
    }

    // MARK: - Internals

    private func installObservers(item: AVPlayerItem, player: AVPlayer) {
        timeObserver = player.addPeriodicTimeObserver(
            forInterval: Self.tickInterval, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.player?.timeControlStatus == .playing else { return }
                self.pushState()
            }
        }
        endObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.didPlayToEndTimeNotification, object: item, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.handlePlaybackEnded() }
        }
        // A call / another app's audio pauses the player under us — mirror it,
        // or the card keeps drawing a moving bar for silence.
        interruptionObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification, object: nil, queue: .main
        ) { [weak self] note in
            MainActor.assumeIsolated {
                guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                    AVAudioSession.InterruptionType(rawValue: raw) == .began
                else { return }
                self?.pushState()
                self?.updateNowPlaying()
            }
        }
        // An unplayable blob (truncated download, mislabelled mime) never
        // starts and never ends — without these two the card plays dead air
        // forever. Reset to rest; a tap retries from scratch.
        itemStatusObserver = item.observe(\.status) { [weak self] observed, _ in
            guard observed.status == .failed else { return }
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self?.stopCurrent(notifyStopped: true) }
            }
        }
        failedToPlayObserver = NotificationCenter.default.addObserver(
            forName: AVPlayerItem.failedToPlayToEndTimeNotification, object: item, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.stopCurrent(notifyStopped: true) }
        }
        // The system also pauses without any interruption notice — headphones
        // unplugged (route change), a stall — and the card would wedge on
        // "playing" with an inverted toggle. Mirror EVERY engine flip. KVO
        // delivers on the mutating thread; hop before touching actor state.
        timeControlObserver = player.observe(\.timeControlStatus) { [weak self] _, _ in
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.pushState()
                    self?.updateNowPlaying()
                }
            }
        }
    }

    /// Rewind and report `stopped` so the card returns to rest; the player
    /// stays loaded, so a replay starts instantly from the top. `ended` is set
    /// FIRST so the pause's own timeControl KVO push can't resurrect the card
    /// as an engaged "paused".
    private func handlePlaybackEnded() {
        ended = true
        player?.seek(to: .zero)
        player?.pause()
        pushState()
        MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
        deactivateSession()
    }

    private func stopCurrent(notifyStopped: Bool) {
        // Observers first: tearing the player down flips timeControlStatus,
        // and that KVO push must not land after the `stopped` below.
        itemStatusObserver?.invalidate()
        itemStatusObserver = nil
        timeControlObserver?.invalidate()
        timeControlObserver = nil
        if let timeObserver, let player {
            player.removeTimeObserver(timeObserver)
        }
        timeObserver = nil
        for observer in [endObserver, interruptionObserver, failedToPlayObserver] {
            if let observer {
                NotificationCenter.default.removeObserver(observer)
            }
        }
        endObserver = nil
        interruptionObserver = nil
        failedToPlayObserver = nil
        player?.pause()
        let usurped = blobId
        player = nil
        blobId = nil
        title = ""
        ended = false
        if notifyStopped, let usurped {
            bridge?.audioState(blobId: usurped, state: "stopped", position: 0, duration: 0)
        }
        MPNowPlayingInfoCenter.default().nowPlayingInfo = nil
        deactivateSession()
    }

    /// Whether the engine is driving toward audible playback. `.waiting…`
    /// counts: right after play() the user's intent — and the correct card
    /// glyph — is "playing", even though the rate is still 0.
    private var isEngineActive: Bool {
        guard let player else { return false }
        return player.timeControlStatus != .paused
    }

    private func pushState() {
        guard let blobId else { return }
        if ended {
            bridge?.audioState(blobId: blobId, state: "stopped", position: 0, duration: 0)
            return
        }
        bridge?.audioState(
            blobId: blobId,
            state: isEngineActive ? "playing" : "paused",
            position: currentPosition,
            duration: currentDuration)
    }

    private var currentPosition: Double {
        guard let time = player?.currentTime(), time.isNumeric else { return 0 }
        return max(0, time.seconds)
    }

    private var currentDuration: Double {
        guard let duration = player?.currentItem?.duration, duration.isNumeric else { return 0 }
        return max(0, duration.seconds)
    }

    /// `.playback`: audible past the ringer switch, and — with the `audio`
    /// background mode — past lock/background.
    private func activateSession() {
        let session = AVAudioSession.sharedInstance()
        try? session.setCategory(.playback, mode: .default)
        try? session.setActive(true)
    }

    private func deactivateSession() {
        try? AVAudioSession.sharedInstance().setActive(
            false, options: .notifyOthersOnDeactivation)
    }

    /// Lock-screen / Control Center transport. Installed once; the targets read
    /// whatever track currently owns the player.
    private func installRemoteCommands() {
        guard !remoteCommandsInstalled else { return }
        remoteCommandsInstalled = true
        let center = MPRemoteCommandCenter.shared()
        center.playCommand.addTarget { [weak self] _ in
            MainActor.assumeIsolated { self?.remoteSetPlaying(true) ?? .noSuchContent }
        }
        center.pauseCommand.addTarget { [weak self] _ in
            MainActor.assumeIsolated { self?.remoteSetPlaying(false) ?? .noSuchContent }
        }
        center.togglePlayPauseCommand.addTarget { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return .noSuchContent }
                return self.remoteSetPlaying(!self.isEngineActive)
            }
        }
    }

    private func remoteSetPlaying(_ play: Bool) -> MPRemoteCommandHandlerStatus {
        guard let player else { return .noSuchContent }
        if play {
            ended = false
            activateSession()
            player.play()
        } else {
            player.pause()
        }
        pushState()
        updateNowPlaying()
        return .success
    }

    private func updateNowPlaying() {
        guard blobId != nil else { return }
        var info: [String: Any] = [
            MPNowPlayingInfoPropertyMediaType: MPNowPlayingInfoMediaType.audio.rawValue,
            MPMediaItemPropertyTitle: title,
            MPNowPlayingInfoPropertyElapsedPlaybackTime: currentPosition,
            MPNowPlayingInfoPropertyPlaybackRate: isEngineActive ? 1.0 : 0.0,
        ]
        if currentDuration > 0 {
            info[MPMediaItemPropertyPlaybackDuration] = currentDuration
        }
        MPNowPlayingInfoCenter.default().nowPlayingInfo = info
    }
}
