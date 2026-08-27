import Foundation

@MainActor
/// Per-device presentation state. Never-opened projects retain server order.
final class ProjectRecency {
    private var opened: [String: Int64] = [:]
    private let url: URL

    static let filename = "project-recency.json"

    init(directory: URL = SessionIndex.supportDirectory()) {
        url = directory.appendingPathComponent(Self.filename)
        load()
    }

    /// Stamp a board as just opened.
    func record(_ projectId: String, at now: Date = Date()) {
        guard !projectId.isEmpty else { return }
        opened[projectId] = Int64((now.timeIntervalSince1970 * 1000).rounded())
        save()
    }

    func lastOpened(_ projectId: String) -> Int64? {
        opened[projectId]
    }

    func ordered(_ projects: [ProjectInfo]) -> [ProjectInfo] {
        var seen: [ProjectInfo] = []
        var unseen: [ProjectInfo] = []
        for project in projects {
            if opened[project.id] != nil {
                seen.append(project)
            } else {
                unseen.append(project)
            }
        }
        seen.sort { (opened[$0.id] ?? 0) > (opened[$1.id] ?? 0) }
        return seen + unseen
    }

    /// Drop a board's stamp — used when a board leaves the list entirely, so a
    /// deleted-and-recreated id cannot inherit an old position.
    func forget(_ projectId: String) {
        guard opened.removeValue(forKey: projectId) != nil else { return }
        save()
    }

    static func remove(in directory: URL = SessionIndex.supportDirectory()) {
        try? FileManager.default.removeItem(at: directory.appendingPathComponent(filename))
    }

    // MARK: - Disk

    private func load() {
        guard let data = try? Data(contentsOf: url),
            let decoded = try? JSONDecoder().decode([String: Int64].self, from: data)
        else { return }
        opened = decoded
    }

    private func save() {
        guard let data = try? JSONEncoder().encode(opened) else { return }
        try? data.write(to: url, options: .atomic)
    }
}
