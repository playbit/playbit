import Foundation

/// A complete local snapshot (Logs with no cursor), copied from the native reply.
struct LogSnapshot: Equatable {
    let lines: [String]
    let dropped: UInt64
    let engine: Data
    let instance: UInt64
    let next: UInt64

    enum Failure: LocalizedError {
        case invalid
        var errorDescription: String? { "The app log response is invalid. Previous logs are retained." }
    }

    init(lines: [String], dropped: UInt64, engine: Data, instance: UInt64, next: UInt64, reset: Bool) throws {
        guard lines.count <= 256, engine.count == 16, !reset else { throw Failure.invalid }
        var bytes = 0
        for line in lines {
            bytes += line.utf8.count
            guard bytes <= 64 * 1024 else { throw Failure.invalid }
        }
        let (count, overflow) = dropped.addingReportingOverflow(UInt64(lines.count))
        guard !overflow, count == next else { throw Failure.invalid }
        self.lines = lines; self.dropped = dropped; self.engine = engine
        self.instance = instance; self.next = next
    }
}
