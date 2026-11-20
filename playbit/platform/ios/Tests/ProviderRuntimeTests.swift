import Foundation

// The production runtime and ledger are compiled unchanged. Stub only OS
// delivery and native network I/O; the CLI does not run the main queue/PushKit.
final class ProviderDomains {
    static let shared = ProviderDomains()
    func connectionResolved(account: String, generation: String) {}
    func reconcile(ledger: ProviderLedger, account: ProviderAccount?, volumes: [ProviderVolume]) {}
    func retryPending() {}
    func signal(account: String, generation: String, acknowledge: @escaping () -> Void) { acknowledge() }
}
enum ProviderClient {
    static func operationError(_ message: String, operation: String) -> NSError {
        NSError(domain: "ProviderRuntimeFixture", code: 1, userInfo: [NSLocalizedDescriptionKey: message])
    }
}

@main
struct ProviderRuntimeTests {
    struct Failure: Error { let message: String }
    static func check(_ value: Bool, _ message: String) throws {
        if !value { throw Failure(message: message) }
    }
    static func wait(_ timeout: TimeInterval = 1, for predicate: () -> Bool) -> Bool {
        let until = DispatchTime.now().uptimeNanoseconds + UInt64(timeout * 1e9)
        repeat {
            if predicate() { return true }
            Thread.sleep(forTimeInterval: 0.001)
        } while DispatchTime.now().uptimeNanoseconds < until
        return false
    }
    static func failure(_ runtime: ProviderRuntime, _ fields: [String: Any]) throws -> NSError {
        let completed = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var observed: NSError?
        runtime.request(fields) { result in
            if case .failure(let error) = result { lock.lock(); observed = error as NSError; lock.unlock() }
            completed.signal()
        }
        try check(completed.wait(timeout: .now() + 1) == .success, "Local failure waited on network startup")
        lock.lock(); let error = observed; lock.unlock()
        guard let error else { throw Failure(message: "Expected local request failure") }
        return error
    }
    static func catalog(_ runtime: ProviderRuntime, known: [String]) throws -> [String: Any] {
        let completed = DispatchSemaphore(value: 0)
        let lock = NSLock()
        var observed: Result<[String: Any], Error>?
        runtime.request(["op": "catalog", "known": known]) { result in
            lock.lock(); observed = result; lock.unlock()
            completed.signal()
        }
        try check(completed.wait(timeout: .now() + 1) == .success, "Catalog waited on unrelated network work")
        lock.lock(); let result = observed; lock.unlock()
        guard let result else { throw Failure(message: "Catalog completed without a result") }
        return try result.get()
    }
    static func run(closeChannel: Bool) throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let ledger = ProviderLedger(root: root)
        let identity = Identifier.encode(withUnsafeBytes(of: UUID().uuid) { Array($0) })
        let volume = Identifier.encode([UInt8](repeating: 2, count: 16))
        let bytes = try JSONSerialization.data(withJSONObject: [
            "account": identity, "email": "provider@example.invalid", "secret": [UInt8](repeating: 7, count: 16),
            "volumes": [["id": volume, "name": "Fixture"]], "retired": [String](),
        ])
        guard let account = try ledger.publish(bytes) else { throw Failure(message: "Missing account") }
        let historical = (1...127).map { Identifier.encode([UInt8](repeating: UInt8($0), count: 16)) }
            + (129...256).map { Identifier.encode([UInt8(truncatingIfNeeded: $0), 1] + [UInt8](repeating: 0, count: 14)) }
        try ledger.exposeVolumes(account: identity, volumes: historical)
        let support = try FileManager.default.url(for: .applicationSupportDirectory,
            in: .userDomainMask, appropriateFor: nil, create: true)
            .appendingPathComponent("Playbit Provider").appendingPathComponent(identity)
        defer { try? FileManager.default.removeItem(at: support) }
        let legacy = support.appendingPathComponent(volume, isDirectory: true)
        try FileManager.default.createDirectory(at: legacy, withIntermediateDirectories: true)
        let pending = legacy.appendingPathComponent("retained-outbox")
        try Data("unsent source".utf8).write(to: pending)
        let runtime = try ProviderRuntimes.acquire(account, ledger: ledger)
        try check(FileManager.default.fileExists(atPath: support.appendingPathComponent("replica").path), "Missing account profile")
        try check(try Data(contentsOf: pending) == Data("unsent source".utf8), "Previous replica source changed")
        let sameAccount = try ProviderRuntimes.acquire(account, ledger: ledger)
        try check(sameAccount === runtime, "Account requests opened another replica")
        sameAccount.releaseRequest(activityFinished: {})
        defer {
            let closed = DispatchSemaphore(value: 0)
            runtime.whenClosed { closed.signal() }
            ProviderRuntimes.retire(runtime)
            precondition(closed.wait(timeout: .now() + 5) == .success)
        }
        // Keep concurrent requests admitted so idle retirement cannot hide the
        // failed-connect notification. Local operations must still complete.
        try runtime.retainRequest()
        let binding = try failure(runtime, ["op": "root"])
        try check(binding.domain == NSCocoaErrorDomain && binding.code == CocoaError.userCancelled.rawValue,
            "A new native profile's account binding became a permanent permission failure")
        let denied = try failure(runtime, ["op": "volume_item", "volume": volume])
        try check(denied.domain == NSCocoaErrorDomain && denied.code == CocoaError.fileReadNoPermission.rawValue,
            "A permanent personal-volume denial became transient")
        let catalogDenied = try failure(runtime, ["op": "catalog"])
        try check(catalogDenied.domain == NSCocoaErrorDomain && catalogDenied.code == CocoaError.fileReadNoPermission.rawValue,
            "An account-catalog permission failure became a write or transient error")
        let local = DispatchGroup()
        for _ in 0..<2 {
            local.enter()
            runtime.request(["op": "volumes"]) { result in
                if case .failure = result { preconditionFailure("Cached operation failed") }
                local.leave()
            }
        }
        try check(local.wait(timeout: .now() + 1) == .success, "Local work waited on failed bootstrap")
        try check(wait { provider_fixture_attempts() == 1 }, "Bootstrap did not fail")
        try check(wait { provider_fixture_readable() == 0 }, "Failed bootstrap left readiness readable during backoff")
        try check(runtime.isActive && runtime.hasRequests, "Idle retirement concealed readiness")
        try check(provider_fixture_advances() == 0, "Backoff advanced the disconnected replica")
        if closeChannel {
            provider_fixture_close_writer()
            try check(wait { !runtime.isActive }, "Closed readiness channel did not retire its owner")
        } else {
            for _ in 0..<8 { provider_fixture_notify() }
            try check(wait { provider_fixture_readable() == 0 }, "Backoff did not coalesce later notifications")
            try check(provider_fixture_attempts() == 1, "Readability bypassed the retry deadline")
            try check(wait(3) { provider_fixture_attempts() == 2 && provider_fixture_advances() > 0 },
                "Deadline did not reconnect and resume advancement")
            try check(wait { provider_fixture_flushes() > 0 }, "Initial authenticated connection did not drain publication")
            provider_fixture_queue_publications()
            try check(provider_fixture_pending_publications() == 3, "Missing retained publication intents")
            let previous = provider_fixture_flushes()
            provider_fixture_reconnect()
            try check(wait { provider_fixture_flushes() > previous && provider_fixture_pending_publications() == 0 },
                "Reconnect left publication intents pending without another mutation")
            // A concurrent snapshot may hold the ledger's short nonblocking
            // lease. Prove that actual admission result, then retry only it.
            var cancellation: NSError?
            for _ in 0..<8 where cancellation == nil {
                do {
                    cancellation = try ledger.withLock(exclusive: true) {
                        try failure(runtime, ["op": "catalog", "known": historical])
                    }
                } catch {
                    let detail = error as NSError
                    guard detail.domain == NSCocoaErrorDomain && detail.code == CocoaError.userCancelled.rawValue else { throw error }
                    Thread.sleep(forTimeInterval: 0.01)
                }
            }
            guard let blocked = cancellation else { throw Failure(message: "Catalog fixture could not acquire its ledger lease") }
            try check(blocked.domain == NSCocoaErrorDomain && blocked.code == CocoaError.userCancelled.rawValue,
                "Contended catalog admission did not cancel: \(blocked.domain)/\(blocked.code): \(blocked.localizedDescription)")
            var catalog: Result<[String: Any], Error> = .failure(blocked)
            for _ in 0..<8 {
                guard case .failure(let error) = catalog else { break }
                let detail = error as NSError
                guard detail.domain == NSCocoaErrorDomain && detail.code == CocoaError.userCancelled.rawValue else { break }
                Thread.sleep(forTimeInterval: 0.01)
                catalog = Result { try Self.catalog(runtime, known: historical) }
            }
            let value: [String: Any]
            do { value = try catalog.get() }
            catch {
                let detail = error as NSError
                throw Failure(message: "Catalog failed: \(detail.domain)/\(detail.code): \(detail.localizedDescription)")
            }
            let rows = value["items"] as? [[String: Any]]
            let discovered = Identifier.encode([UInt8](repeating: 128, count: 16))
            try check(rows?.first?["id"] as? String == discovered, "Catalog namespace items did not cross the native boundary")
            let known = try ledger.knownVolumes(account: identity)
            try check(known.count == 256 && known.contains(discovered), "Catalog returned before durably reserving its exposed volume identity")
            // A restored App Group may retain all historical identities while the
            // private account checkpoint is empty. Root creation must fail before
            // the native request, rather than commit an unexposable volume.
            let creation: [String: Any] = ["op": "create_volume", "id": Identifier.encode([UInt8](repeating: 255, count: 16)), "name": "Overflow"]
            var capacity = try failure(runtime, creation)
            // The newly exposed catalog also schedules a snapshot. A short
            // competing ledger lease cancels admission; retry that specific
            // arbitration result before asserting the persistent capacity gate.
            for _ in 0..<8 where capacity.code == CocoaError.userCancelled.rawValue {
                Thread.sleep(forTimeInterval: 0.01)
                capacity = try failure(runtime, creation)
            }
            try check(capacity.code == CocoaError.fileWriteOutOfSpace.rawValue, "Root creation bypassed historical exposure capacity: \(capacity)")

        }
    }
    static func main() {
        do {
            try run(closeChannel: false)
            try run(closeChannel: true)
            print("Provider runtime: failed-bootstrap readiness, concurrent local work, backoff/reconnect, event-driven publication drain, initial account binding, permanent denials and closed-channel retirement passed")
        } catch {
            fputs("Provider runtime failure: \(error)\n", stderr)
            exit(1)
        }
    }
}
