import Foundation

struct GuestTextEdit: Equatable {
    let start: Int
    let end: Int
    let insertedLength: Int
}

// UIKit supplies UTF-16 ranges before mutation. Keep those exact ranges;
// a notification without a matching delegate call is explicitly snapshot-only.
struct GuestTextEdits {
    static let countMax = 256
    static let textBytesMax = 256 * 1024
    static let payloadBytesMax = textBytesMax + 4 + countMax * 12
    private var edits: [GuestTextEdit]? = []
    private var before: (text: String, range: NSRange, expected: String)?
    private var composition: (text: String, range: NSRange)?

    mutating func willChange(_ text: String, range: NSRange, replacement: String) -> Bool {
        guard let span = Range(range, in: text), (edits?.count ?? 0) < Self.countMax else { return false }
        let expected = text.replacingCharacters(in: span, with: replacement)
        guard expected.utf8.count <= Self.textBytesMax else { return false }
        before = (text, range, expected)
        return true
    }
    mutating func didChange(_ text: String, composing: Bool) {
        if composing {
            if composition == nil {
                if let before, before.expected == text { composition = (before.text, before.range) }
                else { edits = nil }
            }
            before = nil
            return
        }
        defer { before = nil; composition = nil }
        guard edits != nil else { return }
        let source: (text: String, range: NSRange)
        if let composition { source = composition }
        else if let before, before.expected == text { source = (before.text, before.range) }
        else { edits = nil; return }
        guard let range = Range(source.range, in: source.text) else { edits = nil; return }
        let start = source.text[..<range.lowerBound].utf8.count
        let end = source.text[..<range.upperBound].utf8.count
        let inserted = text.utf8.count - source.text.utf8.count + end - start
        let old = Array(source.text.utf8), value = Array(text.utf8)
        guard inserted >= 0, start + inserted <= value.count,
            old[..<start].elementsEqual(value[..<start]),
            old[end...].elementsEqual(value[(start + inserted)...]),
            edits!.count < Self.countMax else { edits = nil; return }
        edits!.append(GuestTextEdit(start: start, end: end, insertedLength: inserted))
    }
    func payload(_ text: String) -> Data {
        var bytes = Data(capacity: 4 + (edits?.count ?? 0) * 12 + text.utf8.count)
        var words: [UInt32] = [edits.map { UInt32($0.count) } ?? UInt32.max]
        for edit in edits ?? [] { words += [UInt32(edit.start), UInt32(edit.end), UInt32(edit.insertedLength)] }
        for var word in words.map({ $0.littleEndian }) {
            withUnsafeBytes(of: &word) { bytes.append(contentsOf: $0) }
        }
        bytes.append(contentsOf: text.utf8)
        return bytes
    }
    mutating func queued() { edits = [] }
    mutating func reset() { self = GuestTextEdits() }
}

// The native editor advances this even when text returns to an earlier value.
// Comparing strings cannot distinguish a stale submission acknowledgment.
struct GuestEditRevision {
    private(set) var value: UInt64 = 0
    private(set) var accepted: UInt64 = 0
    var dirty: Bool { value != accepted }
    mutating func edit() { value += 1 }
    mutating func acknowledge(_ revision: UInt64) { accepted = max(accepted, revision) }
    func accepts(_ acknowledgment: UInt64?) -> Bool { !dirty || acknowledgment == value }
}

struct GuestInput {
    let kind: Int
    let target: UInt32
    let data: Data
    let revision: UInt64
    var requiresPreviousSuccess = false
    var bytes: Int { data.count + 128 }
}

// Owned on the main thread. The first item remains charged until its response
// is applied, and only that item is handed to EngineHost's serial worker.
struct GuestInputQueue {
    private var items: [GuestInput] = []
    private(set) var bytes = 0
    var first: GuestInput? { items.first }
    var count: Int { items.count }
    mutating func admit(_ inputs: [GuestInput]) -> Bool {
        let size = inputs.reduce(0) { $0 + $1.bytes }
        guard inputs.allSatisfy({ $0.data.count <= ($0.kind == 14 ? GuestTextEdits.payloadBytesMax : GuestTextEdits.textBytesMax) }), items.count + inputs.count <= 32,
            bytes + size <= 2 * 1024 * 1024 else { return false }
        items.append(contentsOf: inputs); bytes += size
        return true
    }
    mutating func finish(success: Bool) {
        guard !items.isEmpty else { return }
        bytes -= items.removeFirst().bytes
        if !success {
            while items.first?.requiresPreviousSuccess == true { bytes -= items.removeFirst().bytes }
        }
    }
}


// The iPhone host owns a safe-area/keyboard-bounded scroll viewport. Its guest
// content excludes the host's status/update rows and 16-point outer insets.
struct GuestSurface: Equatable {
    let width: UInt32
    let height: UInt32
    let pixelScale: Float
    init?(width: Double, height: Double, pixelScale: Float) {
        guard width.isFinite, height.isFinite, width >= 0, height >= 0,
            width <= Double(UInt32.max), height <= Double(UInt32.max),
            pixelScale.isFinite, pixelScale > 0 else { return nil }
        self.width = UInt32(width.rounded(.down)); self.height = UInt32(height.rounded(.down))
        self.pixelScale = pixelScale
    }
    var input: GuestInput {
        // SurfaceChanged's exact little-endian PBSurface payload. UIKit delivers
        // touch and software/hardware keyboard input; do not infer a mouse.
        var bytes = Data(capacity: 16)
        for var word in [width, height, pixelScale.bitPattern, 6].map({ $0.littleEndian }) {
            withUnsafeBytes(of: &word) { bytes.append(contentsOf: $0) }
        }
        return GuestInput(kind: 15, target: 0, data: bytes, revision: 0)
    }
}
struct GuestSurfaceDelivery {
    private var admitted: GuestSurface?
    mutating func reset() { admitted = nil }
    mutating func notify(_ surface: GuestSurface, enqueue: (GuestInput) -> Bool) {
        if admitted != surface && enqueue(surface.input) { admitted = surface }
    }
}
