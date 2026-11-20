import Foundation
import FileProvider
import CryptoKit
import Darwin

/// Each account anchor names bounded, independently captured volume cuts. Pending
/// components retain their last cut until initialization or reconnect completes.
final class ProviderEnumerator: NSObject, NSFileProviderEnumerator {
    static let checkpointLock = NSLock()
    static let captures = DispatchSemaphore(value: 2)
    static let retainedVolumes = 128
    static let retainedVersions = 512
    #if os(iOS)
    static let checkpointItems = 1024
    static let checkpointBytes = 1048576
    static let encodedCheckpointBytes = 2097152
    #else
    static let checkpointItems = 16384
    static let checkpointBytes = 8388608
    static let encodedCheckpointBytes = 16777216
    #endif
    let client: ProviderClient
    let domain: NSFileProviderDomain
    let container: NSFileProviderItemIdentifier
    let storage: URL?
    let lifecycle = NSLock()
    var invalidated = false
    init(client: ProviderClient, domain: NSFileProviderDomain, container: NSFileProviderItemIdentifier, storage: URL? = nil) {
        self.client = client; self.domain = domain; self.container = container; self.storage = storage
    }
    func invalidate() { lifecycle.lock(); invalidated = true; lifecycle.unlock() }
    func checkActive() throws {
        lifecycle.lock(); defer { lifecycle.unlock() }
        if invalidated { throw CocoaError(.userCancelled) }
    }
    static func hash(_ bytes: Data) -> String {
        SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
    }
    static func encode<T: Encodable>(_ value: T) throws -> Data {
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
        return try encoder.encode(value)
    }
    struct Component: Codable, Equatable {
        var anchor: Data?
        var file: String?
        var pending: Bool
    }
    struct Manifest: Codable {
        var folders: [String: Data]
        var components: [String: Component]
    }
    struct Checkpoint: Codable {
        let anchor: Data
        let items: [String: Data]
    }
    struct WorkingPage: Codable {
        let catalog: String
        let volume: String
        let after: String?
    }
    func rootDirectory() throws -> URL {
        let root = try storage ?? FileManager.default.url(for: .applicationSupportDirectory, in: .userDomainMask,
            appropriateFor: nil, create: true).appendingPathComponent("AccountProviderCheckpoints", isDirectory: true)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        return root
    }
    func accountDirectory() throws -> URL {
        guard Identifier.valid(domain.identifier.rawValue) else { throw NSFileProviderError(.notAuthenticated) }
        let directory = try rootDirectory().appendingPathComponent(Self.hash(Data(domain.identifier.rawValue.utf8)), isDirectory: true)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let others = try FileManager.default.contentsOfDirectory(at: rootDirectory(), includingPropertiesForKeys: [.contentModificationDateKey])
            .filter { validHash($0.lastPathComponent) && $0 != directory }
            .sorted { ((try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast)
                > ((try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast) }
        for expired in others.dropFirst(Self.retainedVolumes - 1) { try FileManager.default.removeItem(at: expired) }
        return directory
    }
    func read<T: Decodable>(_ type: T.Type, at url: URL, limit: Int) throws -> T {
        let metadata = try url.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
        guard metadata.isRegularFile == true, let size = metadata.fileSize, size <= limit else {
            throw CocoaError(.fileReadTooLarge)
        }
        return try JSONDecoder().decode(type, from: Data(contentsOf: url))
    }
    func folder(_ bytes: Data) throws -> [String: Any] {
        guard let value = try JSONSerialization.jsonObject(with: bytes) as? [String: Any] else {
            throw CocoaError(.fileReadCorruptFile)
        }
        return value
    }
    func unavailable(_ bytes: Data) throws -> Data {
        var value = try folder(bytes)
        value["unavailable"] = true; value["writable"] = false
        return try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
    }
    /// Short cross-process leases protect exposure admission before local commits.
    /// The ledger is durable state, separate from evictable checkpoint metadata.
    func withExposure<T>(update: Bool, _ body: (inout [String: [String: Data]]) throws -> T) throws -> T {
        guard Identifier.valid(domain.identifier.rawValue) else { throw NSFileProviderError(.notAuthenticated) }
        let activity = ProcessInfo.processInfo.beginActivity(options: .userInitiated, reason: "Keep Playbit Files edits")
        defer { ProcessInfo.processInfo.endActivity(activity) }
        Self.checkpointLock.lock(); defer { Self.checkpointLock.unlock() }
        let root = try rootDirectory()
        let descriptor = Darwin.open(root.appendingPathComponent("exposure.lock").path,
            O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0o600)
        guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
        defer { Darwin.close(descriptor) }
        guard flock(descriptor, LOCK_EX | LOCK_NB) == 0 else { throw CocoaError(.userCancelled) }
        defer { flock(descriptor, LOCK_UN) }
        let url = root.appendingPathComponent("exposed.json")
        var ledger = FileManager.default.fileExists(atPath: url.path)
            ? try read([String: [String: Data]].self, at: url, limit: 524288) : [:]
        guard ledger.count <= Self.retainedVolumes,
              ledger.keys.allSatisfy({ Identifier.valid($0) }),
              ledger.values.reduce(0, { $0 + $1.count }) <= Self.retainedVolumes,
              ledger.values.allSatisfy({ $0.keys.allSatisfy { Identifier.valid($0) } }) else { throw CocoaError(.fileReadCorruptFile) }
        let value = try body(&ledger)
        if update {
            guard ledger.count <= Self.retainedVolumes,
                  ledger.values.reduce(0, { $0 + $1.count }) <= Self.retainedVolumes else { throw CocoaError(.fileWriteOutOfSpace) }
            guard ledger.keys.allSatisfy({ Identifier.valid($0) }),
                  ledger.values.allSatisfy({ $0.keys.allSatisfy { Identifier.valid($0) } }) else { throw CocoaError(.fileReadCorruptFile) }
            let bytes = try Self.encode(ledger)
            guard bytes.count <= 524288 else { throw CocoaError(.fileWriteOutOfSpace) }
            try bytes.write(to: url, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
            for path in [url.path, root.path] {
                let file = Darwin.open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
                guard file >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
                let result = fsync(file), status = errno
                Darwin.close(file)
                guard result == 0 else { throw POSIXError(POSIXErrorCode(rawValue: status) ?? .EIO) }
            }
        }
        return value
    }
    func reserveVolume(_ volume: String, name: String) throws {
        guard Identifier.valid(volume) else { throw CocoaError(.fileReadCorruptFile) }
        try withExposure(update: true) { ledger in
            var known = ledger[domain.identifier.rawValue] ?? [:]
            if known[volume] == nil {
                known[volume] = try JSONSerialization.data(withJSONObject: ["id": volume, "filename": name, "reserved": true], options: [.sortedKeys])
            }
            ledger[domain.identifier.rawValue] = known
        }
    }
    func disambiguate(_ folders: inout [String: Data]) throws {
        func key(_ value: String) -> String { value.precomposedStringWithCanonicalMapping.lowercased() }
        var used = Set<String>()
        let identifiers = try folders.keys.sorted().sorted {
            (try folder(folders[$0]!)["unavailable"] as? Bool != true ? 0 : 1)
                < (try folder(folders[$1]!)["unavailable"] as? Bool != true ? 0 : 1)
        }
        for id in identifiers {
            var value = try folder(folders[id]!)
            guard let original = value["filename"] as? String else { throw CocoaError(.fileReadCorruptFile) }
            if used.insert(key(original)).inserted { continue }
            guard value["unavailable"] as? Bool == true else { throw CocoaError(.fileReadCorruptFile) }
            let escaped = id.utf8.map { (65...90).contains($0) ? String(format: "%%%02X", $0) : String(UnicodeScalar($0)) }.joined()
            var resolved: String?
            for attempt in 0...Self.retainedVolumes {
                let suffix = " (Unavailable " + escaped + (attempt == 0 ? "" : "-\(attempt)") + ")"
                var stem = original
                while stem.utf8.count + suffix.utf8.count > 255 { stem.removeLast() }
                let candidate = stem + suffix
                if used.insert(key(candidate)).inserted { resolved = candidate; break }
            }
            guard let resolved else { throw CocoaError(.fileWriteOutOfSpace) }
            value["filename"] = resolved
            var revision = Data(value["metadata_version"] as? [UInt8] ?? [])
            revision.append(Data(resolved.utf8))
            value["metadata_version"] = Array(SHA256.hash(data: revision))
            folders[id] = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        }
    }
    func catalog(_ completion: @escaping (Result<[String: Data], Error>) -> Void) {
        do {
            try checkActive()
            let known = try withExposure(update: false) { Array(($0[domain.identifier.rawValue] ?? [:]).keys).sorted() }
            client.call(["op": "catalog", "known": known]) { result in
                do {
                    try self.checkActive()
                    let value = try result.get()
                    guard let rows = value["items"] as? [[String: Any]], rows.count <= Self.retainedVolumes,
                          let retired = value["retired"] as? [String], retired.count <= Self.retainedVolumes else {
                        throw CocoaError(.fileReadCorruptFile)
                    }
                    var current: [String: Data] = [:]
                    for row in rows {
                        guard let id = row["id"] as? String, Identifier.valid(id), current[id] == nil else { throw CocoaError(.fileReadCorruptFile) }
                        current[id] = try JSONSerialization.data(withJSONObject: row, options: [.sortedKeys])
                    }
                    try self.withExposure(update: true) { ledger in
                        let previous = ledger[self.domain.identifier.rawValue] ?? [:]
                        var reservations: [String: Data] = [:]
                        for (id, bytes) in previous where current[id] == nil || retired.contains(id) {
                            if try self.folder(bytes)["reserved"] as? Bool == true { reservations[id] = bytes }
                            else { current[id] = try self.unavailable(bytes) }
                        }
                        try self.disambiguate(&current)
                        ledger[self.domain.identifier.rawValue] = current.merging(reservations) { visible, _ in visible }
                    }
                    completion(.success(current))
                } catch { completion(.failure(error)) }
            }
        } catch { completion(.failure(error)) }
    }
    func enumerateItems(for observer: NSFileProviderEnumerationObserver, startingAt page: NSFileProviderPage) {
        let initial = page.rawValue == NSFileProviderPage.initialPageSortedByName as Data || page.rawValue == NSFileProviderPage.initialPageSortedByDate as Data
        if container == .rootContainer || container == .workingSet {
            catalog { result in
                do {
                    let folders = try result.get()
                    if initial {
                        observer.didEnumerate(try folders.keys.sorted().compactMap { try folders[$0].map { ProviderItem(try self.folder($0)) } })
                        if self.container == .workingSet, let first = try self.available(folders).first {
                            let cursor = WorkingPage(catalog: Self.hash(try Self.encode(folders)), volume: first, after: nil)
                            observer.finishEnumerating(upTo: NSFileProviderPage(try Self.encode(cursor)))
                        } else { observer.finishEnumerating(upTo: nil) }
                    } else {
                        guard self.container == .workingSet, page.rawValue.count <= 4096 else { throw NSFileProviderError(.pageExpired) }
                        let cursor = try JSONDecoder().decode(WorkingPage.self, from: page.rawValue)
                        guard cursor.catalog == Self.hash(try Self.encode(folders)), Identifier.valid(cursor.volume),
                              cursor.after == nil || Identifier.valid(cursor.after!, bytes: 15) else { throw NSFileProviderError(.pageExpired) }
                        self.enumerateWorkingPage(cursor, folders: folders, observer: observer)
                    }
                } catch { observer.finishEnumeratingWithError(error) }
            }
            return
        }
        do {
            let node = try ProviderNode(container)
            var fields = try node.parentFields(); fields["op"] = "list"
            if !initial {
                guard let after = String(data: page.rawValue, encoding: .utf8), Identifier.valid(after, bytes: 15) else {
                    throw NSFileProviderError(.pageExpired)
                }
                fields["after"] = after
            }
            client.call(fields) { result in
                do {
                    try self.checkActive()
                    let value = try result.get()
                    guard let body = value["page"] as? [String: Any], let items = body["items"] as? [[String: Any]], items.count <= 128 else {
                        throw CocoaError(.fileReadCorruptFile)
                    }
                    observer.didEnumerate(items.map(ProviderItem.init))
                    observer.finishEnumerating(upTo: (body["next"] as? String).map { NSFileProviderPage(Data($0.utf8)) })
                } catch { observer.finishEnumeratingWithError(error) }
            }
        } catch { observer.finishEnumeratingWithError(error) }
    }
    func available(_ folders: [String: Data]) throws -> [String] {
        try folders.keys.sorted().filter { try folder(folders[$0]!)["unavailable"] as? Bool != true }
    }
    func enumerateWorkingPage(_ cursor: WorkingPage, folders: [String: Data], observer: NSFileProviderEnumerationObserver) {
        var fields: [String: Any] = ["op": "scan", "volume": cursor.volume]
        fields["after"] = cursor.after
        client.call(fields) { result in
            do {
                try self.checkActive()
                let value = try result.get()
                guard let items = value["items"] as? [[String: Any]], items.count <= 128 else { throw CocoaError(.fileReadCorruptFile) }
                observer.didEnumerate(items.map(ProviderItem.init))
                let next: WorkingPage?
                if let after = value["next"] as? String {
                    guard Identifier.valid(after, bytes: 15), after != cursor.after else { throw CocoaError(.fileReadCorruptFile) }
                    next = WorkingPage(catalog: cursor.catalog, volume: cursor.volume, after: after)
                } else {
                    let remaining = try self.available(folders).filter { $0 > cursor.volume }
                    next = remaining.first.map { WorkingPage(catalog: cursor.catalog, volume: $0, after: nil) }
                }
                observer.finishEnumerating(upTo: try next.map { NSFileProviderPage(try Self.encode($0)) })
            } catch { observer.finishEnumeratingWithError(error) }
        }
    }
    func currentSyncAnchor(completionHandler: @escaping (NSFileProviderSyncAnchor?) -> Void) {
        guard Self.captures.wait(timeout: .now()) == .success else { completionHandler(nil); return }
        let previous = try? latest()
        capture(previous: previous) { result in
            defer { Self.captures.signal() }
            do {
                let manifest = try result.get()
                completionHandler(NSFileProviderSyncAnchor(try self.save(manifest)))
            } catch { completionHandler(nil) }
        }
    }
    func enumerateChanges(for observer: NSFileProviderChangeObserver, from anchor: NSFileProviderSyncAnchor) {
        guard Self.captures.wait(timeout: .now()) == .success else {
            observer.finishEnumeratingWithError(NSFileProviderError(.serverUnreachable)); return
        }
        do {
            let old = try load(anchor.rawValue)
            capture(previous: old) { result in
                defer { Self.captures.signal() }
                do {
                    try self.checkActive()
                    let next = try result.get()
                    let anchor = try self.save(next)
                    try self.emit(old.folders, next.folders, observer: observer)
                    for volume in Set(old.components.keys).union(next.components.keys).sorted() {
                        let before = old.components[volume], after = next.components[volume]
                        if before?.file == after?.file { continue }
                        let oldItems: [String: Data], nextItems: [String: Data]
                        do {
                            oldItems = try before?.file.map { try self.loadComponent(volume, file: $0).items } ?? [:]
                            nextItems = try after?.file.map { try self.loadComponent(volume, file: $0).items } ?? [:]
                        } catch { throw NSFileProviderError(.syncAnchorExpired) }
                        try self.emit(oldItems, nextItems, observer: observer)
                    }
                    observer.finishEnumeratingChanges(upTo: NSFileProviderSyncAnchor(anchor), moreComing: false)
                } catch { observer.finishEnumeratingWithError(error) }
            }
        } catch {
            Self.captures.signal()
            observer.finishEnumeratingWithError(NSFileProviderError(.syncAnchorExpired))
        }
    }
    func emit(_ before: [String: Data], _ after: [String: Data], observer: NSFileProviderChangeObserver) throws {
        var removed: [NSFileProviderItemIdentifier] = [], changed: [NSFileProviderItem] = []
        for id in before.keys.sorted() where after[id] == nil {
            removed.append(NSFileProviderItemIdentifier(id))
            if removed.count == 128 { observer.didDeleteItems(withIdentifiers: removed); removed.removeAll(keepingCapacity: true) }
        }
        if !removed.isEmpty { observer.didDeleteItems(withIdentifiers: removed) }
        for id in after.keys.sorted() where before[id] != after[id] {
            changed.append(ProviderItem(try folder(after[id]!)))
            if changed.count == 128 { observer.didUpdate(changed); changed.removeAll(keepingCapacity: true) }
        }
        if !changed.isEmpty { observer.didUpdate(changed) }
    }
    func anchor(_ volume: String, _ completion: @escaping (Result<Data, Error>) -> Void) {
        client.call(["op": "anchor", "volume": volume]) { result in
            completion(result.flatMap { value in Result {
                guard let anchor = value["anchor"], !(anchor is NSNull) else { throw NSFileProviderError(.serverUnreachable) }
                return try JSONSerialization.data(withJSONObject: anchor, options: [.sortedKeys])
            } })
        }
    }
    func capture(previous: Manifest?, completion: @escaping (Result<Manifest, Error>) -> Void) {
        catalog { result in
            switch result {
            case .failure(let error): completion(.failure(error))
            case .success(let folders):
                let state = Capture(folders: folders, previous: previous)
                self.captureNext(state, completion: completion)
            }
        }
    }
    final class Capture {
        var manifest: Manifest
        let volumes: [String]
        var next = 0
        init(folders: [String: Data], previous: Manifest?) {
            manifest = Manifest(folders: folders, components: previous?.components ?? [:])
            volumes = folders.keys.sorted()
        }
    }
    func captureNext(_ state: Capture, completion: @escaping (Result<Manifest, Error>) -> Void) {
        do {
            try checkActive()
            guard state.next < state.volumes.count else {
                client.call(["op": "root"]) { result in
                    completion(result.map { _ in state.manifest })
                }
                return
            }
            let volume = state.volumes[state.next]; state.next += 1
            let old = state.manifest.components[volume]
            let resume: (Result<Component, Error>) -> Void = { result in
                switch result {
                case .success(let component): state.manifest.components[volume] = component
                case .failure(let error): completion(.failure(error)); return
                }
                // Yield between volume captures; one unavailable volume never spins
                // or forces unrelated, already captured components to be discarded.
                DispatchQueue.global(qos: .utility).async { self.captureNext(state, completion: completion) }
            }
            if try folder(state.manifest.folders[volume]!)["unavailable"] as? Bool == true {
                state.manifest.components[volume] = Component(anchor: old?.anchor, file: old?.file, pending: true)
                DispatchQueue.global(qos: .utility).async { self.captureNext(state, completion: completion) }
                return
            }
            anchor(volume) { result in
                switch result {
                case .failure(let error): resume(.failure(error))
                case .success(let before):
                    if let old, old.anchor == before, !old.pending, let file = old.file,
                       (try? self.loadComponent(volume, file: file)) != nil { resume(.success(old)); return }
                    self.captureVolume(volume, before: before, attempt: 0, completion: resume)
                }
            }
        } catch { completion(.failure(error)) }
    }
    func captureVolume(_ volume: String, before: Data, attempt: Int, completion: @escaping (Result<Component, Error>) -> Void) {
        scan(volume, after: nil, state: Scan()) { result in
            switch result {
            case .failure(let error): completion(.failure(error))
            case .success(let checkpoint):
                self.anchor(volume) { result in
                    do {
                        let after = try result.get()
                        if before != after {
                            guard attempt < 2 else { throw NSFileProviderError(.serverUnreachable) }
                            self.captureVolume(volume, before: after, attempt: attempt + 1, completion: completion)
                            return
                        }
                        let file = try self.saveComponent(volume, checkpoint: Checkpoint(anchor: after, items: checkpoint.items))
                        completion(.success(Component(anchor: after, file: file, pending: false)))
                    } catch { completion(.failure(error)) }
                }
            }
        }
    }
    final class Scan {
        var items: [String: Data] = [:]
        var bytes = 0
        var pages = 0
    }
    func scan(_ volume: String, after: String?, state: Scan, completion: @escaping (Result<Scan, Error>) -> Void) {
        do { try checkActive() } catch { completion(.failure(error)); return }
        var fields: [String: Any] = ["op": "scan", "volume": volume]; fields["after"] = after
        client.call(fields) { result in
            do {
                try self.checkActive()
                let value = try result.get()
                guard let items = value["items"] as? [[String: Any]], items.count <= 128 else { throw CocoaError(.fileReadCorruptFile) }
                state.pages += 1
                guard state.pages <= 256 else { throw CocoaError(.fileReadTooLarge) }
                for item in items {
                    guard let id = item["id"] as? String, Identifier.valid(id, bytes: 31),
                          try ProviderNode(NSFileProviderItemIdentifier(id)).volume == volume, state.items[id] == nil else {
                        throw CocoaError(.fileReadCorruptFile)
                    }
                    let bytes = try JSONSerialization.data(withJSONObject: item, options: [.sortedKeys])
                    state.bytes += bytes.count
                    guard state.items.count < Self.checkpointItems, state.bytes <= Self.checkpointBytes else { throw CocoaError(.fileReadTooLarge) }
                    state.items[id] = bytes
                }
                if let next = value["next"] as? String {
                    guard Identifier.valid(next, bytes: 15), next != after else { throw CocoaError(.fileReadCorruptFile) }
                    self.scan(volume, after: next, state: state, completion: completion)
                } else { completion(.success(state)) }
            } catch { completion(.failure(error)) }
        }
    }
    func validHash(_ text: String) -> Bool { text.count == 64 && text.allSatisfy(\.isHexDigit) }
    func componentURL(_ volume: String, file: String) throws -> URL {
        guard Identifier.valid(volume), validHash(file) else { throw CocoaError(.fileReadCorruptFile) }
        return try accountDirectory().appendingPathComponent("components", isDirectory: true)
            .appendingPathComponent(volume, isDirectory: true).appendingPathComponent(file + ".json")
    }
    func loadComponent(_ volume: String, file: String) throws -> Checkpoint {
        let checkpoint = try read(Checkpoint.self, at: componentURL(volume, file: file), limit: Self.encodedCheckpointBytes)
        guard checkpoint.items.count <= Self.checkpointItems,
              checkpoint.items.values.reduce(0, { $0 + $1.count }) <= Self.checkpointBytes,
              Self.hash(try Self.encode(checkpoint)) == file else { throw CocoaError(.fileReadCorruptFile) }
        return checkpoint
    }
    func saveComponent(_ volume: String, checkpoint: Checkpoint) throws -> String {
        let bytes = try Self.encode(checkpoint)
        guard bytes.count <= Self.encodedCheckpointBytes else { throw CocoaError(.fileWriteOutOfSpace) }
        let file = Self.hash(bytes)
        Self.checkpointLock.lock(); defer { Self.checkpointLock.unlock() }
        let url = try componentURL(volume, file: file)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try bytes.write(to: url, options: .atomic)
        try trimComponents()
        return file
    }
    func trimComponents() throws {
        let root = try rootDirectory()
        let accounts = try FileManager.default.contentsOfDirectory(at: root, includingPropertiesForKeys: [.isDirectoryKey])
        var files: [(URL, Date)] = []
        var identities = 0
        for account in accounts where validHash(account.lastPathComponent) {
            let directory = account.appendingPathComponent("components", isDirectory: true)
            guard FileManager.default.fileExists(atPath: directory.path) else { continue }
            for volume in try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.isDirectoryKey]) {
                guard Identifier.valid(volume.lastPathComponent) else { throw CocoaError(.fileReadCorruptFile) }
                identities += 1
                guard identities <= Self.retainedVolumes else { throw CocoaError(.fileWriteOutOfSpace) }
                let versions = try FileManager.default.contentsOfDirectory(at: volume, includingPropertiesForKeys: [.contentModificationDateKey])
                    .filter { validHash($0.deletingPathExtension().lastPathComponent) && $0.pathExtension == "json" }
                    .map { ($0, (try $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast) }
                    .sorted { $0.1 > $1.1 }
                for (file, _) in versions.dropFirst(4) { try FileManager.default.removeItem(at: file) }
                files.append(contentsOf: versions.prefix(4))
            }
        }
        for (file, _) in files.sorted(by: { $0.1 > $1.1 }).dropFirst(Self.retainedVersions) {
            try FileManager.default.removeItem(at: file)
        }
    }
    func manifestURL(_ anchor: Data) throws -> URL {
        guard anchor.count == 32 else { throw NSFileProviderError(.syncAnchorExpired) }
        return try accountDirectory().appendingPathComponent(Self.hash(anchor) + ".manifest")
    }
    func load(_ anchor: Data) throws -> Manifest {
        Self.checkpointLock.lock(); defer { Self.checkpointLock.unlock() }
        let value = try read(Manifest.self, at: manifestURL(anchor), limit: 524288)
        guard value.folders.count <= Self.retainedVolumes, value.components.count <= Self.retainedVolumes,
              value.folders.keys.allSatisfy({ Identifier.valid($0) }), value.components.keys.allSatisfy({ Identifier.valid($0) }),
              Data(SHA256.hash(data: try Self.encode(value))) == anchor else { throw NSFileProviderError(.syncAnchorExpired) }
        for (volume, component) in value.components {
            if let file = component.file, !FileManager.default.fileExists(atPath: try componentURL(volume, file: file).path) {
                throw NSFileProviderError(.syncAnchorExpired)
            }
        }
        return value
    }
    func latest() throws -> Manifest? {
        let directory = try accountDirectory()
        let latest = try FileManager.default.contentsOfDirectory(at: directory, includingPropertiesForKeys: [.contentModificationDateKey])
            .filter { $0.pathExtension == "manifest" }
            .sorted { ((try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast)
                > ((try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast) }.first
        guard let latest else { return nil }
        let candidate = try read(Manifest.self, at: latest, limit: 524288)
        return try load(Data(SHA256.hash(data: Self.encode(candidate))))
    }
    func save(_ manifest: Manifest) throws -> Data {
        try checkActive()
        let bytes = try Self.encode(manifest)
        guard bytes.count <= 524288 else { throw CocoaError(.fileWriteOutOfSpace) }
        let anchor = Data(SHA256.hash(data: bytes))
        Self.checkpointLock.lock(); defer { Self.checkpointLock.unlock() }
        let url = try manifestURL(anchor)
        try bytes.write(to: url, options: .atomic)
        let files = try FileManager.default.contentsOfDirectory(at: url.deletingLastPathComponent(), includingPropertiesForKeys: [.contentModificationDateKey])
            .filter { $0.pathExtension == "manifest" }
            .sorted { ((try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast)
                > ((try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast) }
        for file in files.dropFirst(4) { try FileManager.default.removeItem(at: file) }
        return anchor
    }
}
