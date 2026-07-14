import Foundation

/// The shared Rust core client. Constructed once; the transport pumps it spawns
/// keep running between calls, and the in-flight pairing sessions it parks
/// between `pairBegin`/`pairConfirm` live inside it.
enum Baybo {
    static let client: BayboClient = {
        // Sandbox tokens come from debug builds, production from release —
        // decided HERE (the Xcode build config) because the Rust core is often
        // compiled in release even for a debug app.
        #if DEBUG
        let env = ApnsEnvironment.sandbox
        #else
        let env = ApnsEnvironment.production
        #endif
        let logDir = FileManager.default
            .urls(for: .libraryDirectory, in: .userDomainMask)
            .first?
            .appendingPathComponent("Logs", isDirectory: true)
            .path
        return BayboClient(
            config: ClientConfig(apnsEnv: env, logDir: logDir, blobCacheDir: blobCacheDir()))
    }()

    /// `Application Support/baybo/blobs` — durable, alongside the session
    /// registry and transcript mirrors. Deliberately NOT the OS temp dir: iOS
    /// reclaims that under storage pressure, and a file the user downloaded
    /// should stay downloaded.
    ///
    /// Excluded from backup: blobs can run to 100 MiB each and are always
    /// re-fetchable from the gateway, so they have no business in iCloud.
    private static func blobCacheDir() -> String? {
        let dir = SessionIndex.supportDirectory()
            .appendingPathComponent("blobs", isDirectory: true)
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            var url = dir
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            try url.setResourceValues(values)
        } catch {
            NSLog("baybo: blob cache dir unavailable (%@); falling back to tmp", "\(error)")
            return nil
        }
        return dir.path
    }
}

/// LEGACY UserDefaults keys from the single-session era — never written
/// anymore. Durable chat state now lives in Application Support: the session
/// registry (`SessionIndex`) and per-session transcript mirrors
/// (`TranscriptStore`). These keys survive only so
/// `SessionIndex.migrateLegacySingleSession` can fold a pre-list install's one
/// conversation into the registry (and scrub the keys).
enum ChatDefaults {
    /// The active chat session id (`CHAT_SESSION_KEY` successor).
    static let sessionId = "baybo.chat.session"
    /// The persisted transcript state JSON the web bundle sent over the bridge
    /// (`CHAT_STATE_KEY` successor); its embedded `lastOrdinal` was the
    /// reconnect cursor.
    static let transcriptState = "baybo.chat.state"
    /// Even older separate-cursor key.
    static let lastOrdinal = "baybo.chat.lastOrdinal"
}
