import Foundation

/// When each board was last opened **on this device**.
///
/// Purely local, and deliberately so: which board you reached for last is a
/// fact about this phone, not about the account. The desk has its own idea of
/// order (`position`, and the rail's own list), the gateway stores nothing
/// about a client's attention, and a board opened on a laptop should not
/// reorder the phone's list.
///
/// Lives beside the board mirror rather than in `UserDefaults` for one reason:
/// **logout must take it**. These stamps are about a specific gateway's boards
/// — a project id that meant `rglide` under one account means nothing under
/// the next — so `ProjectsStore.removeMirror` deletes this file with the rest.
@MainActor
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
        // `.rounded()`, not a truncating cast: seconds-as-Double times 1000
        // lands just under the integer often enough to matter, and a stamp
        // one millisecond behind another written in the same instant is a
        // coin toss for which board leads the list.
        opened[projectId] = Int64((now.timeIntervalSince1970 * 1000).rounded())
        save()
    }

    func lastOpened(_ projectId: String) -> Int64? {
        opened[projectId]
    }

    /// Boards in the order this phone should show them: most recently opened
    /// first, then everything never opened here.
    ///
    /// **Never-opened boards keep the server's order rather than sinking to a
    /// timestamp of zero**, and they go after the opened ones. Two things this
    /// gets right that a plain `?? 0` sort would not: a board created on the
    /// desk and never touched here stays where the server put it relative to
    /// its peers, and a board created ON this phone is stamped the moment it
    /// opens — which the create flow does — so it never appears at the bottom
    /// of the list a second after being made.
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
    //
    // A plain `[id: ms]` map. Decoded leniently — this is on-disk JSON, not a
    // trusted type, and a corrupt file costs the ORDER, never the list.

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
