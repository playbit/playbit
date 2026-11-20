import Foundation
import FileProvider
import PushKit
import Darwin

/// One lightweight owner at a time in each extension process. iOS can run
/// different accounts in different processes: each account has its own profile.
/// The profile lock rejects overlapping owners of the same account.
/// No app database or persistent app coordinator is required.
final class ProviderRuntime: NSObject {
    let account: ProviderAccount
    let ledger: ProviderLedger
    private let lock = NSLock()
    private var active = true
    private var snapshotPending = false
    private var snapshotDirty = false
    private var flushPending = false
    private var flushDirty = false
    private var pendingRows: [ProviderVolume]?
    private var pendingChanged = Set<String>()
    private var pendingNamespace: Data?
    private var pendingConnectionResolved = false
    private var registrationDirty = false
    private let jobs = DispatchGroup()
    private let closed = DispatchGroup()
    private var requests = 0
    private var activityWaiting = false
    private var idleRetirementPending = false
    private var deliveryPending = false
    private let local = OperationQueue()
    private let files = OperationQueue()
    private let network = DispatchQueue(label: "app.playbit.provider.network", qos: .utility)
    private var handle: OpaquePointer?
    private var referenceDirectory: URL?
    private var source: DispatchSourceRead?
    private var kick: DispatchSourceUserDataAdd?
    private var deadline: DispatchSourceTimer?
    private var delivery: DispatchSourceUserDataAdd?
    private var retryAt: UInt64 = 0
    private var connecting = true
    private var failures = 0
    private var revision: Any?
    private var lastNamespace: Data?
    private var notificationCheckpoint: URL?
    private var confirmedRetired = Set<String>()
    private var registered: String?
    private var pushCatalog = ""
    private var connectionGeneration: UInt64 = 0

    init(account: ProviderAccount, ledger: ProviderLedger) throws {
        self.account = account; self.ledger = ledger
        super.init()
        closed.enter()
        local.name = "app.playbit.provider.local"
        local.maxConcurrentOperationCount = 2
        local.qualityOfService = .userInitiated
        files.name = "app.playbit.provider.files"
        files.maxConcurrentOperationCount = 2
        files.qualityOfService = .userInitiated
        // A persistent database/profile lock in an App Group prevents safe
        // suspension (0xdead10cc). No other app needs this replica: keep it in
        // the extension's private container and share only the short ledger.
        let support = try FileManager.default.url(for: .applicationSupportDirectory,
            in: .userDomainMask, appropriateFor: nil, create: true)
        let path = support.appendingPathComponent("Playbit Provider", isDirectory: true)
            .appendingPathComponent(account.account, isDirectory: true)
            .appendingPathComponent("replica", isDirectory: true)
        try FileManager.default.createDirectory(at: path, withIntermediateDirectories: true,
            attributes: [.protectionKey: FileProtectionType.completeUntilFirstUserAuthentication])
        var descriptor: Int32 = -1
        var baseline: UnsafeMutablePointer<CChar>?
        try Self.check(path.path.withCString { pb_provider_open($0, &handle, &descriptor, &baseline) })
        defer { if let baseline { pb_provider_response_free(baseline) } }
        do {
            guard descriptor >= 0, handle != nil, let baseline, strlen(baseline) <= 1024 else {
                throw CocoaError(.fileReadCorruptFile)
            }
            guard let value = try JSONSerialization.jsonObject(with: Data(bytes: baseline, count: strlen(baseline))) as? [String: Any],
                  let anchor = value["anchor"], let revision = value["revision"] else { throw CocoaError(.fileReadCorruptFile) }
            self.revision = revision
            _ = try Self.namespaceStamp(anchor)
            let checkpoint = path.appendingPathComponent("provider-notified.json")
            notificationCheckpoint = checkpoint
            do {
                let size = try checkpoint.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? 0
                if size <= 512 {
                    let bytes = try Data(contentsOf: checkpoint)
                    let value = try JSONSerialization.jsonObject(with: bytes, options: [.fragmentsAllowed])
                    lastNamespace = try Self.namespaceStamp(value)
                }
            } catch {
                // This non-authority cache can be missing, evicted or unreadable.
                // Reconcile conservatively; it must never prevent admission.
                lastNamespace = nil
            }
        } catch {
            if descriptor >= 0 { Darwin.close(descriptor) }
            if let handle { pb_provider_stop(handle); pb_provider_close(handle); self.handle = nil }
            throw error
        }
        pendingRows = account.volumes
        pushCatalog = account.volumes.map(\.id).sorted().joined(separator: ":")
        referenceDirectory = path.appendingPathComponent("provider-references", isDirectory: true)
        let source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: network)
        source.setEventHandler { [weak self] in self?.networkTurn() }
        source.setCancelHandler { Darwin.close(descriptor) }
        self.source = source
        let deadline = DispatchSource.makeTimerSource(queue: network)
        deadline.setEventHandler { [weak self] in self?.networkTurn() }
        deadline.schedule(deadline: .distantFuture)
        self.deadline = deadline
        let kick = DispatchSource.makeUserDataAddSource(queue: network)
        kick.setEventHandler { [weak self] in self?.networkTurn() }
        self.kick = kick
        source.resume(); deadline.resume(); kick.resume()
        ProviderPush.start()
        let delivery = DispatchSource.makeUserDataAddSource(queue: .main)
        delivery.setEventHandler { [weak self] in self?.deliver() }
        self.delivery = delivery
        delivery.resume()
        // Reserve initialization before the first local reply can release its
        // activity. Complete one ordinary bootstrap even if that reply is fast.
        _ = begin()
        network.async {
            defer { self.jobs.leave() }
            self.networkTurn()
        }
    }
    var isActive: Bool { lock.lock(); defer { lock.unlock() }; return active }
    private func begin() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard active else { return false }
        jobs.enter()
        return true
    }
    static func check(_ error: UnsafeMutablePointer<CChar>?) throws {
        if let error {
            defer { pb_provider_response_free(error) }
            throw NSError(domain: "PlaybitProvider", code: 1,
                userInfo: [NSLocalizedDescriptionKey: String(cString: error)])
        }
    }
    func request(_ fields: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) {
        guard begin() else {
            completion(.failure(CocoaError(.userCancelled))); return
        }
        let fileWork = fields["op"] as? String == "fetch" || fields["path"] is String
        (fileWork ? files : local).addOperation {
            do {
                let copy = try self.copySource(fields)
                var updated = fields
                if let copy { updated["path"] = copy.path.path }
                let fields = updated
                let finish: (Result<[String: Any], Error>) -> Void = { result in
                    if let copy {
                        do { try FileManager.default.removeItem(at: copy.path) }
                        catch { ProviderRuntimes.retire(self) }
                    }
                    self.finishRequest(fields, result: result, completion: completion)
                }
                if let copy {
                    self.network.async {
                        do { try self.prepareCopy(copy, destination: fields["volume"] as? String) }
                        catch { ProviderRuntimes.retire(self); finish(.failure(error)); return }
                        self.files.addOperation { finish(Result { try self.execute(fields, source: copy.volume) }) }
                    }
                } else {
                    let result = Result { try self.execute(fields, source: copy?.volume) }
                    if case .failure(let error) = result,
                       error.localizedDescription == "file content is unavailable offline"
                        || (error as NSError).domain == NSCocoaErrorDomain && (error as NSError).code == CocoaError.userCancelled.rawValue {
                        // Cached hydration completes above without waiting for
                        // network startup. An uncached read can race the new
                        // owner's first connection: retry once after that
                        // existing network turn, retaining this request credit.
                        self.network.async {
                            do { try self.select(fields["volume"] as? String) }
                            catch { finish(.failure(error)); return }
                            self.files.addOperation { finish(Result { try self.execute(fields) }) }
                        }
                    } else { finish(result) }
                }
            } catch {
                ProviderRuntimes.retire(self)
                self.finishRequest(fields, result: .failure(error), completion: completion)
            }
        }
    }
    private func finishRequest(_ fields: [String: Any], result: Result<[String: Any], Error>,
        completion: @escaping (Result<[String: Any], Error>) -> Void) {
        defer { jobs.leave() }
        completion(result)
        if ["create", "modify", "delete", "create_volume", "rename_volume"].contains(fields["op"] as? String ?? "") {
            self.snapshot()
            self.flush()
        }
        self.kick?.add(data: 1)
    }
    private func flush() {
        lock.lock()
        guard active else { lock.unlock(); return }
        if flushPending { flushDirty = true; lock.unlock(); return }
        flushPending = true
        jobs.enter()
        lock.unlock()
        network.async {
            defer {
                self.lock.lock()
                self.flushPending = false
                let dirty = self.flushDirty
                self.flushDirty = false
                self.lock.unlock()
                if dirty { self.flush() }
                self.jobs.leave()
            }
            guard self.isActive, let handle = self.handle else { return }
            do { try Self.check(pb_provider_flush(handle)) }
            catch { NSLog("Playbit Files publication remains pending") }
            self.snapshot()
        }
    }
    private struct CopySource {
        let volume: String
        let entry: String
        let path: URL
    }
    private func copySource(_ fields: [String: Any]) throws -> CopySource? {
        guard fields["op"] as? String == "create", fields["directory"] as? Bool == false,
              (fields["name"] as? String)?.hasSuffix(".pbdb") == true, fields["path"] is String else { return nil }
        let bytes = try JSONSerialization.data(withJSONObject: fields)
        guard bytes.count <= 65536 else { throw CocoaError(.fileReadTooLarge) }
        let response = bytes.withUnsafeBytes {
            pb_provider_copy_source($0.bindMemory(to: UInt8.self).baseAddress, $0.count)
        }
        guard let response else { throw CocoaError(.fileReadCorruptFile) }
        defer { pb_provider_response_free(response) }
        let length = strlen(response)
        guard length <= 4096,
              let value = try JSONSerialization.jsonObject(with: Data(bytes: response, count: length)) as? [String: Any] else {
            throw CocoaError(.fileReadCorruptFile)
        }
        if let error = value["error"] as? String { throw Self.failure(error, operation: "fetch") }
        guard let volume = value["volume"] as? String, Identifier.valid(volume),
              let entry = value["entry"] as? String, Identifier.valid(entry, bytes: 15),
              let reference = value["reference"] as? String, reference.utf8.count <= 62 else {
            throw CocoaError(.fileReadCorruptFile)
        }
        // The system owns the original staging file. Snapshot the validated
        // reference so it cannot change its source between admission and commit.
        guard let referenceDirectory else { throw NSFileProviderError(.serverUnreachable) }
        let path = referenceDirectory.appendingPathComponent(UUID().uuidString)
        do { try Data(reference.utf8).write(to: path, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication]) }
        catch { try? FileManager.default.removeItem(at: path); throw error }
        return CopySource(volume: volume, entry: entry, path: path)
    }
    private func prepareCopy(_ copy: CopySource, destination: String?) throws {
        try ledger.authorize(account, volumes: [copy.volume], mutation: false) {
            guard let handle, let identity = Identifier.decode(account.account, bytes: 16),
                  let destination, let target = Identifier.decode(destination, bytes: 16),
                  let volume = Identifier.decode(copy.volume, bytes: 16),
                  let entry = Identifier.decode(copy.entry, bytes: 15) else { throw CocoaError(.fileReadCorruptFile) }
            try Self.check(identity.withUnsafeBufferPointer { identity in
                volume.withUnsafeBufferPointer { volume in
                    entry.withUnsafeBufferPointer { entry in
                        target.withUnsafeBufferPointer { target in
                            pb_provider_prepare_copy(handle, identity.baseAddress, target.baseAddress, volume.baseAddress, entry.baseAddress)
                        }
                    }
                }
            })
        }
    }
    private func select(_ volume: String?) throws {
        guard let volume else { return }
        guard let handle, let bytes = Identifier.decode(volume, bytes: 16) else { throw CocoaError(.fileReadCorruptFile) }
        try ledger.authorize(account, volumes: [volume], mutation: false) {
            try Self.check(bytes.withUnsafeBufferPointer { pb_provider_select(handle, $0.baseAddress, 1) })
        }
    }
    private func execute(_ fields: [String: Any], source: String? = nil) throws -> [String: Any] {
        guard let handle else { throw NSFileProviderError(.serverUnreachable) }
        let mutation = ["create", "modify", "delete", "create_volume", "rename_volume"].contains(fields["op"] as? String ?? "")
        var volumes = Set<String>()
        if let volume = fields["volume"] as? String { volumes.insert(volume) }
        if let source { volumes.insert(source) }
        if fields["op"] as? String == "create_volume" {
            guard let id = fields["id"] as? String, Identifier.valid(id) else { throw CocoaError(.fileReadCorruptFile) }
            // Historical App Group reservations survive checkpoint eviction.
            // Reserve before the native commit, then recheck this generation
            // under the ordinary mutation lease before changing the account.
            try ledger.exposeVolumes(account: account.account, volumes: [id], expected: account)
        }
        return try ledger.authorize(account, volumes: volumes, mutation: mutation) {
            var fields = fields
            fields["account"] = account.account
            let bytes = try JSONSerialization.data(withJSONObject: fields)
            guard bytes.count <= 65536 else { throw CocoaError(.fileReadTooLarge) }
            let response = bytes.withUnsafeBytes {
                pb_provider_request(handle, $0.bindMemory(to: UInt8.self).baseAddress, $0.count)
            }
            guard let response else { throw NSFileProviderError(.serverUnreachable) }
            defer { pb_provider_response_free(response) }
            let count = strlen(response)
            guard count <= 524288,
                  let value = try JSONSerialization.jsonObject(with: Data(bytes: response, count: count)) as? [String: Any] else {
                throw CocoaError(.fileReadCorruptFile)
            }
            if let message = value["error"] as? String {
                let operation = fields["op"] as? String ?? ""
                // The protected ledger authorized this account. Its independent
                // native profile may still be binding during the first local
                // request; other permission failures remain permanent.
                if value["code"] as? String == "permission_denied",
                   message.hasSuffix("File Provider account is no longer signed in") {
                    throw Self.failure(message, operation: operation)
                }
                switch value["code"] as? String {
                case "permission_denied": throw CocoaError(["list", "scan", "item", "fetch", "anchor", "root", "volumes", "catalog", "volume_item"].contains(operation) ? .fileReadNoPermission : .fileWriteNoPermission)
                case "invalid_input": throw ProviderClient.operationError(message, operation: operation)
                case "not_found": throw NSFileProviderError(.noSuchItem)
                default: throw Self.failure(message, operation: operation)
                }
            }
            let operation = fields["op"] as? String
            let exposureField = operation == "catalog" ? "items" : "volumes"
            if ["catalog", "volumes"].contains(operation ?? ""), let rows = value[exposureField] as? [[String: Any]] {
                let ids = rows.compactMap { $0["id"] as? String }
                guard ids.count == rows.count else { throw CocoaError(.fileReadCorruptFile) }
                try ledger.exposeVolumes(account: account.account, volumes: ids)
            }
            return value
        }
    }
    private static func failure(_ message: String, operation: String) -> Error {
        if message.contains("provider directory not empty") { return NSFileProviderError(.directoryNotEmpty) }
        if message.contains("version changed") {
            // iOS has no versionNoLongerAvailable code. A transient fetch retries
            // current metadata; mutations keep the user's conflicting local bytes.
            if operation == "fetch" { return CocoaError(.userCancelled) }
            return NSError(domain: NSCocoaErrorDomain, code: CocoaError.fileWriteFileExists.rawValue,
                userInfo: [NSLocalizedDescriptionKey: "This item changed in Playbit. Copy your local changes to a new file to resolve the conflict."])
        }
        // The ledger already authenticated this call. A new private replica
        // can still be binding that account; it is not a user sign-in failure.
        if message.contains("signed in") || message.contains("waiting for initial synchronization") {
            return CocoaError(.userCancelled)
        }
        if message.contains("not found") { return NSFileProviderError(.noSuchItem) }
        if message.contains("already exists") { return CocoaError(.fileWriteFileExists) }
        if message.contains("denied") {
            return CocoaError(["root", "volumes", "catalog", "volume_item", "list", "scan", "item", "fetch", "anchor"].contains(operation) ? .fileReadNoPermission : .fileWriteNoPermission)
        }
        // An individual read/commit can finish failing after a transport has
        // already reconnected. Keep that operation's diagnostic without
        // reinstating a domain-wide connectivity throttle for the new owner.
        return ProviderClient.operationError(message, operation: operation)
    }
    private func networkTurn() {
        guard begin() else { return }
        defer { jobs.leave() }
        if connecting && DispatchTime.now().uptimeNanoseconds < retryAt {
            // A failed connect can publish readiness before advance gets to
            // drain it. Consume that notification without advancing/retrying;
            // a readable socket otherwise dispatches continuously in backoff.
            guard let source else { ProviderRuntimes.retire(self); return }
            var bytes = [UInt8](repeating: 0, count: 256)
            for _ in 0..<64 {
                let count = Darwin.read(Int32(source.handle), &bytes, bytes.count)
                if count > 0 || count < 0 && errno == EINTR { continue }
                if count == 0 || errno != EAGAIN && errno != EWOULDBLOCK {
                    NSLog("Playbit Files readiness channel closed")
                    ProviderRuntimes.retire(self)
                }
                break
            }
            return
        }
        do {
            try ledger.authorize(account, mutation: false) {}
            guard let handle else { return }
            if connecting {
                guard let identity = Identifier.decode(account.account, bytes: 16) else { throw CocoaError(.fileReadCorruptFile) }
                try Self.check(identity.withUnsafeBufferPointer { id in
                    account.secret.withUnsafeBufferPointer { key in
                        account.email.withCString { pb_provider_connect(handle, id.baseAddress, $0, key.baseAddress) }
                    }
                })
                connecting = false
                failures = 0
            }
            var next = UInt64.max
            var connection: UInt64 = 0
            let error = pb_provider_advance(handle, &next, &connection)
            if next != UInt64.max {
                deadline?.schedule(deadline: .now() + .nanoseconds(Int(min(next, UInt64(Int.max)))))
            } else { deadline?.schedule(deadline: .distantFuture) }
            let connectionChanged = connection != 0 && connection != connectionGeneration
            if connectionChanged {
                lock.lock()
                registrationDirty = true
                pendingConnectionResolved = true
                lock.unlock()
                // Account authentication is established even if advance reports
                // an unrelated volume error. Drain all retained intents on this
                // event without waiting for another Files mutation or a timer.
                flush()
            }
            connectionGeneration = connection
            try Self.check(error)
            try registerToken()
        } catch {
            // Retry failed initialization only, with bounded backoff. Healthy
            // replicas receive AP pub/sub; this deadline never refreshes data.
            if connecting, failures < 3 {
                failures += 1
                retryAt = DispatchTime.now().uptimeNanoseconds + UInt64(1 << failures) * 1_000_000_000
                deadline?.schedule(deadline: DispatchTime(uptimeNanoseconds: retryAt))
            }
            if connecting && failures >= 3 { retryAt = DispatchTime.now().uptimeNanoseconds + 30_000_000_000 }
            NSLog("Playbit Files connection remains unavailable")
        }
        snapshot()
    }
    func snapshot() {
        lock.lock()
        guard active else { lock.unlock(); return }
        if snapshotPending { snapshotDirty = true; lock.unlock(); return }
        snapshotPending = true
        jobs.enter()
        lock.unlock()
        local.addOperation {
            defer {
                self.lock.lock()
                self.snapshotPending = false
                let dirty = self.snapshotDirty
                self.snapshotDirty = false
                self.lock.unlock()
                if dirty { self.snapshot() }
                self.jobs.leave()
            }
            do {
                var request: [String: Any] = ["op": "volumes"]
                let known = try self.ledger.knownVolumes(account: self.account.account)
                request["known"] = known
                request["after"] = self.revision
                let value = try self.execute(request)
                guard let retired = value["retired"] as? [String], retired.count <= 256 else {
                    throw CocoaError(.fileReadCorruptFile)
                }
                let unconfirmed = retired.filter { !self.confirmedRetired.contains($0) }
                if !unconfirmed.isEmpty {
                    try self.ledger.retire(self.account, volumes: unconfirmed)
                    self.confirmedRetired.formUnion(unconfirmed)
                }
                // Reconnect adoption can emit conservative delivery notices
                // without changing an item. Compare the domain's persistent
                // namespace, captured before connect, so reopening an owner
                // cannot signal Files into an endless reopen/bootstrap loop.
                guard let anchor = value["anchor"] else { throw CocoaError(.fileReadCorruptFile) }
                let stamp = try Self.namespaceStamp(anchor)
                if !(anchor is NSNull), stamp != self.lastNamespace {
                    self.lastNamespace = stamp
                    self.lock.lock(); self.pendingChanged.insert(self.account.account); self.pendingNamespace = stamp; self.lock.unlock()
                }
                guard let volumes = value["volumes"] as? [[String: Any]] else {
                    self.revision = value["revision"]
                    self.scheduleDelivery()
                    return
                }
                let rows = volumes.compactMap { row -> ProviderVolume? in
                    guard let id = row["id"] as? String, let name = row["name"] as? String else { return nil }
                    return ProviderVolume(id: id, name: name)
                }
                guard rows.count == volumes.count, rows.count <= 256 else { throw CocoaError(.fileReadCorruptFile) }
                let catalog = rows.map { $0.id + ":" + $0.name }.sorted().joined(separator: "\n")
                self.lock.lock()
                let catalogChanged = self.pushCatalog != catalog
                self.pushCatalog = catalog
                self.lock.unlock()
                if catalogChanged { self.kick?.add(data: 1) }
                self.lock.lock()
                self.pendingRows = rows
                self.lock.unlock()
                self.revision = value["revision"]
                self.scheduleDelivery()
            } catch {
                // Retain the previous fence so the next event retries the cut.
                NSLog("Playbit Files change enumeration remains pending")
            }
        }
    }
    private static func namespaceStamp(_ anchor: Any) throws -> Data {
        let stamp = try JSONSerialization.data(withJSONObject: anchor, options: [.sortedKeys, .fragmentsAllowed])
        guard stamp.count <= 512 else { throw CocoaError(.fileReadCorruptFile) }
        return stamp
    }
    private func scheduleDelivery() {
        lock.lock()
        guard active else { lock.unlock(); return }
        if !deliveryPending { deliveryPending = true; jobs.enter() }
        lock.unlock()
        delivery?.add(data: 1)
    }
    private func deliver() {
        lock.lock()
        guard deliveryPending else { lock.unlock(); return }
        deliveryPending = false
        let rows = pendingRows
        let changed = pendingChanged
        let stamp = pendingNamespace
        pendingNamespace = nil
        let connectionResolved = pendingConnectionResolved
        pendingConnectionResolved = false
        pendingRows = nil; pendingChanged.removeAll(keepingCapacity: true)
        let live = active
        lock.unlock()
        defer { jobs.leave() }
        guard live else { return }
        guard let rows else {
            if connectionResolved { ProviderDomains.shared.connectionResolved(account: account.account, generation: account.generation) }
            signal(changed, stamp: stamp)
            ProviderDomains.shared.retryPending(); return
        }
        ProviderDomains.shared.reconcile(ledger: ledger, account: account, volumes: rows)
        if connectionResolved { ProviderDomains.shared.connectionResolved(account: account.account, generation: account.generation) }
        signal(changed, stamp: stamp)
    }
    private func signal(_ changed: Set<String>, stamp: Data?) {
        guard changed.contains(account.account), let stamp, let checkpoint = notificationCheckpoint else { return }
        // This callback retains only a tiny cache value/path, never the replica
        // or its activity. Writing an older acknowledged root is conservative:
        // the next owner detects a mismatch and signals the current root again.
        ProviderDomains.shared.signal(account: account.account, generation: account.generation) {
            do { try stamp.write(to: checkpoint, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication]) }
            catch { NSLog("Playbit Files notification checkpoint remains pending") }
        }
    }
    func tokenChanged() { kick?.add(data: 1) }
    private func registerToken() throws {
        guard let token = ProviderPush.shared.token, !connecting, connectionGeneration != 0 else { return }
        let bundle = Bundle.main.object(forInfoDictionaryKey: "PlaybitContainingAppIdentifier") as? String ?? ""
        lock.lock()
        let catalog = pushCatalog
        let dirty = registrationDirty
        registrationDirty = false
        lock.unlock()
        if dirty { registered = nil }
        let signature = token.bytes.base64EncodedString() + ":" + String(token.invalidated) + ":" + catalog
        guard registered != signature else { return }
        _ = try execute(["op": "push_token", "token": token.bytes.map { String(format: "%02x", $0) }.joined(), "invalidated": token.invalidated,
            "bundle": bundle, "sandbox": Bundle.main.object(forInfoDictionaryKey: "PlaybitPushEnvironment") as? String == "development"])
        registered = signature
    }
    func retainRequest() throws {
        lock.lock(); defer { lock.unlock() }
        guard active else { throw CocoaError(.userCancelled) }
        requests += 1
    }
    func releaseRequest(activityFinished: @escaping () -> Void) {
        lock.lock()
        precondition(requests > 0)
        requests -= 1
        let idle = requests == 0
        // One completed call keeps the activity alive through bootstrap/drain.
        // Other completed calls return their admission immediately; do not pin
        // all seven credits behind a slow network operation.
        let keepActivity = idle && !activityWaiting
        if keepActivity { activityWaiting = true }
        let scheduleRetirement = idle && !idleRetirementPending
        if scheduleRetirement { idleRetirementPending = true }
        lock.unlock()
        if keepActivity { whenClosed(activityFinished) }
        else { activityFinished() }
        if scheduleRetirement {
            jobs.notify(queue: ProviderRuntimes.retirement) {
                self.lock.lock(); self.idleRetirementPending = false; self.lock.unlock()
                ProviderRuntimes.retire(self, ifUnused: true)
            }
        }
    }
    var hasRequests: Bool { lock.lock(); defer { lock.unlock() }; return requests != 0 }
    func whenClosed(_ completion: @escaping () -> Void) {
        closed.notify(queue: ProviderRuntimes.retirement, execute: completion)
    }
    func retire(completion: @escaping () -> Void) {
        whenClosed(completion)
        lock.lock()
        guard active else { lock.unlock(); return }
        active = false
        let cancelDelivery = deliveryPending
        deliveryPending = false
        lock.unlock()
        if cancelDelivery { jobs.leave() }
        if let handle { pb_provider_stop(handle) }
        source?.cancel(); kick?.cancel(); deadline?.cancel(); delivery?.cancel()
        jobs.notify(queue: ProviderRuntimes.retirement) {
            if let handle = self.handle { pb_provider_close(handle); self.handle = nil }
            self.closed.leave()
        }
    }

}

/// Account turnover is serialized, but never waits on app UI or a remote ack.
/// One retiring owner and one pending retry bound transition state.
enum ProviderRuntimes {
    static let retirement = DispatchQueue(label: "app.playbit.provider.retirement")
    private static let lock = NSLock()
    private static var current: ProviderRuntime?
    private static var retiring = false
    private static var handoffPending = false
    private static var changePending = false
    private static let observe: Void = {
        CFNotificationCenterAddObserver(CFNotificationCenterGetDarwinNotifyCenter(), nil,
            { _, _, _, _, _ in ProviderRuntimes.changed() }, ProviderLedger.changed as CFString, nil, .deliverImmediately)
        CFNotificationCenterAddObserver(CFNotificationCenterGetDarwinNotifyCenter(), nil,
            { _, _, _, _, _ in
                lock.lock(); let runtime = current; lock.unlock()
                runtime?.snapshot()
            }, ProviderLedger.publicationNeeded as CFString, nil, .deliverImmediately)
    }()
    static func acquire(_ account: ProviderAccount, ledger: ProviderLedger) throws -> ProviderRuntime {
        _ = observe
        lock.lock(); defer { lock.unlock() }
        if let current, current.account.account == account.account,
           current.account.generation == account.generation {
            try current.retainRequest()
            return current
        }
        guard current == nil, !retiring else {
            if !handoffPending {
                handoffPending = true
                retirement.async {
                    lock.lock()
                    handoffPending = false
                    let previous = current
                    lock.unlock()
                    if let previous { retire(previous, ifUnused: true) }
                }
            }
            throw CocoaError(.userCancelled)
        }
        let runtime = try ProviderRuntime(account: account, ledger: ledger)
        try runtime.retainRequest()
        current = runtime
        return runtime
    }
    static func retire(_ runtime: ProviderRuntime, ifUnused: Bool = false) {
        lock.lock()
        guard current === runtime, !ifUnused || !runtime.hasRequests else { lock.unlock(); return }
        current = nil; retiring = true
        lock.unlock()
        runtime.retire {
            lock.lock(); retiring = false; lock.unlock()
        }
    }
    static func changed() {
        lock.lock()
        guard !changePending else { lock.unlock(); return }
        changePending = true
        lock.unlock()
        retirement.async {
            lock.lock(); changePending = false; let runtime = current; lock.unlock()
            guard let runtime else { return }
            do {
                let active = try runtime.ledger.withLock(exclusive: false) { try runtime.ledger.read() }
                if active?.account == runtime.account.account, active?.generation == runtime.account.generation { return }
            } catch { /* Locked/malformed state revokes admission until a later request. */ }
            retire(runtime)
        }
    }
    static func tokenChanged() {
        lock.lock(); let runtime = current; lock.unlock()
        runtime?.tokenChanged()
    }
}

/// PushKit registration belongs to the extension process, independently of a
/// short replica activity. It holds no profile, store, credentials or raw handle.
/// The next admitted owner publishes the latest app/device token if no owner is
/// currently running; iOS controls when this process receives execution time.
final class ProviderPush: NSObject, PKPushRegistryDelegate {
    static let shared = ProviderPush()
    private static let startup: Void = { DispatchQueue.main.async { shared.begin() } }()
    private let lock = NSLock()
    private var current: (bytes: Data, invalidated: Bool)?
    private var registry: PKPushRegistry?
    static func start() { _ = startup }
    var token: (bytes: Data, invalidated: Bool)? { lock.lock(); defer { lock.unlock() }; return current }
    private func begin() {
        let registry = PKPushRegistry(queue: .main)
        registry.delegate = self
        self.registry = registry
        registry.desiredPushTypes = [.fileProvider]
        if let token = registry.pushToken(for: .fileProvider), !token.isEmpty, token.count <= 128 {
            lock.lock(); current = (token, false); lock.unlock()
            ProviderRuntimes.tokenChanged()
        }
    }
    func pushRegistry(_ registry: PKPushRegistry, didUpdate credentials: PKPushCredentials, for type: PKPushType) {
        guard type == .fileProvider, !credentials.token.isEmpty, credentials.token.count <= 128 else {
            NSLog("Playbit Files received an invalid push token"); return
        }
        lock.lock(); current = (credentials.token, false); lock.unlock()
        ProviderRuntimes.tokenChanged()
    }
    func pushRegistry(_ registry: PKPushRegistry, didInvalidatePushTokenFor type: PKPushType) {
        guard type == .fileProvider else { return }
        lock.lock()
        guard let token = current else { lock.unlock(); return }
        current = (token.bytes, true)
        lock.unlock()
        ProviderRuntimes.tokenChanged()
    }
}
