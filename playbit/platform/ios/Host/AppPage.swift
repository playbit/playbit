import Foundation

/// One bounded publication page. Runtime admission verifies the app and build
/// before selecting its deterministic World volume or accessing app data.
struct AppPage {
    struct Row {
        let publisher: String
        let appID: String
        let build: String
        let unavailable: String?
        var id: String { publisher + ":" + appID }
        var label: String { appID }
    }
    let rows: [Row]
    let next: String?
    init() { rows = []; next = nil }

    enum Failure: LocalizedError {
        case invalid
        var errorDescription: String? { "The app list is invalid or too large. The previous page is retained." }
    }
    init(_ value: [String: Any], after: String?) throws {
        guard let entries = value["apps"] as? [[String: Any]], entries.count <= 32,
              after.map(Self.validAppID) ?? true else { throw Failure.invalid }
        var rows: [Row] = []
        var previous = after
        for entry in entries {
            guard let identity = entry["identity"] as? [String: Any],
                  let publisher = identity["publisher"] as? String, Identifier.valid(publisher),
                  let appID = identity["app_id"] as? String, Self.validAppID(appID),
                  previous.map({ $0 < appID }) ?? true,
                  let build = entry["build_id"] as? String, Identifier.valid(build, bytes: 32),
                  let state = entry["state"] as? String, ["active", "disabled", "deleted"].contains(state)
            else { throw Failure.invalid }
            let unavailable = state == "disabled" ? "Disabled" : (state == "deleted" ? "Deleted" : nil)
            rows.append(Row(publisher: publisher, appID: appID, build: build, unavailable: unavailable))
            previous = appID
        }
        let next = value["next"] as? String
        if let next {
            guard Self.validAppID(next), after.map({ $0 < next }) ?? true,
                  previous.map({ $0 <= next }) ?? true else { throw Failure.invalid }
        } else if !(value["next"] is NSNull) { throw Failure.invalid }
        self.rows = rows; self.next = next
    }
    static func validAppID(_ value: String) -> Bool {
        guard !value.isEmpty, value.utf8.count <= 64 else { return false }
        var separator = true
        for byte in value.utf8 {
            if (97...122).contains(byte) || (48...57).contains(byte) { separator = false }
            else if (byte == 45 || byte == 46) && !separator { separator = true }
            else { return false }
        }
        return !separator
    }
}
