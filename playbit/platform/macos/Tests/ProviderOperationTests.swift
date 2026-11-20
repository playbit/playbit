import Foundation

@main
struct ProviderOperationTests {
    static func main() {
        // Cancellation must complete before a backend reply and retain no
        // second delivery when that reply eventually arrives.
        let delivered = DispatchSemaphore(value: 0)
        let cancelled = ProviderOperation<Int>(cancelled: -1) { value in
            precondition(value == -1)
            delivered.signal()
        }
        cancelled.progress.cancel()
        precondition(delivered.wait(timeout: .now() + 1) == .success)
        precondition(!cancelled.finish(1))

        // An XPC failure/reply can race cancellation. Exactly one caller wins.
        for _ in 0..<128 {
            let lock = NSLock()
            var deliveries = 0
            let operation = ProviderOperation<Int>(cancelled: -1) { _ in
                lock.lock()
                deliveries += 1
                lock.unlock()
            }
            DispatchQueue.concurrentPerform(iterations: 8) { index in
                if index == 0 { operation.progress.cancel() }
                else { operation.finish(index) }
            }
            precondition(deliveries == 1)
            precondition(operation.progress.completedUnitCount == 1)
        }
        let item: [String: Any] = ["id": "entry", "content_version": [UInt8](repeating: 1, count: 32),
            "metadata_version": [UInt8](repeating: 2, count: 32), "uploaded": false]
        var uploaded = item
        uploaded["uploaded"] = true
        let uploads = ProviderUploads()
        var callbacks = 0
        let operation = ProviderOperation<Int>(cancelled: -1) { value in
            precondition(value == 1)
            callbacks += 1
        }
        var verify: (([String: Any]?) -> Void)?
        operation.finish(1, upload: { progress in
            uploads.track(item, progress: progress) { verify = $0 }
        })
        precondition(callbacks == 1)
        precondition(operation.progress.completedUnitCount == 1 && operation.progress.totalUnitCount == 2)
        // An ACK consumed before registration is recovered by one explicit
        // metadata read; ordinary later enumeration uses the same observation.
        verify?(uploaded)
        precondition(operation.progress.isFinished)
        precondition(callbacks == 1)

        var next = item
        next["content_version"] = [UInt8](repeating: 3, count: 32)
        let older = Progress(totalUnitCount: 2)
        let newer = Progress(totalUnitCount: 2)
        var oldRead: (([String: Any]?) -> Void)?
        uploads.track(item, progress: older) { oldRead = $0 }
        uploads.track(next, progress: newer) { _ in }
        // A read captured before the newer registration cannot cancel it when
        // that read's completion arrives out of order.
        oldRead?(uploaded)
        precondition(older.isFinished && !newer.isCancelled && !newer.isFinished)
        uploads.invalidate()
        precondition(newer.isCancelled)

        // Capacity pressure cancels only upload progress. The successful durable
        // local reply has already been claimed and is never replaced by cancel.
        let bounded = ProviderUploads()
        var pending: [Progress] = []
        for _ in 0..<129 {
            let request = ProviderOperation<Int>(cancelled: -1) { precondition($0 == 1) }
            request.finish(1, upload: { progress in bounded.track(item, progress: progress) { _ in } })
            pending.append(request.progress)
        }
        precondition(pending.filter(\.isCancelled).count == 1)
        bounded.invalidate()
        precondition(pending.allSatisfy(\.isCancelled))
        print("File Provider cancellation, upload progress and capacity races passed")
    }
}
