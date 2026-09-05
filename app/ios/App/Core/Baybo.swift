import Foundation

/// The shared Rust core client. Constructed once; the transport pumps it spawns
/// keep running between calls, and the in-flight pairing sessions it parks
/// between `pairBegin`/`pairConfirm` live inside it.
enum Baybo {
    static let apnsEnvironmentInfoKey = "BayboApnsEnvironment"

    static func apnsEnvironment(for value: String?) -> ApnsEnvironment {
        switch value?.lowercased() {
        case "production":
            return .production
        case "development", "sandbox":
            return .sandbox
        default:
            return .sandbox
        }
    }

    static let apnsEnvironment: ApnsEnvironment = {
        let value = Bundle.main.object(
            forInfoDictionaryKey: apnsEnvironmentInfoKey
        ) as? String
        let environment = apnsEnvironment(for: value)
        switch value?.lowercased() {
        case "development", "sandbox", "production":
            break
        default:
            NSLog(
                "baybo: %@ missing or invalid; defaulting APNs environment to sandbox",
                apnsEnvironmentInfoKey)
        }
        return environment
    }()

    static var apnsEnvironmentName: String {
        switch apnsEnvironment {
        case .sandbox:
            return "sandbox"
        case .production:
            return "production"
        }
    }

    static let client: BayboClient = {
        let logDir = FileManager.default
            .urls(for: .libraryDirectory, in: .userDomainMask)
            .first?
            .appendingPathComponent("Logs", isDirectory: true)
            .path
        // `secureStore: nil` and `try!` are both load-bearing, and neither is a
        // shortcut. The core keeps ONE exported signature for both shells
        // because uniffi-bindgen generates them from a single host cdylib, so
        // the store is an argument even on the platform that does not use one:
        // iOS credentials go through the Security framework inside the core,
        // whose keychain item identity the continuity contract freezes. The
        // constructor's only failure is refusing a missing store, and that arm
        // is compiled out under `target_os = "ios"` — there is no error this
        // call can return, and turning a real one into an optional would only
        // hide it. Android passes a KeystoreSecureStore here and gets the
        // refusal if it ever forgets.
        return try! BayboClient(
            config: ClientConfig(
                logDir: logDir,
                blobCacheDir: ServerCache.blobDirectory(in: SessionIndex.supportDirectory())),
            secureStore: nil)
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
