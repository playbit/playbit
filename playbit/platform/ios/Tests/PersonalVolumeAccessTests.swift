import Foundation

@main enum PersonalVolumeAccessTests {
    static func identifier(_ number: Int) -> String {
        Identifier.encode([UInt8](repeating: 0, count: 14) + [UInt8(number >> 8), UInt8(number & 255)])
    }
    static func payload(_ count: Int) -> [String: Any] {
        ["volumes": (0..<count).map { ["id": identifier($0 + 1), "name": String(repeating: "n", count: 64)] }, "grants": []]
    }
    static func rejected(_ value: [String: Any]) {
        do { _ = try PersonalVolumeAccess(value); preconditionFailure("Accepted an invalid consent snapshot") }
        catch {}
    }
    static func main() throws {
        let benchmark = CommandLine.arguments.contains("--benchmark")
        var value = payload(4)
        let id = identifier(1)
        value["grants"] = [["volume": id, "write": true]]
        var draft = try PersonalVolumeAccess(value)
        precondition(draft.rows[0].choice == 2)
        precondition(draft.change(id, to: 1))
        let first = draft.beginSave()!
        precondition(first.count == 1 && first[0].write == false)
        precondition(!draft.change(id, to: 2) && draft.beginSave() == nil)
        precondition(draft.finishSave(success: false))
        precondition(draft.rows[0].choice == 1 && draft.phase == .ready)
        precondition(!draft.change(id, to: 3))
        precondition(draft.change(id, to: 0))
        precondition(draft.beginSave()!.isEmpty)
        precondition(draft.finishSave(success: true))
        precondition(!draft.finishSave(success: false) && draft.beginSave() == nil)
        // Discarding a draft cannot change the previously loaded snapshot.
        let original = try PersonalVolumeAccess(value)
        precondition(original.rows[0].choice == 2)
        let fresh = try PersonalVolumeAccess(["volumes": payload(4)["volumes"]!, "grants": NSNull()])
        precondition(fresh.rows.allSatisfy { $0.choice == 0 })
        rejected([:]); rejected(payload(129))
        let one = payload(1)["volumes"] as! [[String: Any]]
        rejected(["volumes": one + one, "grants": []])
        rejected(["volumes": [["id": id, "name": ""]], "grants": []])
        rejected(["volumes": [["id": "x", "name": "Notes"]], "grants": []])
        rejected(["volumes": [["id": id, "name": String(repeating: "a", count: 257)]], "grants": []])
        rejected(["volumes": one, "grants": [["volume": id, "write": 1]]])
        rejected(["volumes": one, "grants": [["volume": id]]])
        rejected(["volumes": one, "grants": [["volume": Identifier.encode([UInt8](repeating: 0xff, count: 16)), "write": false]]])
        rejected(["volumes": one, "grants": [["volume": id, "write": false], ["volume": id, "write": true]]])
        for count in [0, 4, 128] {
            let value = payload(count)
            var samples: [UInt64] = []
            for _ in 0..<(benchmark ? 1000 : 1) {
                let start = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                var model = try PersonalVolumeAccess(value)
                for row in model.rows { precondition(model.change(row.id, to: 2)) }
                let grants = model.beginSave()!
                let volumes = grants.flatMap { Identifier.decode($0.volume, bytes: 16)! }
                let writes = grants.map { UInt8($0.write ? 1 : 0) }
                if benchmark { samples.append(DispatchTime.now().uptimeNanoseconds - start) }
                precondition(volumes.count == count * 16 && writes.count == count)
                precondition(volumes.count + writes.count + 64 <= 16 * 1024)
                precondition(model.finishSave(success: false))
            }
            samples.sort()
            if benchmark { print("consent parse + edit all + prepare typed spans rows=\(count) n=1000 p50=\(samples[499]) p99=\(samples[989]) max=\(samples[999]) ns") }
        }
        print("PersonalVolumeAccessTests: complete snapshots, busy state, failure retry, revocation and limits passed")
    }
}
