import Foundation
import FileProvider

final class ProviderClient {
    let uploads = ProviderUploads()
    let account: String
    private let lock = NSLock()
    private var invalidated = false
    init(account: String) { self.account = account }
    func invalidate() { lock.lock(); invalidated = true; lock.unlock(); uploads.invalidate() }
    func call(_ fields: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) {
        guard ProviderCall.admissions.wait(timeout: .now()) == .success else {
            completion(.failure(CocoaError(.userCancelled))); return
        }
        let call = ProviderCall(operation: fields["op"] as? String ?? "") { result in
            self.uploads.observe(result)
            completion(result)
        }
        call.start {
            guard Identifier.valid(self.account) else { throw NSFileProviderError(.noSuchItem) }
            let ledger = try ProviderLedger.shared()
            let active = try ledger.withLock(exclusive: false) { try ledger.read() }
            guard let active, active.account == self.account else { throw NSFileProviderError(.notAuthenticated) }
            self.lock.lock()
            let valid = !self.invalidated
            self.lock.unlock()
            guard valid else { throw CocoaError(.userCancelled) }
            // Retained domain instances capture a fresh generation for each
            // call; already admitted work keeps its original account lease.
            let runtime = try ProviderRuntimes.acquire(active, ledger: ledger)
            return (runtime, fields)
        }
    }
    static func operationError(_ message: String, operation: String) -> NSError {
        let reading = ["root", "volumes", "catalog", "volume_item", "list", "scan", "item", "fetch", "anchor"].contains(operation)
        return NSError(domain: NSCocoaErrorDomain, code: reading ? CocoaError.fileReadUnknown.rawValue : CocoaError.fileWriteUnknown.rawValue,
            userInfo: [NSLocalizedDescriptionKey: message])
    }
}

/// Acquire execution time before opening any locked profile. The OS may deny or
/// expire this activity; stop native work and release every database lock before
/// its callback returns. A completed Files call never promises future execution.
private final class ProviderCall {
    static let admissions = DispatchSemaphore(value: 7)
    private let lock = NSLock()
    private let setup = DispatchGroup()
    private let closed = DispatchGroup()
    private var started = false
    private var expired = false
    private var finished = false
    private var owner: ProviderRuntime?
    private let operation: String
    private var completion: ((Result<[String: Any], Error>) -> Void)?

    init(operation: String, completion: @escaping (Result<[String: Any], Error>) -> Void) {
        self.operation = operation
        self.completion = completion
        setup.enter(); closed.enter()
    }
    func start(_ acquire: @escaping () throws -> (ProviderRuntime, [String: Any])) {
        ProcessInfo.processInfo.performExpiringActivity(withReason: "Finish Playbit Files work") { expired in
            if expired { self.expire(); return }
            self.lock.lock()
            guard !self.started else { self.lock.unlock(); return }
            self.started = true
            self.lock.unlock()
            do {
                let (runtime, fields) = try acquire()
                self.lock.lock()
                self.owner = runtime
                let expired = self.expired
                self.lock.unlock()
                self.setup.leave()
                if expired { ProviderRuntimes.retire(runtime) }
                runtime.request(fields) { result in
                    self.complete(result)
                    runtime.releaseRequest { self.finish() }
                }
            } catch {
                self.setup.leave()
                self.complete(.failure(error))
                self.finish()
            }
            // One bounded activity per admitted call; native close, including
            // any admitted local commit, ends the execution-time assertion.
            self.closed.wait()
        }
    }
    private func expire() {
        lock.lock()
        guard !finished else { lock.unlock(); return }
        expired = true
        if !started {
            started = true
            lock.unlock()
            setup.leave()
            complete(.failure(CocoaError(.userCancelled)))
            finish()
            return
        }
        lock.unlock()
        setup.wait()
        lock.lock(); let runtime = owner; lock.unlock()
        if let runtime { ProviderRuntimes.retire(runtime) }
        // Retirement interrupts network waits and drains admitted commits. Keep
        // their actual result rather than inventing a failed/replayable mutation.
        closed.wait()
    }
    private func complete(_ result: Result<[String: Any], Error>) {
        lock.lock(); let callback = completion; completion = nil; lock.unlock()
        callback?(result.mapError { error in
            let value = error as NSError
            if value.domain == NSCocoaErrorDomain || value.domain == NSFileProviderErrorDomain { return error }
            return ProviderClient.operationError(value.localizedDescription, operation: operation)
        })
    }
    private func finish() {
        lock.lock()
        guard !finished else { lock.unlock(); return }
        finished = true
        lock.unlock()
        Self.admissions.signal()
        closed.leave()
    }
}
