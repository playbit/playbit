import Foundation

/// Local form state only; the runtime owns credentials and durable backup state.
struct RecoverySetup {
    enum Phase { case idle, preparing, failed, ready, saving }
    private(set) var phase = Phase.idle
    private(set) var secret: String?
    private(set) var acknowledged = false
    private(set) var error: String?

    var canCopy: Bool { secret != nil }
    var canAcknowledge: Bool { phase == .ready }
    var canSave: Bool { phase == .ready && acknowledged }
    var canRetry: Bool { phase == .failed }

    mutating func beginPreparation() -> Bool {
        guard phase == .idle || phase == .failed else { return false }
        phase = .preparing
        secret = nil
        acknowledged = false
        error = nil
        return true
    }
    mutating func prepared(_ value: String?) {
        guard phase == .preparing else { return }
        guard let value, !value.isEmpty, value.utf8.prefix(1025).count <= 1024 else {
            preparationFailed("The runtime did not return a valid account key. Try again.")
            return
        }
        secret = value
        phase = .ready
    }
    mutating func preparationFailed(_ message: String) {
        guard phase == .preparing else { return }
        phase = .failed
        error = String(decoding: message.utf8.prefix(4092), as: UTF8.self)
    }
    mutating func acknowledge(_ value: Bool) {
        if canAcknowledge { acknowledged = value }
    }
    mutating func beginSave() -> Bool {
        guard canSave else { return false }
        phase = .saving
        error = nil
        return true
    }
    mutating func saveFailed(_ message: String) {
        guard phase == .saving else { return }
        phase = .ready
        error = String(decoding: message.utf8.prefix(4092), as: UTF8.self)
    }
}
