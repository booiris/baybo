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

/// UserDefaults keys for the durable chat pointers the webview's localStorage
/// used to hold (native owns navigation + persistence now).
enum ChatDefaults {
    /// The active chat session id (`CHAT_SESSION_KEY` successor).
    static let sessionId = "baybo.chat.session"
    /// The persisted transcript state JSON the web bundle sends over the bridge
    /// (`CHAT_STATE_KEY` successor). Also the single durable source of the
    /// reconnect cursor — its embedded `lastOrdinal` is written atomically with
    /// the messages it matches.
    static let transcriptState = "baybo.chat.state"
    /// Legacy separate-cursor key: never written anymore (the cursor could
    /// durably outrun the debounced transcript blob); still scrubbed on logout.
    static let lastOrdinal = "baybo.chat.lastOrdinal"
}
