import Foundation
import FileProvider
import Security
import Darwin

struct ProviderVolume: Codable, Equatable {
    let id: String
    let name: String
}

private struct ProviderDomainReservation: Codable, Equatable {
    let id: String
    var retired: Bool
}

/// This is private credential storage in the company App Group. Neither the
/// ledger nor decoding failures are logged or copied into provider metadata.
struct ProviderAccount: Codable, Equatable {
    let account: String
    let email: String
    let secret: [UInt8]
    var volumes: [ProviderVolume]
    var retired: [String]
    var generation: String

    func validate() throws {
        guard Identifier.valid(account), Identifier.valid(generation),
              email.utf8.count <= 254, email.contains("@"), email == email.lowercased(),
              secret.count == 16, volumes.count <= 256,
              Set(volumes.map(\.id)).count == volumes.count,
              retired.count <= 256, Set(retired).count == retired.count,
              retired.allSatisfy({ Identifier.valid($0) }),
              Set(volumes.map(\.id)).isDisjoint(with: retired),
              volumes.allSatisfy({ Identifier.valid($0.id) && !$0.name.isEmpty && $0.name.utf8.count <= 256 }) else {
            throw CocoaError(.fileReadCorruptFile)
        }
    }
}

final class ProviderLedger {
    // 256 names of 256 UTF-8 bytes can each expand sixfold in JSON.
    static let accountBytesMax = 512 * 1024
    static let changed = "app.playbit.provider-account-changed"
    static let publicationNeeded = "app.playbit.provider-publication-needed"
    let root: URL
    init(root: URL) { self.root = root }
    static func shared() throws -> ProviderLedger {
        guard let group = Bundle.main.object(forInfoDictionaryKey: "PlaybitProviderGroup") as? String,
              let root = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: group) else {
            throw NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.notAuthenticated.rawValue,
                userInfo: [NSLocalizedDescriptionKey: "Open Playbit to enable Files access."])
        }
        return ProviderLedger(root: root.appendingPathComponent("Library/Application Support/FileProvider", isDirectory: true))
    }

    func withLock<T>(exclusive: Bool, _ body: () throws -> T) throws -> T {
        // Domain reconciliation also reads this short ledger after an async
        // manager callback. Protect that bounded critical section independently
        // of the native replica's expiring activity.
        let activity = ProcessInfo.processInfo.beginActivity(options: .userInitiated,
            reason: "Access Playbit Files account")
        defer { ProcessInfo.processInfo.endActivity(activity) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        let descriptor = Darwin.open(root.appendingPathComponent("account.lock").path,
            O_RDWR | O_CREAT | O_NOFOLLOW | O_CLOEXEC, 0o600)
        guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        defer { Darwin.close(descriptor) }
        guard flock(descriptor, (exclusive ? LOCK_EX : LOCK_SH) | LOCK_NB) == 0 else {
            if errno == EWOULDBLOCK { throw CocoaError(.userCancelled) }
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        defer { flock(descriptor, LOCK_UN) }
        return try body()
    }

    // Call only while holding the ledger lock. Absence and a signed-out marker
    // mean no account; malformed/locked state fails explicitly.
    func read() throws -> ProviderAccount? {
        let url = root.appendingPathComponent("account.json")
        guard FileManager.default.fileExists(atPath: url.path) else { return nil }
        let size = try url.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard size.isRegularFile == true, let count = size.fileSize, count <= Self.accountBytesMax else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let bytes = try Data(contentsOf: url)
        if bytes == Data("null".utf8) { return nil }
        var value = try JSONDecoder().decode(ProviderAccount.self, from: bytes)
        try value.validate()
        value.retired = try retiredVolumes(account: value.account)
        value.volumes.removeAll { value.retired.contains($0.id) }
        return value
    }

    @discardableResult
    func publish(_ snapshot: Data?) throws -> ProviderAccount? {
        let account = try withLock(exclusive: true) { () -> ProviderAccount? in
            let old = try read()
            if FileManager.default.fileExists(atPath: root.appendingPathComponent("domains.json").path) {
                try synchronize(name: "domains.json")
            }
            var next: ProviderAccount?
            if let snapshot {
                guard snapshot.count <= Self.accountBytesMax,
                      var fields = try JSONSerialization.jsonObject(with: snapshot) as? [String: Any] else {
                    throw CocoaError(.fileReadCorruptFile)
                }
                if !fields.isEmpty {
                    var identity = [UInt8](repeating: 0, count: 16)
                    guard SecRandomCopyBytes(kSecRandomDefault, identity.count, &identity) == errSecSuccess else {
                        throw CocoaError(.fileWriteUnknown)
                    }
                    fields["generation"] = Identifier.encode(identity)
                    let bytes = try JSONSerialization.data(withJSONObject: fields)
                    var decoded = try JSONDecoder().decode(ProviderAccount.self, from: bytes)
                    try decoded.validate()
                    try recordRetired(account: decoded.account, volumes: decoded.retired)
                    decoded.retired = try retiredVolumes(account: decoded.account)
                    decoded.volumes.removeAll { decoded.retired.contains($0.id) }
                    if let old, old.account == decoded.account, old.email == decoded.email, old.secret == decoded.secret {
                        decoded.generation = old.generation
                    }
                    try decoded.validate()
                    next = decoded
                }
            }
            if next == old {
                if FileManager.default.fileExists(atPath: root.appendingPathComponent("account.json").path) {
                    try synchronize(name: "account.json")
                } else { try store(Data("null".utf8), name: "account.json") }
                return next
            }
            let bytes = try next.map { try JSONEncoder().encode($0) } ?? Data("null".utf8)
            guard bytes.count <= Self.accountBytesMax else { throw CocoaError(.fileWriteOutOfSpace) }
            try store(bytes, name: "account.json")
            return next
        }
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(Self.changed as CFString), nil, nil, true)
        return account
    }

    private func store(_ bytes: Data, name: String) throws {
        guard bytes.count <= Self.accountBytesMax else { throw CocoaError(.fileWriteOutOfSpace) }
        let url = root.appendingPathComponent(name)
        try bytes.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
        try synchronize(name: name)
    }
    private func synchronize(name: String) throws {
        let url = root.appendingPathComponent(name)
        let file = try FileHandle(forWritingTo: url)
        defer { try? file.close() }
        try file.synchronize()
        let directory = Darwin.open(root.path, O_RDONLY | O_DIRECTORY | O_CLOEXEC)
        guard directory >= 0 else { throw POSIXError(.EIO) }
        defer { Darwin.close(directory) }
        guard fsync(directory) == 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
    }

    func retire(_ expected: ProviderAccount, volumes: [String]) throws {
        guard !volumes.isEmpty else { return }
        try withLock(exclusive: true) {
            guard var active = try read(), active.account == expected.account,
                  active.generation == expected.generation else { throw NSFileProviderError(.notAuthenticated) }
            try recordRetired(account: expected.account, volumes: volumes)
            active.retired = try retiredVolumes(account: expected.account)
            active.volumes.removeAll { active.retired.contains($0.id) }
            try active.validate()
            try store(JSONEncoder().encode(active), name: "account.json")
        }
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(Self.changed as CFString), nil, nil, true)
        CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
            CFNotificationName(Self.publicationNeeded as CFString), nil, nil, true)
    }

    /// Reserve identities durably under the cross-process ledger lock before
    /// asynchronous OS registration. Reservations survive process termination
    /// and account turnover, including an OS add that outlives its caller.
    func reserveDomains(_ domains: Set<String>) throws {
        try withLock(exclusive: true) {
            let name = "locations.json"
            let path = root.appendingPathComponent(name)
            var reserved: [String] = []
            if FileManager.default.fileExists(atPath: path.path) {
                let size = try path.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
                guard size.isRegularFile == true, (size.fileSize ?? 65537) <= 65536 else { throw CocoaError(.fileReadCorruptFile) }
                reserved = try JSONDecoder().decode([String].self, from: Data(contentsOf: path))
                guard reserved.count <= 256, Set(reserved).count == reserved.count,
                      reserved.allSatisfy({ Identifier.valid($0) }) else { throw CocoaError(.fileReadCorruptFile) }
            }
            let next = Set(reserved).union(domains)
            guard next.count <= 256, next.allSatisfy({ Identifier.valid($0) }) else { throw CocoaError(.fileWriteOutOfSpace) }
            try store(JSONEncoder().encode(next.sorted()), name: name)
        }
    }

    /// Volume exposure survives sign-out and account turnover independently of
    /// account locations. OS-held edits cannot be proven absent or discarded.
    func exposeVolumes(account: String, volumes: [String], expected: ProviderAccount? = nil) throws {
        try withLock(exclusive: true) {
            guard Identifier.valid(account), volumes.count <= 256,
                  volumes.allSatisfy({ Identifier.valid($0) }) else { throw CocoaError(.fileReadCorruptFile) }
            if let expected {
                guard expected.account == account, let active = try read(), active.account == account else {
                    throw NSFileProviderError(.notAuthenticated)
                }
                guard active.generation == expected.generation else { throw CocoaError(.userCancelled) }
                guard Set(volumes).isDisjoint(with: active.retired) else {
                    throw NSError(domain: NSCocoaErrorDomain, code: CocoaError.fileWriteNoPermission.rawValue,
                        userInfo: [NSLocalizedDescriptionKey: "This Playbit volume is unavailable. Keep a copy of any local changes."])
                }
            }
            var reserved = try readDomains()
            let existing = Set(reserved.map(\.id))
            let additions = Set(volumes.map { account + "-" + $0 }).subtracting(existing)
            guard reserved.count + additions.count <= 256 else { throw CocoaError(.fileWriteOutOfSpace) }
            reserved += additions.map { ProviderDomainReservation(id: $0, retired: false) }
            try store(JSONEncoder().encode(reserved.sorted { $0.id < $1.id }), name: "domains.json")
        }
    }

    private static func validDomain(_ domain: String) -> Bool {
        let parts = domain.split(separator: "-", omittingEmptySubsequences: false)
        return parts.count == 2 && parts.allSatisfy { Identifier.valid(String($0)) }
    }
    private func readDomains() throws -> [ProviderDomainReservation] {
        let path = root.appendingPathComponent("domains.json")
        guard FileManager.default.fileExists(atPath: path.path) else { return [] }
        let size = try path.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard size.isRegularFile == true, let count = size.fileSize, count <= 65536 else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let domains = try JSONDecoder().decode([ProviderDomainReservation].self, from: Data(contentsOf: path))
        guard domains.count <= 256, Set(domains.map(\.id)).count == domains.count,
              domains.allSatisfy({ Self.validDomain($0.id) }) else { throw CocoaError(.fileReadCorruptFile) }
        return domains
    }
    private func retiredVolumes(account: String) throws -> [String] {
        try readDomains().filter { $0.retired && $0.id.hasPrefix(account + "-") }
            .map { String($0.id.split(separator: "-")[1]) }.sorted()
    }
    private func recordRetired(account: String, volumes: [String]) throws {
        var domains = try readDomains()
        for volume in Set(volumes) {
            guard Identifier.valid(volume) else { throw CocoaError(.fileReadCorruptFile) }
            let id = account + "-" + volume
            if let index = domains.firstIndex(where: { $0.id == id }) { domains[index].retired = true }
            else {
                guard domains.count < 256 else { throw CocoaError(.fileWriteOutOfSpace) }
                domains.append(ProviderDomainReservation(id: id, retired: true))
            }
        }
        // Confirm earlier writes too: a retry may observe an atomic replacement
        // whose original file/directory synchronization failed.
        try store(JSONEncoder().encode(domains.sorted { $0.id < $1.id }), name: "domains.json")
    }
    func knownVolumes(account: String? = nil) throws -> [String] {
        try withLock(exclusive: false) {
            Set(try readDomains().filter { row in account.map { row.id.hasPrefix($0 + "-") } ?? true }
                .map { String($0.id.split(separator: "-")[1]) }).sorted()
        }
    }

    func authorize<T>(_ expected: ProviderAccount, volumes: Set<String> = [], mutation: Bool, _ body: () throws -> T) throws -> T {
        if mutation {
            // Wake a containing app whose pending account publication was
            // excluded by this lease. This is a commit/admission event, not a
            // freshness timer, and fires after withLock releases the descriptor.
            defer {
                CFNotificationCenterPostNotification(CFNotificationCenterGetDarwinNotifyCenter(),
                    CFNotificationName(Self.publicationNeeded as CFString), nil, nil, true)
            }
            return try withLock(exclusive: false) {
                guard let active = try read(), active.account == expected.account else { throw NSFileProviderError(.notAuthenticated) }
                guard active.generation == expected.generation else { throw CocoaError(.userCancelled) }
                if !volumes.isDisjoint(with: active.retired) {
                    throw NSError(domain: NSCocoaErrorDomain, code: mutation ? CocoaError.fileWriteNoPermission.rawValue : CocoaError.fileReadNoPermission.rawValue,
                        userInfo: [NSLocalizedDescriptionKey: "This Playbit volume is unavailable. Keep a copy of any local changes."])
                }
                return try body()
            }
        }
        try withLock(exclusive: false) {
            guard let active = try read(), active.account == expected.account else { throw NSFileProviderError(.notAuthenticated) }
            guard active.generation == expected.generation else { throw CocoaError(.userCancelled) }
            if !volumes.isDisjoint(with: active.retired) {
                    throw NSError(domain: NSCocoaErrorDomain, code: mutation ? CocoaError.fileWriteNoPermission.rawValue : CocoaError.fileReadNoPermission.rawValue,
                        userInfo: [NSLocalizedDescriptionKey: "This Playbit volume is unavailable. Keep a copy of any local changes."])
                }
        }
        // Hydration and AP registration never keep account transitions waiting
        // on a remote response. Admitted reads may finish during domain removal.
        return try body()
    }
}
