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
        return BayboClient(config: ClientConfig(apnsEnv: env, logDir: logDir))
    }()
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
