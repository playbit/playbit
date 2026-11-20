import Foundation

@main struct RecoverySetupTests {
    static func main() {
        var form = RecoverySetup()
        precondition(!form.canCopy && !form.canSave && !form.canAcknowledge)
        precondition(form.beginPreparation())
        precondition(!form.beginPreparation() && !form.beginSave())
        form.preparationFailed("Offline")
        precondition(form.canRetry && !form.canCopy && form.error == "Offline")
        precondition(form.beginPreparation())
        form.prepared(nil)
        precondition(form.canRetry && !form.canSave)
        precondition(form.beginPreparation())
        form.prepared(String(repeating: "x", count: 1025))
        precondition(form.canRetry && !form.canCopy)
        precondition(form.beginPreparation())
        form.prepared("test-only secret")
        precondition(form.canCopy && form.canAcknowledge && !form.canSave)
        precondition(!form.beginPreparation(), "preparation cannot replace an already displayed secret")
        form.acknowledge(true)
        precondition(form.beginSave() && !form.beginSave())
        form.acknowledge(false)
        precondition(form.acknowledged && !form.canAcknowledge && !form.canSave)
        form.saveFailed("Upload failed")
        precondition(form.secret == "test-only secret" && form.canSave && form.canCopy)
        form.acknowledge(false)
        precondition(!form.canSave && !form.beginSave(), "retry requires the current acknowledgment")
        form.acknowledge(true)
        precondition(form.beginSave())
        form.saveFailed(String(repeating: "🦀", count: 4096))
        precondition(form.error!.utf8.count <= 4096 && form.secret == "test-only secret")
        print("Recovery setup admission, retry and acknowledgment checks passed")
        // Explicit reference-device measurement only. CI invokes without this flag.
        if CommandLine.arguments.contains("--measure") { measure() }
    }

    @inline(never) private static func cycle(secret: String, error: String) -> Int {
        var form = RecoverySetup()
        precondition(form.beginPreparation())
        form.preparationFailed(error)
        precondition(form.beginPreparation())
        form.prepared(secret)
        form.acknowledge(true)
        precondition(form.beginSave())
        form.saveFailed(error)
        form.acknowledge(false)
        precondition(!form.canSave && form.canCopy)
        return form.secret!.utf8.count + form.error!.utf8.count
    }

    private static func measure() {
        let secret = String(repeating: "s", count: 1024)
        let error = String(repeating: "🦀", count: 1024)
        var checksum = 0
        var samples = [UInt64]()
        samples.reserveCapacity(10_000)
        for index in 0..<10_100 {
            let start = DispatchTime.now().uptimeNanoseconds
            checksum += cycle(secret: secret, error: error)
            let elapsed = DispatchTime.now().uptimeNanoseconds - start
            if index >= 100 { samples.append(elapsed) }
        }
        samples.sort()
        print("setup cycle (preparation failure/retry, 1KiB secret, 4KiB error, save failure/deselect), ns: p50=\(samples[4999]) p95=\(samples[9499]) p99=\(samples[9899]) max=\(samples.last!), n=\(samples.count), checksum=\(checksum)")
    }
}
