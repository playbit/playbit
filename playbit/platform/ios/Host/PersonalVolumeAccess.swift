import Foundation
import CoreFoundation

/// A complete, bounded mount draft for one session. Never save a partial snapshot.
struct PersonalVolumeAccess {
    enum Phase { case ready, saving, saved }
    struct Row: Equatable {
        let id: String
        let name: String
        var choice: Int // Native segments: Off, Read, Read/write.
    }
    enum Failure: LocalizedError {
        case invalid, tooLarge
        var errorDescription: String? {
            switch self {
            case .invalid: return "The volume access list is invalid. Nothing was changed."
            case .tooLarge: return "This device can show up to 128 personal volumes. Nothing was changed."
            }
        }
    }
    private(set) var rows: [Row]
    private(set) var phase = Phase.ready

    init(_ value: [String: Any]) throws {
        guard let volumes = value["volumes"] as? [[String: Any]] else { throw Failure.invalid }
        let grants: [[String: Any]]
        // Null is the current protocol's never-consented state, distinct from
        // an explicit saved empty grant list. Both start with every segment Off.
        if value["grants"] is NSNull { grants = [] }
        else if let saved = value["grants"] as? [[String: Any]] { grants = saved }
        else { throw Failure.invalid }
        guard volumes.count <= 128, grants.count <= 128 else { throw Failure.tooLarge }
        var rows: [Row] = []
        rows.reserveCapacity(volumes.count)
        var ids = Set<String>()
        var nameBytes = 0
        for volume in volumes {
            guard let id = volume["id"] as? String, Self.validID(id), ids.insert(id).inserted,
                  let name = volume["name"] as? String, !name.isEmpty,
                  name.utf8.count <= 256 else { throw Failure.invalid }
            nameBytes += name.utf8.count
            guard nameBytes <= 64 * 1024 else { throw Failure.tooLarge }
            rows.append(Row(id: id, name: name, choice: 0))
        }
        var selected = Set<String>()
        for grant in grants {
            guard let id = grant["volume"] as? String, selected.insert(id).inserted,
                  let index = rows.firstIndex(where: { $0.id == id }),
                  let write = grant["write"] as? NSNumber,
                  CFGetTypeID(write) == CFBooleanGetTypeID() else { throw Failure.invalid }
            rows[index].choice = write.boolValue ? 2 : 1
        }
        self.rows = rows
    }

    private static func validID(_ id: String) -> Bool {
        Identifier.valid(id)
    }
    @discardableResult mutating func change(_ id: String, to choice: Int) -> Bool {
        guard phase == .ready, (0...2).contains(choice),
              let index = rows.firstIndex(where: { $0.id == id }) else { return false }
        rows[index].choice = choice
        return true
    }
    mutating func beginSave() -> [(volume: String, write: Bool)]? {
        guard phase == .ready else { return nil }
        phase = .saving
        return rows.compactMap { row in
            row.choice == 0 ? nil : (volume: row.id, write: row.choice == 2)
        }
    }
    @discardableResult mutating func finishSave(success: Bool) -> Bool {
        guard phase == .saving else { return false }
        phase = success ? .saved : .ready
        return true
    }
}
