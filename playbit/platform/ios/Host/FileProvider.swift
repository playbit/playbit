import Foundation
import FileProvider

/// Called on EngineHost's existing serialized worker, only after a catalog or
/// account change. The native snapshot skips backing edits without disk reads.
enum ProviderApplication {
    private static let retry: DispatchSourceUserDataAdd = {
        let source = DispatchSource.makeUserDataAddSource(queue: .main)
        source.setEventHandler { ProviderDomains.shared.retryPending() }
        source.resume()
        return source
    }()
    static func retryPending() { retry.add(data: 1) }
    static func update(_ handle: OpaquePointer) throws {
        let ledger = try ProviderLedger.shared()
        let known = try JSONSerialization.data(withJSONObject: ledger.knownVolumes())
        var snapshot: UnsafeMutablePointer<CChar>?
        let error = known.withUnsafeBytes {
            pb_ios_provider_state(handle, $0.bindMemory(to: UInt8.self).baseAddress, $0.count, &snapshot)
        }
        if let error {
            defer { pb_ios_string_free(error) }
            throw NSError(domain: "Playbit", code: 1,
                userInfo: [NSLocalizedDescriptionKey: String(cString: error)])
        }
        guard let snapshot else { return }
        defer { pb_ios_string_free(snapshot) }
        let length = strlen(snapshot)
        guard length <= ProviderLedger.accountBytesMax else { throw CocoaError(.fileReadTooLarge) }
        let account = try ledger.publish(Data(bytes: snapshot, count: length))
        if let error = pb_ios_provider_state_committed(handle) {
            defer { pb_ios_string_free(error) }
            throw NSError(domain: "Playbit", code: 1,
                userInfo: [NSLocalizedDescriptionKey: String(cString: error)])
        }
        DispatchQueue.main.async {
            ProviderDomains.shared.reconcile(ledger: ledger, account: account, volumes: account?.volumes ?? [])
        }
    }
    static func disable() throws {
        let ledger = try ProviderLedger.shared()
        try ledger.publish(nil)
        DispatchQueue.main.async {
            ProviderDomains.shared.reconcile(ledger: ledger, account: nil, volumes: [])
        }
    }
}
