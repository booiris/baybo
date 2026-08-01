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
        return BayboClient(
            config: ClientConfig(
                apnsEnv: apnsEnvironment,
                logDir: logDir,
                blobCacheDir: blobCacheDir()))
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
