import Foundation

/// Display-only labels. Opening a row always uses its complete session identity.
struct SessionListSnapshot {
    struct Row: Equatable {
        let id: String
        let title: String
        let subtitle: String?
        let build: String?
        let reference: String?
        init(id: String, title: String, subtitle: String?, build: String?, reference: String? = nil) {
            self.id = id; self.title = title; self.subtitle = subtitle; self.build = build; self.reference = reference
        }
    }
    private(set) var rows: [Row]
    init() { rows = [] }
    mutating func move(from source: Int, to destination: Int) {
        guard rows.indices.contains(source), rows.indices.contains(destination) else { return }
        rows.insert(rows.remove(at: source), at: destination)
    }

    enum Failure: LocalizedError {
        case invalid
        var errorDescription: String? { "The saved session list is invalid or too large. Previous sessions are retained." }
    }

    init(_ value: [String: Any]) throws {
        guard let sessions = value["sessions"] as? [[String: Any]], sessions.count <= 256,
              let volumes = value["volumes"] as? [[String: Any]] else { throw Failure.invalid }
        var ids: [String] = [], names: [String] = [], roots: [String] = []
        var builds: [String?] = []
        var references: [String?] = []
        var buildMessages: [String?] = []
        var seen = Set<String>()
        var neededRoots = Set<String>()
        var checkedBuilds = Set<String>()
        var groups: [String: [Int]] = [:]
        for (index, session) in sessions.enumerated() {
            guard let id = Self.text(session["id"], limit: 44), Identifier.valid(id, bytes: 32),
                  seen.insert(id).inserted,
                  let name = Self.text(session["app_name"], limit: 256),
                  let root = Self.text(session["root_volume"], limit: 22),
                  let build = Self.text(session["resolved_build_id"], limit: 44),
                  let state = session["build_state"] as? [String: Any], let kind = state["kind"] as? String,
                  ["checking", "not_downloaded", "downloading", "ready", "unreadable", "invalid", "download_failed", "metadata_failed"].contains(kind)
            else { throw Failure.invalid }
            if neededRoots.insert(root).inserted && !Identifier.valid(root) { throw Failure.invalid }
            if checkedBuilds.insert(build).inserted && !Identifier.valid(build, bytes: 32) { throw Failure.invalid }
            let reference = session["reference"] as? String
            if let reference, !Identifier.valid(reference) { throw Failure.invalid }
            references.append(reference)
            ids.append(id); names.append(name); roots.append(root)
            builds.append(kind == "ready" ? build : nil)
            buildMessages.append(session["build_message"] as? String)
            groups[name, default: []].append(index)
        }
        var rootNames: [String: String] = [:]
        for volume in volumes {
            guard let id = volume["id"] as? String, neededRoots.contains(id) else { continue }
            guard rootNames[id] == nil, let name = Self.text(volume["name"], limit: 256)
            else { throw Failure.invalid }
            rootNames[id] = name
        }
        var subtitles = [String?](repeating: nil, count: ids.count)
        for indices in groups.values where indices.count > 1 {
            let ordered = indices.sorted { (references[$0] ?? "", ids[$0]) < (references[$1] ?? "", ids[$1]) }
            let showRoot = Set(indices.map { roots[$0] }).count > 1
            for (position, index) in ordered.enumerated() {
                // In sorted order, the longest common prefix is with a neighbor.
                // Avoid comparing every pair when all 256 names are identical.
                var length = 8
                if position > 0 { length = max(length, Self.commonPrefix(references[index] ?? "", references[ordered[position - 1]] ?? "") + 1) }
                if position + 1 < ordered.count { length = max(length, Self.commonPrefix(references[index] ?? "", references[ordered[position + 1]] ?? "") + 1) }
                // Validated IDs are ASCII; no grapheme traversal is needed.
                var subtitle = references[index].map { String(decoding: $0.utf8.prefix(length), as: UTF8.self) } ?? "Session"
                if showRoot { subtitle += " • " + (rootNames[roots[index]] ?? "Volume \(roots[index])") }
                subtitles[index] = subtitle
            }
        }
        rows = ids.indices.map {
            let subtitle = [subtitles[$0], buildMessages[$0]].compactMap { $0 }.joined(separator: " • ")
            return Row(id: ids[$0], title: names[$0], subtitle: subtitle.isEmpty ? nil : subtitle, build: builds[$0], reference: references[$0])
        }
    }

    private static func text(_ value: Any?, limit: Int) -> String? {
        guard var text = value as? String, !text.isEmpty, text.utf16.count <= limit else { return nil }
        // Foundation supplies NSString-backed values. Materialize UTF-8 once
        // before hashing, validating or traversing them repeatedly.
        text.makeContiguousUTF8()
        return text.utf8.count <= limit ? text : nil
    }

    private static func commonPrefix(_ left: String, _ right: String) -> Int {
        var count = 0
        for (a, b) in zip(left.utf8, right.utf8) {
            if a != b { break }
            count += 1
        }
        return count
    }
}
