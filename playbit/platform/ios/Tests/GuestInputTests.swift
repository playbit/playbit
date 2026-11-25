// Run with Host/GuestInput.swift via swiftc; no simulator or runtime mocks.
import Foundation
import CoreGraphics

@main enum GuestInputTests {
    static func main() {
        gestureTerminalsAtCapacity()
        precondition(MemoryLayout<GuestEditRevision>.stride <= 32)
        var revision = GuestEditRevision()
        precondition(revision.accepts(nil))
        revision.edit(); let submitted = revision.value
        revision.edit(); revision.edit() // A → B → A still has a newer revision.
        precondition(!revision.accepts(submitted))
        precondition(!revision.accepts(nil))
        precondition(revision.accepts(revision.value))
        revision.acknowledge(submitted); precondition(revision.dirty)
        revision.acknowledge(revision.value); precondition(!revision.dirty && revision.accepts(nil))
        var queue = GuestInputQueue()
        let change = GuestInput(kind: 14, target: 1, data: Data("current".utf8), revision: revision.value)
        let add = GuestInput(kind: 2, target: 2, data: Data(), revision: revision.value, requiresPreviousSuccess: true)
        precondition(queue.admit([change, add, change]))
        queue.finish(success: false)
        precondition(queue.count == 1 && queue.first?.kind == 14) // Failed draft never sends stale Add.
        queue.finish(success: true); precondition(queue.bytes == 0)
        let secondDraft = GuestInput(kind: 14, target: 9, data: Data("another editor".utf8), revision: 1, requiresPreviousSuccess: true)
        precondition(queue.admit([change, secondDraft, add, change]))
        queue.finish(success: false)
        precondition(queue.count == 1 && queue.first?.target == 1)
        queue.finish(success: true)
        precondition(queue.admit(Array(repeating: change, count: 31)))
        precondition(!queue.admit([change, add])) // Atomic admission of draft + Add.
        precondition(queue.count == 31)
        precondition(queue.admit([change])); precondition(queue.count == 32)
        precondition(!queue.admit([change]))
        for _ in 0..<32 { queue.finish(success: true) }
        precondition(queue.count == 0 && queue.bytes == 0)
        let maximum = GuestInput(kind: 3, target: 1, data: Data(repeating: 65, count: GuestTextEdits.textBytesMax), revision: 0)
        precondition(queue.admit(Array(repeating: maximum, count: 7)))
        precondition(!queue.admit([maximum])) // Includes bounded request envelope accounting.
        let tooLarge = GuestInput(kind: 3, target: 1, data: Data(repeating: 65, count: GuestTextEdits.textBytesMax + 1), revision: 0)
        precondition(!queue.admit([tooLarge]))
        for _ in 0..<7 { queue.finish(success: true) }
        var maximumEdits = GuestTextEdits()
        let maximumText = String(repeating: "é", count: GuestTextEdits.textBytesMax / 2)
        precondition(maximumEdits.willChange("", range: NSRange(location: 0, length: 0), replacement: maximumText))
        maximumEdits.didChange(maximumText, composing: false)
        precondition(!maximumEdits.willChange(maximumText, range: NSRange(location: maximumText.utf16.count, length: 0), replacement: "x"))
        let maximumChange = GuestInput(kind: 14, target: 1, data: maximumEdits.payload(maximumText), revision: 1)
        precondition(maximumChange.data.count == GuestTextEdits.textBytesMax + 16)
        precondition(queue.admit([maximumChange]))
        queue.finish(success: true)
        // UIKit's repeated-text selection is retained, and UTF-16 emoji ranges become bytes.
        var edits = GuestTextEdits()
        precondition(edits.willChange("éaaa", range: NSRange(location: 1, length: 0), replacement: "a"))
        edits.didChange("éaaaa", composing: false)
        precondition(edits.payload("éaaaa") == Data([1,0,0,0, 2,0,0,0, 2,0,0,0, 1,0,0,0] + Array("éaaaa".utf8)))
        edits.queued()
        precondition(edits.willChange("é🦀aaa", range: NSRange(location: 1, length: 2), replacement: ""))
        edits.didChange("éaaa", composing: false)
        precondition(edits.payload("éaaa") == Data([1,0,0,0, 2,0,0,0, 6,0,0,0, 0,0,0,0] + Array("éaaa".utf8)))
        edits.queued()
        precondition(edits.willChange("éaaa", range: NSRange(location: 1, length: 1), replacement: "に"))
        edits.didChange("éにaa", composing: true)
        precondition(edits.willChange("éにaa", range: NSRange(location: 1, length: 1), replacement: "日本"))
        edits.didChange("é日本aa", composing: false)
        precondition(edits.payload("é日本aa") == Data([1,0,0,0, 2,0,0,0, 3,0,0,0, 6,0,0,0] + Array("é日本aa".utf8)))
        edits.queued()
        // A missing first native range cannot be recovered from a later marked update.
        edits.didChange("に", composing: true)
        precondition(edits.willChange("に", range: NSRange(location: 0, length: 1), replacement: "日本"))
        edits.didChange("日本", composing: false)
        precondition(edits.payload("日本").prefix(4) == Data([255,255,255,255]))
        edits.queued()
        edits.didChange("Autofill", composing: false)
        precondition(edits.payload("Autofill").prefix(4) == Data([255,255,255,255]))
        let surface = GuestSurface(width: 368.9, height: 448.1, pixelScale: 2)!
        precondition(MemoryLayout<GuestSurfaceDelivery>.stride <= 32)
        precondition(surface.input.kind == 15 && surface.input.target == 0)
        precondition(surface.input.revision == 0 && !surface.input.requiresPreviousSuccess)
        precondition(surface.input.data == Data([112, 1, 0, 0, 192, 1, 0, 0, 0, 0, 0, 64, 6, 0, 0, 0]))
        precondition(GuestSurface(width: 0, height: 0, pixelScale: 3) != nil)
        for invalid in [Double.nan, .infinity, -.infinity, -1, Double(UInt32.max) + 1] {
            precondition(GuestSurface(width: invalid, height: 448, pixelScale: 2) == nil)
            precondition(GuestSurface(width: 368, height: invalid, pixelScale: 2) == nil)
        }
        for scale: Float in [.nan, .infinity, -.infinity, 0, -1] {
            precondition(GuestSurface(width: 368, height: 448, pixelScale: scale) == nil)
        }
        var delivery = GuestSurfaceDelivery(), admitted: [GuestInput] = []
        delivery.notify(surface) { _ in false }
        delivery.notify(surface) { admitted.append($0); return true }
        delivery.notify(surface) { _ in preconditionFailure("duplicate surface") }
        precondition(admitted.count == 1)
        delivery.reset()
        delivery.notify(surface) { admitted.append($0); return true }
        delivery.notify(GuestSurface(width: 640, height: 368, pixelScale: 2)!) {
            admitted.append($0); return true
        }
        precondition(admitted.count == 3)
        print("GuestInputTests: surface encoding/admission/reset, revision, failed-draft barrier, atomic admission and byte/count bounds passed")
    }

    static func gestureTerminalsAtCapacity() {
        // Leave room for two of three gesture beginnings and their eventual endings.
        var queue = GuestInputQueue(), sequences = GuestInputSequences(), reserved = 0
        let input = GuestInput(kind: 25, target: 0, data: Data(repeating: 0, count: 48), revision: 0)
        precondition(queue.admit(Array(repeating: input, count: 27)))
        var admissions: [Int] = []
        func send(_ reservation: Int) -> Bool {
            guard queue.count + 1 + reserved + reservation <= 32, queue.admit([input]) else { return false }
            reserved += reservation
            admissions.append(reservation)
            return true
        }
        for kind in UInt8(0)...2 { sequences.deliver(.began, kind: kind, point: .zero, send: send) }
        for _ in 0..<10 {
            for kind in UInt8(0)...2 { sequences.deliver(.changed, kind: kind, point: .zero, send: send) }
        }
        precondition(admissions == [1, 1, 0] && reserved == 2)
        for kind in UInt8(0)...2 { sequences.deliver(.ended, kind: kind, point: .zero, send: send) }
        precondition(admissions == [1, 1, 0, -1, -1] && reserved == 0 && sequences.points.isEmpty)
        queue.finish(success: true)
        sequences.deliver(.ended, kind: 0, point: .zero) { _ in preconditionFailure("duplicate terminal") }
        sequences.deliver(.changed, kind: 2, point: .zero) { _ in preconditionFailure("unadmitted gesture") }
    }
}
