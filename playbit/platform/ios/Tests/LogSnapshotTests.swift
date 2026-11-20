import Foundation

@main enum LogSnapshotTests {
    static func snapshot(_ lines: [String], dropped: UInt64 = 0, next: UInt64? = nil,
                         engine: Data = Data(repeating: 7, count: 16), reset: Bool = false) throws -> LogSnapshot {
        try LogSnapshot(lines: lines, dropped: dropped, engine: engine, instance: UInt64.max,
            next: next ?? (dropped &+ UInt64(lines.count)), reset: reset)
    }
    static func rejected(_ action: () throws -> LogSnapshot) {
        do { _ = try action(); preconditionFailure("Accepted malformed log snapshot") } catch {}
    }
    static func main() throws {
        let benchmark = CommandLine.arguments.contains("--benchmark")
        let empty = try snapshot([])
        precondition(empty.lines.isEmpty)
        let repeated = try snapshot(["same", "same", "third\nline", "é\0🔑"], dropped: 10)
        precondition(repeated.lines == ["same", "same", "third\nline", "é\0🔑"] && repeated.dropped == 10)
        rejected { try snapshot(Array(repeating: "", count: 257)) }
        rejected { try snapshot([String(repeating: "x", count: 65_537)]) }
        rejected { try snapshot([String(repeating: "é", count: 32_769)]) }
        rejected { try snapshot([String(repeating: "x", count: 32_768), String(repeating: "x", count: 32_769)]) }
        rejected { try snapshot([], reset: true) }
        rejected { try snapshot(["line"], dropped: UInt64.max) }
        rejected { try snapshot([], engine: Data(repeating: 0, count: 15)) }
        rejected { try snapshot(["line"], next: 0) }
        let maximum = try snapshot([], dropped: UInt64.max)
        precondition(maximum.next == UInt64.max)
        for (count, length) in [(0, 0), (32, 128), (256, 256), (1, 65_536)] {
            let lines = Array(repeating: String(repeating: "x", count: length), count: count)
            var samples: [UInt64] = []
            for _ in 0..<(benchmark ? 1000 : 1) {
                let start = benchmark ? DispatchTime.now().uptimeNanoseconds : 0
                let value = try snapshot(lines)
                if benchmark { samples.append(DispatchTime.now().uptimeNanoseconds - start) }
                precondition(value.lines.count == count)
            }
            samples.sort()
            if benchmark { print("native log snapshot validate lines=\(count) bytes=\(count * length) n=1000 p99=\(samples[989]) max=\(samples[999]) ns") }
        }
        print("LogSnapshotTests: repeated lines, native cursors and size bounds passed")
    }
}
