import Foundation

// Cancellation completes the OS request immediately. An already submitted,
// durable mutation may still finish; its ordinary retry identity is retained.
// Backend admission stays held until the actual XPC reply, even after cancel.
final class ProviderOperation<Reply> {
    let progress = Progress(totalUnitCount: 1)
    private let lock = NSLock()
    private var completed = false
    private let completion: (Reply) -> Void

    init(cancelled: Reply, completion: @escaping (Reply) -> Void) {
        self.completion = completion
        progress.cancellationHandler = { [weak self] in self?.finish(cancelled) }
    }

    @discardableResult
    func finish(_ reply: Reply, upload: ((Progress) -> Void)? = nil) -> Bool {
        lock.lock()
        if completed { lock.unlock(); return false }
        completed = true
        lock.unlock()
        progress.cancellationHandler = nil
        if upload != nil { progress.totalUnitCount = 2 }
        progress.completedUnitCount = 1
        upload?(progress)
        completion(reply)
        return true
    }
}

/// Pending upload progress owns no runtime, store or execution-time assertion.
/// A process-wide credit bounds retained Progress objects across domain clients.
final class ProviderUploads {
    private static let admissions = DispatchSemaphore(value: 128)
    private struct Pending {
        let entry: String
        let version: Data
        let progress: Progress
    }
    private let lock = NSLock()
    private var pending: [UUID: Pending] = [:]
    private var active = true

    private static func version(_ item: [String: Any]) -> Data {
        Data((item["content_version"] as? [UInt8] ?? []) + (item["metadata_version"] as? [UInt8] ?? []))
    }
    func track(_ item: [String: Any], progress: Progress,
        verify: (@escaping ([String: Any]?) -> Void) -> Void) {
        guard let entry = item["id"] as? String,
              Self.admissions.wait(timeout: .now()) == .success else { progress.cancel(); return }
        let token = UUID()
        lock.lock()
        guard active else { lock.unlock(); Self.admissions.signal(); progress.cancel(); return }
        pending[token] = Pending(entry: entry, version: Self.version(item), progress: progress)
        // Every captured operation committed before this explicit local read.
        // Later registrations are excluded: callback order is not commit order.
        let captured = pending.filter { $0.value.entry == entry }.map(\.key)
        lock.unlock()
        progress.cancellationHandler = { [weak self] in self?.settle([token], cancel: true) }
        if progress.isCancelled { settle([token], cancel: true) }
        verify { [weak self] current in
            guard let self, let current else { return }
            self.observe(current)
            let version = Self.version(current)
            self.lock.lock()
            let superseded = captured.filter { self.pending[$0].map { $0.version != version } ?? false }
            self.lock.unlock()
            self.settle(superseded, cancel: true)
        }
    }
    func observe(_ item: [String: Any]) {
        guard item["uploaded"] as? Bool == true, let entry = item["id"] as? String else { return }
        let version = Self.version(item)
        lock.lock()
        let complete = pending.filter { $0.value.entry == entry && $0.value.version == version }.map(\.key)
        lock.unlock()
        settle(complete, cancel: false)
    }
    func observe(_ response: Result<[String: Any], Error>) {
        guard case .success(let value) = response else { return }
        if let item = value["item"] as? [String: Any] { observe(item) }
        let page = value["page"] as? [String: Any] ?? value
        for item in page["items"] as? [[String: Any]] ?? [] { observe(item) }
    }
    private func settle(_ tokens: [UUID], cancel: Bool) {
        lock.lock()
        let values = tokens.compactMap { pending.removeValue(forKey: $0) }
        lock.unlock()
        for value in values {
            value.progress.cancellationHandler = nil
            if cancel { value.progress.cancel() }
            else { value.progress.completedUnitCount = value.progress.totalUnitCount }
            Self.admissions.signal()
        }
    }
    func invalidate() {
        lock.lock(); active = false; let tokens = Array(pending.keys); lock.unlock()
        settle(tokens, cancel: true)
    }
    deinit { invalidate() }
}
