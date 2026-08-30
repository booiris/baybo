import Foundation

/// TRIAGE INSTRUMENT (delete with the white-flash investigation): a
/// crash-proof trail of the transcript webview's lifecycle, appended to a
/// file in the app container. A WebContent process exploding at GB/s loses
/// its own beacons (postMessage never ships) and the OS redacts NSLog off a
/// user-launched app — a native-side file survives both, and comes off the
/// device with `devicectl device copy from`.
@MainActor
enum WebTrail {
    private static let url: URL = {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("baybo", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("webtrail.log")
    }()

    private static let clock: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm:ss.SSS"
        return f
    }()

    static func note(_ line: String) {
        let stamped = "\(clock.string(from: Date())) \(line)\n"
        guard let data = stamped.data(using: .utf8) else { return }
        if let handle = try? FileHandle(forWritingTo: url) {
            defer { try? handle.close() }
            _ = try? handle.seekToEnd()
            try? handle.write(contentsOf: data)
        } else {
            try? data.write(to: url)
        }
    }
}
