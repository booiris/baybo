import Foundation

enum ServerCache {
    private static let serversDirectoryName = "servers"
    private static let unboundDirectoryName = "unbound"

    static func rootDirectory() -> URL {
        let base = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)
            .first ?? FileManager.default.temporaryDirectory
        let root = base.appendingPathComponent("baybo", isDirectory: true)
        try? FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root
    }

    static func activeSupportDirectory() -> URL {
        let key: String?
        do {
            key = try activeServerCacheKey()
        } catch {
            NSLog("baybo: read server cache key: %@", String(describing: error))
            key = nil
        }
        return supportDirectory(for: key, in: rootDirectory())
    }

    static func supportDirectory(for serverKey: String?, in root: URL) -> URL {
        let component = serverKey.flatMap(validComponent) ?? unboundDirectoryName
        let directory = root
            .appendingPathComponent(serversDirectoryName, isDirectory: true)
            .appendingPathComponent(component, isDirectory: true)
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        return directory
    }

    static func blobDirectory(in supportDirectory: URL) -> String? {
        let directory = supportDirectory.appendingPathComponent("blobs", isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: directory, withIntermediateDirectories: true)
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            var mutable = directory
            try mutable.setResourceValues(values)
            return directory.path
        } catch {
            NSLog("baybo: blob cache dir unavailable (%@); falling back to tmp", "\(error)")
            return nil
        }
    }

    private static func validComponent(_ value: String) -> String? {
        guard !value.isEmpty, value.count <= 128 else { return nil }
        let allowed = CharacterSet(charactersIn: "abcdefghijklmnopqrstuvwxyz0123456789-")
        guard value.unicodeScalars.allSatisfy(allowed.contains) else { return nil }
        return value
    }

}
