import Foundation
import FileProvider
import Darwin

/// Exercise the actual item adapter and checkpoint implementation with a local
/// transport fixture. This does not register a domain or exercise Finder UI.
final class ProviderClient {
    let handler: ([String: Any]) throws -> [String: Any]
    init(_ handler: @escaping ([String: Any]) throws -> [String: Any]) { self.handler = handler }
    func call(_ fields: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) {
        completion(Result { try handler(fields) })
    }
}

private func identity(_ byte: UInt8, width: Int = 16) -> String {
    Identifier.encode([UInt8](repeating: byte, count: width))
}

private func entryIdentity(_ volume: UInt8, _ entry: UInt8) -> String {
    Identifier.encode([UInt8](repeating: volume, count: 16) + [UInt8](repeating: entry, count: 15))
}

private func volumeItem(_ byte: UInt8) -> [String: Any] {
    ["id": identity(byte), "parent": NSFileProviderItemIdentifier.rootContainer.rawValue,
        "role": "volume", "filename": "Volume \(byte)", "kind": "directory", "writable": true,
        "content_version": [UInt8](repeating: byte, count: 32), "metadata_version": [UInt8](repeating: byte, count: 32)]
}

private func entryItem(_ volume: UInt8, _ entry: UInt8, version: UInt8 = 1) -> [String: Any] {
    ["id": entryIdentity(volume, entry), "parent": identity(volume), "role": "entry",
        "filename": "File \(entry)", "kind": "file", "writable": true,
        "content_version": [UInt8](repeating: version, count: 32), "metadata_version": [UInt8](repeating: version, count: 32)]
}

private func awaitResult<T>(_ body: (@escaping (Result<T, Error>) -> Void) -> Void) throws -> T {
    let done = DispatchSemaphore(value: 0)
    var result: Result<T, Error>?
    body { value in result = value; done.signal() }
    precondition(done.wait(timeout: .now() + 5) == .success, "provider callback did not finish")
    return try result!.get()
}

private final class Backend {
    let lock = NSLock()
    var folders: [[String: Any]]
    var retired: [String] = []
    var blocked: Set<String> = []
    var failedOperation: String?
    var beforeScan: ((String) throws -> Void)?
    var revisions: [String: Int] = [:]
    var entries: [String: [[String: Any]]] = [:]
    var calls: [[String: Any]] = []
    init(_ folders: [[String: Any]]) { self.folders = folders }
    func call(_ fields: [String: Any]) throws -> [String: Any] {
        lock.lock(); defer { lock.unlock() }
        calls.append(fields)
        switch fields["op"] as? String {
        case "catalog":
            let rows = folders.map { row -> [String: Any] in
                var value = row
                if let id = row["id"] as? String, blocked.contains(id) {
                    value["unavailable"] = true; value["writable"] = false
                }
                return value
            }
            return ["items": rows, "retired": retired]
        case "root": return ["item": ["role": "root"]]
        case "anchor":
            let volume = fields["volume"] as! String
            if blocked.contains(volume) || failedOperation == "anchor" { throw NSFileProviderError(.serverUnreachable) }
            return ["anchor": ["revision": revisions[volume] ?? 1]]
        case "scan":
            let volume = fields["volume"] as! String
            if blocked.contains(volume) || failedOperation == "scan" { throw NSFileProviderError(.serverUnreachable) }
            try beforeScan?(volume)
            return ["items": entries[volume] ?? [], "next": NSNull()]
        default: preconditionFailure("unexpected provider operation")
        }
    }
    func edit(_ change: (Backend) -> Void) {
        lock.lock(); defer { lock.unlock() }
        change(self)
    }
    func operations(_ op: String) -> [[String: Any]] {
        lock.lock(); defer { lock.unlock() }
        return calls.filter { $0["op"] as? String == op }
    }
}

private final class Changes: NSObject, NSFileProviderChangeObserver {
    let done = DispatchSemaphore(value: 0)
    var updated: [NSFileProviderItem] = []
    var deleted: [NSFileProviderItemIdentifier] = []
    var anchor: NSFileProviderSyncAnchor?
    var moreComing: Bool?
    var error: Error?
    func didUpdate(_ items: [NSFileProviderItem]) { updated.append(contentsOf: items) }
    func didDeleteItems(withIdentifiers identifiers: [NSFileProviderItemIdentifier]) { deleted.append(contentsOf: identifiers) }
    func finishEnumeratingChanges(upTo anchor: NSFileProviderSyncAnchor, moreComing: Bool) {
        self.anchor = anchor; self.moreComing = moreComing; done.signal()
    }
    func finishEnumeratingWithError(_ error: Error) { self.error = error; done.signal() }
    func wait() throws {
        precondition(done.wait(timeout: .now() + 5) == .success, "change enumeration did not finish")
        if let error { throw error }
    }
}

@main
struct ProviderNamespaceTests {
    private static func enumerator(_ backend: Backend, root: URL, account: UInt8 = 200) -> ProviderEnumerator {
        ProviderEnumerator(client: ProviderClient(backend.call),
            domain: NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identity(account)), displayName: "Playbit"),
            container: .workingSet, storage: root)
    }

    static func identitiesAndCapabilities() throws {
        let a = try ProviderNode(NSFileProviderItemIdentifier(entryIdentity(1, 3)))
        let b = try ProviderNode(NSFileProviderItemIdentifier(entryIdentity(2, 3)))
        precondition(a != b)
        precondition(a == .entry(volume: identity(1), entry: identity(3, width: 15)))
        precondition(b == .entry(volume: identity(2), entry: identity(3, width: 15)))
        let rootNode = try ProviderNode(.rootContainer)
        precondition(rootNode == .root)
        let volume = try ProviderNode(NSFileProviderItemIdentifier(identity(1)))
        precondition(volume == .volume(identity(1)))
        let request = try a.entryRequest("delete")
        precondition(request["volume"] as? String == identity(1))
        precondition(request["entry"] as? String == identity(3, width: 15))
        for node in [ProviderNode.root, volume] {
            do { _ = try node.entryRequest("delete"); preconditionFailure("synthetic folder admitted entry mutation") }
            catch { precondition((error as NSError).domain == NSCocoaErrorDomain && (error as NSError).code == CocoaError.fileWriteNoPermission.rawValue) }
        }
        let fields = try volume.parentFields()
        precondition(fields["volume"] as? String == identity(1) && fields["parent"] is NSNull)
        let root = ProviderItem(["role": "root", "kind": "directory", "writable": true])
        let folder = ProviderItem(volumeItem(1))
        let file = ProviderItem(entryItem(1, 3))
        precondition(root.capabilities.contains(.allowsAddingSubItems))
        precondition(!root.capabilities.contains(.allowsRenaming))
        precondition(folder.capabilities.contains(.allowsRenaming))
        precondition(folder.capabilities.contains(.allowsAddingSubItems))
        precondition(!folder.capabilities.contains(.allowsDeleting) && !folder.capabilities.contains(.allowsReparenting))
        precondition(file.capabilities.contains(.allowsDeleting) && file.capabilities.contains(.allowsReparenting))
        var unavailable = volumeItem(1); unavailable["unavailable"] = true
        precondition(!ProviderItem(unavailable).capabilities.contains(.allowsAddingSubItems))
        precondition(!ProviderItem(unavailable).capabilities.contains(.allowsRenaming))
        do { _ = try ProviderNode(NSFileProviderItemIdentifier(identity(1, width: 15))); preconditionFailure("entry without volume admitted") }
        catch { precondition((error as NSError).code == NSFileProviderError.noSuchItem.rawValue) }
    }

    static func exposureSurvivesRemoval(root: URL) throws {
        let backend = Backend([volumeItem(1)])
        let first = enumerator(backend, root: root)
        let exposed = try awaitResult(first.catalog)
        precondition(exposed.count == 1)
        backend.edit { $0.folders = []; $0.retired = [identity(1)] }
        // A fresh adapter must load the persistent exposure ledger and continue
        // presenting a protected unavailable folder after confirmed retirement.
        let reopened = enumerator(backend, root: root)
        let retired = try awaitResult(reopened.catalog)
        precondition(retired.count == 1)
        let retainedFolder = try reopened.folder(retired[identity(1)]!)
        precondition(retainedFolder["unavailable"] as? Bool == true)
        precondition(retainedFolder["writable"] as? Bool == false)
        backend.edit { $0.retired = [] }
        let absent = try awaitResult(reopened.catalog)
        precondition(absent[identity(1)] == retired[identity(1)])
    }

    static func globalExposureLimit(root: URL) throws {
        let firstBackend = Backend((1...128).map { volumeItem(UInt8($0)) })
        let first = enumerator(firstBackend, root: root)
        let folders = try awaitResult(first.catalog)
        precondition(folders.count == 128)
        let second = enumerator(Backend([volumeItem(129)]), root: root, account: 201)
        do { _ = try awaitResult(second.catalog); preconditionFailure("account turnover multiplied exposure capacity") }
        catch { precondition((error as NSError).code == CocoaError.fileWriteOutOfSpace.rawValue) }
        let retained = try awaitResult(first.catalog)
        precondition(retained.count == 128)
    }

    static func reservationAdmission(root: URL) throws {
        let backend = Backend([])
        let pending = enumerator(backend, root: root)
        try pending.reserveVolume(identity(1), name: "Documents")
        // A durably reserved identity consumes capacity without exposing a
        // folder before the backend has committed and returned its catalog row.
        let beforeCommit = try awaitResult(pending.catalog)
        precondition(beforeCommit.isEmpty)
        let reopened = enumerator(backend, root: root)
        let beforeReopenCommit = try awaitResult(reopened.catalog)
        precondition(beforeReopenCommit.isEmpty)
        backend.edit { $0.folders = [volumeItem(1)] }
        let committed = try awaitResult(reopened.catalog)
        precondition(committed.count == 1 && committed[identity(1)] != nil)
        for byte in 2...128 { try reopened.reserveVolume(identity(UInt8(byte)), name: "Pending \(byte)") }
        // Reservations are global across accounts, so admission fails before
        // any create RPC can occur. Existing readable catalog state survives.
        let otherBackend = Backend([])
        let other = enumerator(otherBackend, root: root, account: 201)
        do { try other.reserveVolume(identity(129), name: "Overflow"); preconditionFailure("over-capacity volume reserved") }
        catch { precondition((error as NSError).code == CocoaError.fileWriteOutOfSpace.rawValue) }
        precondition(otherBackend.operations("create_volume").isEmpty)
        let stillReadable = try awaitResult(reopened.catalog)
        precondition(stillReadable.count == 1 && stillReadable[identity(1)] != nil)
        let otherEmpty = try awaitResult(other.catalog)
        precondition(otherEmpty.isEmpty)
        // Exact creation retries reuse their admitted reservation at capacity.
        try reopened.reserveVolume(identity(128), name: "Pending 128")
    }

    static func invalidDomainCannotChangeExposure(root: URL) throws {
        let backend = Backend([volumeItem(1)])
        let current = enumerator(backend, root: root)
        _ = try awaitResult(current.catalog)
        let path = root.appendingPathComponent("exposed.json")
        let before = try Data(contentsOf: path)
        // Old per-volume locations can deliver cached create callbacks after
        // an upgrade. They must not poison the account ledger before RPC admission.
        let old = ProviderEnumerator(client: ProviderClient(backend.call),
            domain: NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(identity(200) + "-" + identity(1)), displayName: "Old location"),
            container: .rootContainer, storage: root)
        do { try old.reserveVolume(identity(2), name: "Pending"); preconditionFailure("invalid domain reserved an account key") }
        catch { precondition((error as NSError).code == NSFileProviderError.notAuthenticated.rawValue) }
        // Revalidate mutations too: a caller cannot persist a malformed key in
        // otherwise valid cached state and break unrelated account enumeration.
        do {
            try current.withExposure(update: true) { $0["invalid-account"] = [:] }
            preconditionFailure("invalid ledger mutation persisted")
        } catch { precondition((error as NSError).code == CocoaError.fileReadCorruptFile.rawValue) }
        let after = try Data(contentsOf: path)
        precondition(after == before)
        try current.reserveVolume(identity(2), name: "Pending")
        let visible = try awaitResult(current.catalog)
        precondition(visible.count == 1 && visible[identity(1)] != nil)
    }

    static func competingProcessReservations(root: URL) throws {
        let current = enumerator(Backend([]), root: root)
        for byte in 1...127 { try current.reserveVolume(identity(UInt8(byte)), name: "Pending \(byte)") }
        let children = [128, 129].map { byte -> Process in
            let child = Process()
            child.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
            child.arguments = ["reserve", root.path, String(byte)]
            return child
        }
        for child in children { try child.run() }
        for child in children { child.waitUntilExit() }
        precondition(children.filter { $0.terminationStatus == 0 }.count == 1)
        precondition(children.filter { $0.terminationStatus == 20 }.count == 1)
        let admitted = try current.withExposure(update: false) { ledger in
            ledger.values.reduce(0) { $0 + $1.count }
        }
        precondition(admitted == 128)
    }

    static func retiredNameCollision(root: URL) throws {
        for (index, liveName) in ["Documents", "documents"].enumerated() {
            var oldFolder = volumeItem(1); oldFolder["filename"] = "Documents"
            let backend = Backend([oldFolder])
            let current = enumerator(backend, root: root.appendingPathComponent("case-\(index)"))
            _ = try awaitResult(current.catalog)
            var liveFolder = volumeItem(2); liveFolder["filename"] = liveName
            backend.edit { $0.folders = [liveFolder]; $0.retired = [identity(1)] }
            let folders = try awaitResult(current.catalog)
            precondition(Set(folders.keys) == Set([identity(1), identity(2)]))
            let unavailable = try current.folder(folders[identity(1)]!)
            let live = try current.folder(folders[identity(2)]!)
            precondition(unavailable["unavailable"] as? Bool == true)
            precondition(live["filename"] as? String == liveName)
            let names = try folders.values.map { try current.folder($0)["filename"] as! String }
            let folded = names.map { $0.precomposedStringWithCanonicalMapping.lowercased() }
            precondition(Set(folded).count == 2)
            let repeatCatalog = try awaitResult(current.catalog)
            precondition(repeatCatalog == folders)
        }
    }

    static func transientFailuresRejectCapture(root: URL) throws {
        for operation in ["anchor", "scan"] {
            let backend = Backend([volumeItem(1)])
            backend.entries = [identity(1): [entryItem(1, 3)]]
            let current = enumerator(backend, root: root.appendingPathComponent(operation))
            let baseline = try awaitResult { current.capture(previous: nil, completion: $0) }
            let anchor = try current.save(baseline)
            backend.edit { $0.failedOperation = operation; $0.revisions[identity(1)] = 2 }
            let changes = Changes()
            current.enumerateChanges(for: changes, from: NSFileProviderSyncAnchor(anchor))
            do { try changes.wait(); preconditionFailure("transient failure became a successful pending checkpoint") }
            catch { precondition((error as NSError).code == NSFileProviderError.serverUnreachable.rawValue) }
            precondition(changes.anchor == nil && changes.moreComing == nil)
            precondition(changes.updated.isEmpty && changes.deleted.isEmpty)
            let retained = try current.load(anchor)
            precondition(retained.components == baseline.components)
            backend.edit { $0.failedOperation = nil }
            let recovered = Changes()
            current.enumerateChanges(for: recovered, from: NSFileProviderSyncAnchor(anchor))
            try recovered.wait()
            precondition(recovered.anchor != nil && recovered.moreComing == false)
        }
    }

    static func componentEvictionDuringDiff(root: URL) throws {
        let backend = Backend([volumeItem(1)])
        backend.entries = [identity(1): [entryItem(1, 3)]]
        let current = enumerator(backend, root: root)
        let baseline = try awaitResult { current.capture(previous: nil, completion: $0) }
        let anchor = try current.save(baseline)
        let oldFile = try current.componentURL(identity(1), file: baseline.components[identity(1)]!.file!)
        backend.edit {
            $0.revisions[identity(1)] = 2
            $0.entries[identity(1)] = [entryItem(1, 3, version: 2)]
            $0.beforeScan = { _ in try FileManager.default.removeItem(at: oldFile) }
        }
        let changes = Changes()
        current.enumerateChanges(for: changes, from: NSFileProviderSyncAnchor(anchor))
        do { try changes.wait(); preconditionFailure("an evicted comparison component completed a new anchor") }
        catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
        precondition(changes.anchor == nil && changes.moreComing == nil)
        precondition(changes.updated.isEmpty && changes.deleted.isEmpty)
    }

    static func unavailableEvictedComponent(root: URL) throws {
        let backend = Backend([volumeItem(1)])
        backend.entries = [identity(1): [entryItem(1, 3)]]
        let current = enumerator(backend, root: root)
        let baseline = try awaitResult { current.capture(previous: nil, completion: $0) }
        _ = try current.save(baseline)
        let file = baseline.components[identity(1)]!.file!
        try FileManager.default.removeItem(at: current.componentURL(identity(1), file: file))
        backend.edit { $0.blocked = [identity(1)] }
        let anchor: NSFileProviderSyncAnchor? = try awaitResult { completion in
            current.currentSyncAnchor { completion(.success($0)) }
        }
        // An expired latest manifest must not be inherited into an anchor that
        // names the unavailable volume's missing component file.
        if let anchor {
            let manifest = try current.load(anchor.rawValue)
            precondition(manifest.components[identity(1)]?.pending == true)
            precondition(manifest.components[identity(1)]?.file == nil)
        }
    }

    static func pendingCaptureAndIncrementalChanges(root: URL) throws {
        let backend = Backend([volumeItem(1), volumeItem(2)])
        backend.entries = [identity(1): [entryItem(1, 3)], identity(2): [entryItem(2, 3)]]
        backend.blocked = [identity(1)]
        let enumerator = enumerator(backend, root: root)
        let initial = try awaitResult { enumerator.capture(previous: nil, completion: $0) }
        precondition(initial.components[identity(1)]?.pending == true)
        precondition(initial.components[identity(1)]?.file == nil)
        precondition(initial.components[identity(2)]?.pending == false)
        let anchor = try enumerator.save(initial)
        backend.edit { $0.blocked = []; $0.calls = [] }
        let ready = Changes()
        enumerator.enumerateChanges(for: ready, from: NSFileProviderSyncAnchor(anchor))
        try ready.wait()
        precondition(ready.moreComing == false && ready.deleted.isEmpty)
        precondition(ready.updated.map(\.itemIdentifier.rawValue) == [identity(1), entryIdentity(1, 3)])
        precondition(backend.operations("scan").map { $0["volume"] as! String } == [identity(1)])
        let recovered = try enumerator.load(ready.anchor!.rawValue)
        precondition(recovered.components[identity(1)]?.pending == false)
        precondition(recovered.components[identity(2)] == initial.components[identity(2)])
        precondition(backend.operations("anchor").filter { $0["volume"] as? String == identity(1) }.count == 2)
        // The catalog is unchanged. Only volume 2's changed cut is scanned and
        // compared; the retained volume 1 component is reused exactly.
        backend.edit {
            $0.calls = []; $0.revisions[identity(2)] = 2
            $0.entries[identity(2)] = [entryItem(2, 3, version: 2)]
        }
        let changed = Changes()
        enumerator.enumerateChanges(for: changed, from: ready.anchor!)
        try changed.wait()
        precondition(changed.moreComing == false && changed.deleted.isEmpty)
        precondition(changed.updated.map(\.itemIdentifier.rawValue) == [entryIdentity(2, 3)])
        precondition(backend.operations("scan").map { $0["volume"] as! String } == [identity(2)])
        let latest = try enumerator.load(changed.anchor!.rawValue)
        precondition(latest.components[identity(1)] == recovered.components[identity(1)])
        // Losing a referenced component explicitly expires the anchor rather
        // than interpreting the unavailable cache as a complete empty volume.
        try FileManager.default.removeItem(at: enumerator.componentURL(identity(2), file: latest.components[identity(2)]!.file!))
        do { _ = try enumerator.load(changed.anchor!.rawValue); preconditionFailure("evicted component accepted") }
        catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
        let expired = Changes()
        enumerator.enumerateChanges(for: expired, from: changed.anchor!)
        do { try expired.wait(); preconditionFailure("expired changes anchor accepted") }
        catch { precondition((error as NSError).code == NSFileProviderError.syncAnchorExpired.rawValue) }
    }

    static func main() throws {
        if CommandLine.arguments.count == 4 && CommandLine.arguments[1] == "reserve" {
            let root = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
            let current = enumerator(Backend([]), root: root)
            do {
                try current.reserveVolume(identity(UInt8(CommandLine.arguments[3])!), name: "Contending reservation")
                exit(0)
            } catch {
                let code = (error as NSError).code
                guard code == CocoaError.fileWriteOutOfSpace.rawValue || code == CocoaError.userCancelled.rawValue else { throw error }
                exit(20)
            }
        }
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("provider-namespace-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }
        try identitiesAndCapabilities()
        try exposureSurvivesRemoval(root: root.appendingPathComponent("retirement"))
        try globalExposureLimit(root: root.appendingPathComponent("capacity"))
        try reservationAdmission(root: root.appendingPathComponent("reservations"))
        try invalidDomainCannotChangeExposure(root: root.appendingPathComponent("invalid-domain"))
        try competingProcessReservations(root: root.appendingPathComponent("cross-process"))
        try retiredNameCollision(root: root.appendingPathComponent("names"))
        try transientFailuresRejectCapture(root: root.appendingPathComponent("transient"))
        try componentEvictionDuringDiff(root: root.appendingPathComponent("diff-eviction"))
        try unavailableEvictedComponent(root: root.appendingPathComponent("eviction"))
        try pendingCaptureAndIncrementalChanges(root: root.appendingPathComponent("checkpoints"))
        print("File Provider namespace, retirement, pending recovery and checkpoint isolation passed")
    }
}
