/// Keys the NSE writes into the delivered notification's `userInfo` after
/// decrypting the preview, read back by the main app's notification-tap
/// handler. One file compiled into BOTH targets (see `project.yml`) so the two
/// sides can't drift.
enum PushPayloadKeys {
    /// The decrypted preview's `session_id` — the tap-routing target. Absent
    /// when the sender predates session routing or decryption failed (the app
    /// falls back to landing on the chat list).
    static let sessionId = "baybo.sessionId"
}
