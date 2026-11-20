import Foundation

@main enum AppPageTests {
    static let publisher = Identifier.encode([UInt8](repeating: 0x44, count: 16))
    static let build = Identifier.encode([UInt8](repeating: 0xaa, count: 32))
    static func app(_ index: Int) -> String { String(format: "app%04d", index) }
    static func entry(_ index: Int, state: String = "active") -> [String: Any] {
        ["identity": ["publisher": publisher, "app_id": app(index)], "state": state, "build_id": build]
    }
    static func page(_ entries: [[String: Any]], next: String? = nil) -> [String: Any] {
        ["apps": entries, "next": next.map { $0 as Any } ?? NSNull()]
    }
    static func rejects(_ value: [String: Any], after: String? = nil) {
        do { _ = try AppPage(value, after: after); preconditionFailure("Accepted malformed page") }
        catch { }
    }
    static func main() throws {
        let benchmark = CommandLine.arguments.contains("--benchmark")
        let empty = try AppPage(page([]), after: nil)
        precondition(empty.rows.isEmpty && empty.next == nil)
        let states = try AppPage(page([entry(1), entry(2, state: "disabled"), entry(3, state: "deleted")]), after: nil)
        precondition(states.rows.map(\.unavailable) == [nil, "Disabled", "Deleted"])
        precondition(states.rows[0].publisher == publisher && states.rows[0].appID == app(1))
        precondition(states.rows[0].build == build)
        var cursor: String?
        for index in 0..<8 {
            let ids = (index * 32 + 1)...(index * 32 + 32)
            let result = try AppPage(page(ids.map { entry($0) }, next: index == 7 ? nil : app(ids.upperBound)), after: cursor)
            precondition(result.rows.count == 32 && result.rows.first?.appID == app(ids.lowerBound))
            cursor = result.next
        }
        precondition(cursor == nil)
        let deleted = try AppPage(page([], next: app(32)), after: app(1))
        precondition(deleted.next == app(32))
        rejects(page([entry(2), entry(1)]))
        rejects(page([entry(1), entry(1)]))
        rejects(page((1...33).map { entry($0) }))
        rejects(page([entry(1)], next: app(0)))
        rejects(page([], next: app(1)), after: app(1))
        rejects(page([entry(1)]), after: app(1))
        rejects(page([entry(1, state: "unknown")]))
        for invalid in ["", "Notes", "notes..dev", "notes_1", String(repeating: "a", count: 65)] {
            var malformed = entry(1); malformed["identity"] = ["publisher": publisher, "app_id": invalid]
            rejects(page([malformed]))
        }
        var wrongPublisher = entry(1); wrongPublisher["identity"] = ["publisher": "1", "app_id": app(1)]
        rejects(page([wrongPublisher]))
        for invalid in ["bad" as Any, NSNull()] {
            var malformed = entry(1); malformed["build_id"] = invalid
            rejects(page([malformed]))
        }
        rejects(["apps": [entry(1)]])
        for count in [0, 4, 32] {
            let fixture = page((0..<count).map { entry($0 + 1) })
            var times: [UInt64] = []
            for _ in 0..<(benchmark ? 1000 : 1) {
                let start = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                let result = try AppPage(fixture, after: nil)
                if benchmark { times.append(DispatchTime.now().uptimeNanoseconds - start) }
                precondition(result.rows.count == count)
            }
            times.sort()
            if benchmark { print("App page rows=\(count) n=1000 p99=\(times[989]) max=\(times[999]) ns") }
        }
        print("AppPageTests: publisher/app identity, build, availability, pagination and malformed response rejection passed")
    }
}
