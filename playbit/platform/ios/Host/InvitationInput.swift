import Foundation

enum InvitationInput {
    static let maximumBytes = 32 * 1024

    static func bounded(_ text: String) -> Bool {
        text.utf8.prefix(maximumBytes + 1).count <= maximumBytes
    }
    static func canSubmit(_ text: String) -> Bool {
        !text.isEmpty && bounded(text)
    }
    /// Normalize only an explicit submission, not every edit/selection change.
    static func submission(_ text: String) -> String? {
        guard bounded(text) else { return nil }
        let value = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return value.isEmpty ? nil : value
    }
    static func acceptsChange(_ current: String, range: NSRange, replacement: String) -> Bool {
        guard bounded(current), bounded(replacement) else { return false }
        let count = current.utf16.count
        guard range.location <= count, range.length <= count - range.location else { return false }
        return bounded((current as NSString).replacingCharacters(in: range, with: replacement))
    }
}
