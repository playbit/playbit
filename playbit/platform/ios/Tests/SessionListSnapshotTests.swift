import Foundation

@main enum SessionListSnapshotTests {
    static func session(_ id: String, _ name: String = "Notes", _ root: String = "1111111111111112") -> [String: Any] {
        let code = id.hasPrefix("72345678") ? String(id.prefix(22)) : Identifier.encode(Array((Identifier.decode(id, bytes: 32) ?? [UInt8](repeating: 0, count: 32)).suffix(16)))
        return ["id": id, "reference": code, "app_name": name, "build_state": ["kind": "ready"], "root_volume": root, "resolved_build_id": Identifier.encode([UInt8](repeating: 0xaa, count: 32))]
    }
    static func value(_ sessions: [[String: Any]], _ volumes: [[String: Any]] = []) -> [String: Any] {
        ["sessions": sessions, "volumes": volumes]
    }
    static func rejected(_ value: [String: Any]) {
        do { _ = try SessionListSnapshot(value); preconditionFailure("Accepted malformed session snapshot") }
        catch {}
    }
    static func main() throws {
        let benchmark = CommandLine.arguments.contains("--benchmark")
        let a = "72345678A" + String(repeating: "a", count: 35)
        let b = "72345678B" + String(repeating: "b", count: 35)
        let c = Identifier.encode([UInt8](repeating: 0xcc, count: 32))
        let empty = try SessionListSnapshot(value([]))
        precondition(empty.rows.isEmpty)
        let single = try SessionListSnapshot(value([session(a)]))
        precondition(single.rows == [.init(id: a, title: "Notes", subtitle: nil, build: Identifier.encode([UInt8](repeating: 0xaa, count: 32)), reference: String(a.prefix(22)))])
        // Only session roots supply subtitles; unrelated long labels do not invalidate the list.
        let longName = String(repeating: "é", count: 129)
        let unrelated = try SessionListSnapshot(value([session(a)], [["id": "1111111111111113", "name": longName]]))
        precondition(unrelated.rows == single.rows)
        rejected(value([session(a)], [["id": "1111111111111112", "name": longName]]))
        var shared = try SessionListSnapshot(value([session(b), session(a)]))
        precondition(shared.rows.map(\.id) == [b, a], "Display order must not change while disambiguating")
        precondition(shared.rows.map(\.subtitle) == ["72345678B", "72345678A"])
        shared.move(from: 1, to: 0)
        precondition(shared.rows.map(\.id) == [a, b])
        shared.move(from: -1, to: 0)
        precondition(shared.rows.map(\.id) == [a, b])
        var malformedReference = session(a)
        malformedReference["reference"] = Identifier.encode([UInt8](repeating: 1, count: 32))
        rejected(value([malformedReference]))
        var changedBuild = session(a)
        changedBuild["resolved_build_id"] = Identifier.encode([UInt8](repeating: 0xbb, count: 32))
        let changedSnapshot = try SessionListSnapshot(value([changedBuild]))
        precondition(changedSnapshot.rows != single.rows,
            "An updated build must refresh artwork even when name/root are unchanged")
        changedBuild["resolved_build_id"] = "../outside"
        rejected(value([changedBuild]))
        changedBuild.removeValue(forKey: "resolved_build_id")
        rejected(value([changedBuild]))
        let roots = [["id": "1111111111111112", "name": "Production"], ["id": "1111111111111113", "name": "Development"]]
        let distinct = try SessionListSnapshot(value([session(a), session(b, "Notes", "1111111111111113"), session(c, "TODO")], roots))
        precondition(distinct.rows.map(\.subtitle) == ["72345678A • Production", "72345678B • Development", nil])
        let missing = try SessionListSnapshot(value([session(a), session(b, "Notes", "1111111111111113")]))
        precondition(missing.rows[1].subtitle == "72345678B • Volume 1111111111111113")
        var pending = session(a)
        pending["build_state"] = ["kind": "download_failed", "error": "blob not found"]
        pending["build_message"] = "Build download failed: blob not found"
        let unavailable = try SessionListSnapshot(value([pending]))
        precondition(unavailable.rows[0].title == "Notes" && unavailable.rows[0].build == nil)
        precondition(unavailable.rows[0].subtitle == "Build download failed: blob not found")
        let renamed = try SessionListSnapshot(value([session(a, "Café 日本語")]))
        precondition(renamed.rows[0].id == a && renamed.rows[0].title == "Café 日本語")
        for malformed in [value([session(a), session(a)]), value([session("../outside")]),
                          value([session(a, "")]), value([session(a, String(repeating: "é", count: 129))]),
                          value([session(a, "Notes", "../root")]),
                          value([session(a)], [["id": "1111111111111112", "name": ""]]),
                          value([session(a)], roots + [roots[0]]),
                          value(Array(repeating: session(a), count: 257)),
                          ["sessions": []], ["volumes": []]] {
            rejected(malformed)
        }
        // Match Foundation decoding at the real callback boundary. No file I/O,
        // JSON decoding, UIKit layout or physical presentation inside this timer.
        for collision in [false, true] {
            for count in [0, 4, 128, 256] {
                let items = (0..<count).map { i in session(Identifier.encode(collision
                    ? [UInt8](repeating: 0, count: 30) + [UInt8(i >> 8), UInt8(i & 255)]
                    : (0..<32).map { j in UInt8((i * 37 + j * 113) & 255) }),
                    count == 256 ? String(repeating: "é", count: 128) : "App \(i % 4)", i % 2 == 0 ? "1111111111111112" : "1111111111111113") }
                let data = try JSONSerialization.data(withJSONObject: value(items, roots))
                let fixture = try JSONSerialization.jsonObject(with: data) as! [String: Any]
                var samples: [UInt64] = []
                for _ in 0..<(benchmark ? 1000 : 1) {
                    let start = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                    let snapshot = try SessionListSnapshot(fixture)
                    if benchmark { samples.append(DispatchTime.now().uptimeNanoseconds - start) }
                    precondition(snapshot.rows.count == count)
                    precondition(Set(snapshot.rows.map(\.id)).count == count)
                }
                samples.sort()
                if benchmark { print("session labels collision=\(collision) rows=\(count) n=1000 p50=\(samples[499]) p99=\(samples[989]) max=\(samples[999]) ns") }
            }
        }
        print("SessionListSnapshotTests: names, root distinctions, colliding prefixes, full IDs and bounds passed")
    }
}
