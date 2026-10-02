import Foundation

/// A small on-disk trail of the kept-warm webviews' lifecycle — loads, `ready`,
/// first paint, WebContent deaths, parks/revives and resume recycles — read off
/// a device with `xcrun devicectl device copy from --domain-type
/// appDataContainer` (`Library/Application Support/baybo/diagnostics/`).
///
/// It exists because NSLog is no witness on a device: an app launched from the
/// home screen has every dynamic argument redacted to `<private>`, so a blank
/// transcript in the field left nothing to read but WebKit's own lines. Only
/// lifecycle facts go here — never message content, session titles or URLs —
/// so the file is safe to pull off a user's phone.
enum WebLifecycleLog {
    enum Host: String {
        /// Scene transitions — the clock the resume recycle runs on.
        case app
        case transcript
        case deck
        case issue
    }

    private static let directoryName = "diagnostics"
    private static let fileName = "web-lifecycle.log"
    /// Past this the oldest half is dropped, so the trail never grows without
    /// bound on a device that is never inspected.
    private static let maxBytes = 256 * 1024

    private static let queue = DispatchQueue(label: "baybo.web-lifecycle-log", qos: .utility)
    private static let formatter = ISO8601DateFormatter()

    static func note(_ host: Host, _ event: String) {
        let line = "\(formatter.string(from: Date())) \(host.rawValue) \(event)\n"
        queue.async { append(line) }
    }

    static var fileURL: URL {
        ServerCache.rootDirectory()
            .appendingPathComponent(directoryName, isDirectory: true)
            .appendingPathComponent(fileName)
    }

    private static func append(_ line: String) {
        let url = fileURL
        let manager = FileManager.default
        try? manager.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        guard let data = line.data(using: .utf8) else { return }
        guard let handle = try? FileHandle(forWritingTo: url) else {
            try? data.write(to: url, options: .atomic)
            return
        }
        let size = (try? handle.seekToEnd()) ?? 0
        guard size + UInt64(data.count) <= UInt64(maxBytes) else {
            try? handle.close()
            trimOldestHalf(url, appending: data)
            return
        }
        try? handle.write(contentsOf: data)
        try? handle.close()
    }

    private static func trimOldestHalf(_ url: URL, appending data: Data) {
        guard let existing = try? Data(contentsOf: url) else {
            try? data.write(to: url, options: .atomic)
            return
        }
        var kept = existing.suffix(maxBytes / 2)
        if let newline = kept.firstIndex(of: UInt8(ascii: "\n")) {
            kept = kept[kept.index(after: newline)...]
        }
        try? (Data(kept) + data).write(to: url, options: .atomic)
    }
}
