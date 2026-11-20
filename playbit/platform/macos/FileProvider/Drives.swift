import Foundation
import FileProvider
import PushKit
import AppKit

/// One background coordinator per engine. Readiness comes from its change wait,
/// never a timer; no filesystem or remote work runs on Playbit's UI thread.
final class Drives: NSObject, PKPushRegistryDelegate {
    var registry: PKPushRegistry!
    var domains: [String: NSFileProviderDomain] = [:]
    var dirty: Set<String> = []
    var pending: (account: String, ready: Bool)?
    var reconciling = false
    var token: Data?
    var tokenInvalidated = false
    var connected = false
    var registrationPending = false
    var lastRegistration: String?
    let bridge: URL
    let process = Process()
    let input = Pipe()
    var buffer = Data()
    var profile = ""
    init(bridge: URL) { self.bridge = bridge; super.init() }
    func start() throws {
        registry = PKPushRegistry(queue: .main)
        registry.delegate = self
        registry.desiredPushTypes = [.fileProvider]
        NSLog("File Provider push credentials requested")
        process.executableURL = bridge
        process.arguments = ["--watch"]
        process.standardOutput = input
        process.standardError = FileHandle.standardError
        input.fileHandleForReading.readabilityHandler = { handle in
            let bytes = handle.availableData
            if bytes.isEmpty { exit(0) }
            DispatchQueue.main.async {
                self.buffer.append(bytes)
                guard self.buffer.count <= 524288 else { exit(1) }
                while let end = self.buffer.firstIndex(of: 10) {
                    let line = self.buffer[..<end]
                    self.buffer.removeSubrange(...end)
                    guard let value = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                        let account = value["account"] as? String,
                        let profile = value["profile"] as? String,
                        let volumes = value["volumes"] as? [[String: Any]] else { continue }
                    self.profile = profile
                    self.connected = value["connected"] as? Bool ?? false
                    guard volumes.count <= 256 else { exit(1) }
                    self.pending = (account, value["provider_ready"] as? Bool ?? false)
                    self.dirty.insert(account)
                    self.reconcile()
                }
            }
        }
        try process.run()
    }
    func reconcile() {
        guard !reconciling, let desired = pending else { return }
        pending = nil
        reconciling = true
        let signals = dirty
        dirty.removeAll()
        NSFileProviderManager.getDomainsWithCompletionHandler { current, error in
            DispatchQueue.main.async {
                guard error == nil else {
                    if self.pending == nil { self.pending = desired }
                    self.dirty.formUnion(signals)
                    self.reconciling = false
                    NSLog("File Provider domain discovery failed")
                    return
                }
                let group = DispatchGroup()
                for domain in current where domain.userInfo?["profile"] as? String == self.profile {
                    guard !desired.ready || domain.identifier.rawValue != desired.account else { continue }
                    // Old layouts need explicit retirement after retaining their
                    // pending replicas. A catalog refresh never deletes them.
                    if !Identifier.valid(domain.identifier.rawValue) {
                        NSLog("Retain the previous Playbit File Provider locations before retiring them")
                        continue
                    }
                    group.enter()
                    NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData) { _, error in
                        if let error { NSLog("File Provider removal: %@", error.localizedDescription) }
                        group.leave()
                    }
                }
                self.domains.removeAll()
                if desired.ready, Identifier.valid(desired.account) {
                    let identifier = NSFileProviderDomainIdentifier(desired.account)
                    let domain = NSFileProviderDomain(identifier: identifier, displayName: "Playbit")
                    domain.userInfo = ["profile": self.profile]
                    domain.supportsSyncingTrash = false
                    self.domains[desired.account] = domain
                    if !current.contains(where: { $0.identifier == identifier && $0.displayName == "Playbit" }) {
                        group.enter()
                        NSFileProviderManager.add(domain) { error in
                            if let error { NSLog("File Provider registration: %@", error.localizedDescription) }
                            group.leave()
                        }
                    }
                }
                group.notify(queue: .main) {
                    for id in signals {
                        guard let domain = self.domains[id] else { continue }
                        NSFileProviderManager(for: domain)?.signalEnumerator(for: .workingSet) { error in
                            if let error { NSLog("File Provider signal: %@", error.localizedDescription) }
                        }
                    }
                    self.reconciling = false
                    self.registerToken()
                    self.reconcile()
                }
            }
        }
    }
    func pushRegistry(_ registry: PKPushRegistry, didUpdate pushCredentials: PKPushCredentials, for type: PKPushType) {
        NSLog("File Provider push credentials received")
        token = pushCredentials.token; tokenInvalidated = false; registerToken()
    }
    func pushRegistry(_ registry: PKPushRegistry, didInvalidatePushTokenFor type: PKPushType) {
        guard let token, !token.isEmpty else { return }
        tokenInvalidated = true; registerToken()
    }
    func registerToken() {
        guard let token, connected, !registrationPending else { return }
        let signature = token.base64EncodedString() + ":" + String(tokenInvalidated) + ":" + domains.keys.sorted().joined(separator: ",")
        guard signature != lastRegistration else { return }
        registrationPending = true
        let request: [String: Any] = ["op": "push_token",
            "token": token.map { String(format: "%02x", $0) }.joined(),
            "invalidated": tokenInvalidated,
            "bundle": Bundle.main.bundleIdentifier ?? "",
            "sandbox": Bundle.main.object(forInfoDictionaryKey: "PlaybitPushEnvironment") as? String == "development"]
        DispatchQueue.global(qos: .utility).async {
            var success = false
            do {
                let process = Process(), input = Pipe(), output = Pipe()
                process.executableURL = self.bridge
                process.standardInput = input
                process.standardOutput = output
                try process.run()
                try input.fileHandleForWriting.write(contentsOf: JSONSerialization.data(withJSONObject: request))
                try input.fileHandleForWriting.close()
                let data = try output.fileHandleForReading.readToEnd() ?? Data()
                process.waitUntilExit()
                if data.count <= 524288, let result = try JSONSerialization.jsonObject(with: data) as? [String: Any] {
                    success = result["error"] == nil
                    if success { NSLog("File Provider push registration saved (delivery enabled: %@)", String(describing: result["registered"] as? Bool ?? false)) }
                    else { NSLog("File Provider push registration remains pending") }
                }
            } catch { fputs("File Provider push registration: \(error)\n", stderr) }
            let registered = success
            DispatchQueue.main.async {
                self.registrationPending = false
                if registered { self.lastRegistration = signature }
                let latest = self.token.map { $0.base64EncodedString() + ":" + String(self.tokenInvalidated) + ":" + self.domains.keys.sorted().joined(separator: ",") }
                if registered || latest != signature { self.registerToken() }
            }
        }
    }
}
@_cdecl("playbit_run_file_provider")
public func runFileProvider() -> Int32 {
    let bridge = Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/playbit-file-provider")
    let drives = Drives(bridge: bridge)
    do { try drives.start(); RunLoop.main.run(); return 0 }
    catch { NSLog("File Provider: %@", error.localizedDescription); return 1 }
}

/// Private owning-app maintenance. Discovery never changes domains; retirement
/// requires an exact legacy identifier and profile and preserves downloaded data.
enum FileProviderManagement {
    static let profileBytesMax = 4096
    static let domainBytesMax = 45 // Two canonical 16-byte IDs and one separator.
    static let domainCountMax = 257 // 256 legacy volume locations plus the account drive.
    static let replyBytesMax = 524288
    static let operationSecondsMax: TimeInterval = 30

    struct Failure: LocalizedError {
        let message: String
        var errorDescription: String? { message }
    }

    static func legacy(_ identifier: String) -> Bool {
        guard identifier.utf8.count <= domainBytesMax else { return false }
        let parts = identifier.split(separator: "-", omittingEmptySubsequences: false)
        return parts.count == 2 && parts.allSatisfy { Identifier.valid(String($0)) }
    }

    static func selection(_ identifier: String, profile: String,
        domains: [NSFileProviderDomain]) throws -> NSFileProviderDomain {
        guard legacy(identifier) else {
            throw Failure(message: "Retirement requires a canonical legacy ACCOUNT-VOLUME domain identifier")
        }
        let matches = domains.filter { $0.identifier.rawValue == identifier }
        guard matches.count == 1, let domain = matches.first,
            domain.userInfo?["profile"] as? String == profile else {
            throw Failure(message: "No matching legacy File Provider domain belongs to the supplied profile")
        }
        return domain
    }

    static func input(_ pointer: UnsafePointer<CChar>, bytesMax: Int) -> String? {
        let count = strnlen(pointer, bytesMax + 1)
        guard count > 0, count <= bytesMax else { return nil }
        return String(validatingUTF8: pointer)
    }

    final class Operation {
        var result: Result<Any, Error>?
        var removalStarted = false
        let deadline = ProcessInfo.processInfo.systemUptime + operationSecondsMax
        let profile: String
        let selected: String?
        init(profile: String, selected: String?) { self.profile = profile; self.selected = selected }

        func start() {
            NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
                DispatchQueue.main.async {
                    guard self.result == nil else { return }
                    guard ProcessInfo.processInfo.systemUptime < self.deadline else {
                        self.timeOut(); CFRunLoopStop(CFRunLoopGetMain()); return
                    }
                    if let error { self.result = .failure(error); CFRunLoopStop(CFRunLoopGetMain()); return }
                    do {
                        guard domains.count <= domainCountMax else {
                            throw Failure(message: "File Provider domain count exceeds the management limit")
                        }
                        if let selected = self.selected {
                            let domain = try selection(selected, profile: self.profile, domains: domains)
                            self.removalStarted = true
                            NSFileProviderManager.remove(domain, mode: .preserveDownloadedUserData) { url, error in
                                DispatchQueue.main.async {
                                    guard self.result == nil else { return }
                                    if let error { self.result = .failure(error) }
                                    else {
                                        self.result = .success(["id": selected,
                                            "preserved_path": url.map { $0.path as Any } ?? NSNull()])
                                    }
                                    CFRunLoopStop(CFRunLoopGetMain())
                                }
                            }
                            return
                        }
                        let rows: [[String: Any]] = domains
                            .filter { $0.userInfo?["profile"] as? String == self.profile }
                            .map { ["id": $0.identifier.rawValue, "name": $0.displayName,
                                "legacy": legacy($0.identifier.rawValue)] }
                        self.result = .success(rows)
                    } catch { self.result = .failure(error) }
                    CFRunLoopStop(CFRunLoopGetMain())
                }
            }
        }

        func timeOut() {
            let message = removalStarted
                ? "File Provider retirement timed out; completion is unknown. Do not retry automatically; inspect the domain and preserved files first"
                : "File Provider discovery timed out; no retirement was requested"
            result = .failure(Failure(message: message))
        }

        func run() throws -> Any {
            // Keep the mode alive until a completion or the finite deadline.
            let port = Port()
            RunLoop.main.add(port, forMode: .default)
            defer { RunLoop.main.remove(port, forMode: .default); port.invalidate() }
            start()
            CFRunLoopRunInMode(.defaultMode, max(0, deadline - ProcessInfo.processInfo.systemUptime), false)
            if result == nil { timeOut() }
            guard let result else { throw Failure(message: "File Provider management did not complete") }
            return try result.get()
        }
    }
}

@_cdecl("playbit_manage_file_provider")
public func manageFileProvider(_ profilePointer: UnsafePointer<CChar>,
    _ domainPointer: UnsafePointer<CChar>?) -> Int32 {
    do {
        guard Thread.isMainThread else {
            throw FileProviderManagement.Failure(message: "File Provider management must run on the main thread")
        }
        guard let profile = FileProviderManagement.input(profilePointer,
            bytesMax: FileProviderManagement.profileBytesMax), profile.hasPrefix("/"),
            URL(fileURLWithPath: profile).standardizedFileURL.path == profile else {
            throw FileProviderManagement.Failure(message: "File Provider management requires a canonical absolute profile path of at most 4096 bytes")
        }
        var selected: String?
        if let domainPointer {
            guard let domain = FileProviderManagement.input(domainPointer,
                bytesMax: FileProviderManagement.domainBytesMax), FileProviderManagement.legacy(domain) else {
                throw FileProviderManagement.Failure(message: "Retirement requires a canonical legacy ACCOUNT-VOLUME domain identifier")
            }
            selected = domain
        }
        let operation = FileProviderManagement.Operation(profile: profile, selected: selected)
        let value = try operation.run()
        var data = try JSONSerialization.data(withJSONObject: value, options: [.sortedKeys])
        guard data.count <= FileProviderManagement.replyBytesMax else {
            throw FileProviderManagement.Failure(message: "File Provider management reply exceeds 256 KiB")
        }
        data.append(10)
        try FileHandle.standardOutput.write(contentsOf: data)
        return 0
    } catch {
        fputs("File Provider management: \(error.localizedDescription)\n", stderr)
        return 1
    }
}

/// The containing app resolves stable item identities through the registered
/// domain. Finder owns opening; no path is reconstructed from display names.
private enum VolumeOpening {
    static let errorBytesMax = 4096

    static func complete(_ context: UnsafeMutableRawPointer?,
        _ completion: @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void,
        _ error: Error?) {
        guard let error else { completion(context, nil); return }
        var bytes = Array(error.localizedDescription.utf8.prefix(errorBytesMax))
        bytes.append(0)
        bytes.withUnsafeBytes { buffer in
            completion(context, buffer.baseAddress?.assumingMemoryBound(to: CChar.self))
        }
    }

    static func selection(profile: String, account: String,
        domains: [NSFileProviderDomain]) throws -> NSFileProviderDomain {
        guard domains.count <= FileProviderManagement.domainCountMax else {
            throw FileProviderManagement.Failure(message: "File Provider domain count exceeds the management limit")
        }
        let matching = domains.filter {
            $0.identifier.rawValue == account && $0.userInfo?["profile"] as? String == profile
        }
        guard matching.count == 1, let domain = matching.first else {
            throw FileProviderManagement.Failure(message: "This account’s Playbit drive is unavailable in Finder. Enable its File Provider location, then try again.")
        }
        return domain
    }

    static func open(profile: String, account: String, item: String, reveal: Bool = false,
        context: UnsafeMutableRawPointer?,
        completion: @escaping @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void) {
        NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
            DispatchQueue.main.async {
                if let error { complete(context, completion, error); return }
                do {
                    let domain = try selection(profile: profile, account: account, domains: domains)
                    guard let manager = NSFileProviderManager(for: domain) else {
                        throw FileProviderManagement.Failure(message: "This account’s Playbit drive is unavailable in Finder.")
                    }
                    manager.getUserVisibleURL(for: NSFileProviderItemIdentifier(item)) { url, error in
                        DispatchQueue.main.async {
                            if let error { complete(context, completion, error); return }
                            guard let url else {
                                complete(context, completion, FileProviderManagement.Failure(message: "The item is unavailable in Finder.")); return
                            }
                            let scoped = url.startAccessingSecurityScopedResource()
                            if reveal {
                                NSWorkspace.shared.activateFileViewerSelecting([url])
                                if scoped { url.stopAccessingSecurityScopedResource() }
                                complete(context, completion, nil)
                                return
                            }
                            NSWorkspace.shared.open(url, configuration: NSWorkspace.OpenConfiguration()) { _, error in
                                if scoped { url.stopAccessingSecurityScopedResource() }
                                complete(context, completion, error)
                            }
                        }
                    }
                } catch { complete(context, completion, error) }
            }
        }
    }
}

@_cdecl("playbit_open_volume")
public func openVolume(_ profilePointer: UnsafePointer<CChar>,
    _ accountPointer: UnsafePointer<CChar>, _ volumePointer: UnsafePointer<CChar>,
    _ context: UnsafeMutableRawPointer?,
    _ completion: @escaping @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void) {
    do {
        guard Thread.isMainThread else {
            throw FileProviderManagement.Failure(message: "Open in Finder must start on the main thread")
        }
        guard let profile = FileProviderManagement.input(profilePointer,
            bytesMax: FileProviderManagement.profileBytesMax), profile.hasPrefix("/"),
            URL(fileURLWithPath: profile).standardizedFileURL.path == profile,
            let account = FileProviderManagement.input(accountPointer, bytesMax: 22), Identifier.valid(account),
            let volume = FileProviderManagement.input(volumePointer, bytesMax: 22), Identifier.valid(volume) else {
            throw FileProviderManagement.Failure(message: "Open in Finder requires a canonical profile and account and volume identities")
        }
        VolumeOpening.open(profile: profile, account: account, item: volume,
            context: context, completion: completion)
    } catch { VolumeOpening.complete(context, completion, error) }
}

/// Open a stable account-domain entry with its default app, or select it in Finder.
private func openEntry(_ profilePointer: UnsafePointer<CChar>,
    _ accountPointer: UnsafePointer<CChar>, _ itemPointer: UnsafePointer<CChar>,
    reveal: Bool,
    _ context: UnsafeMutableRawPointer?,
    _ completion: @escaping @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void) {
    do {
        guard Thread.isMainThread else {
            throw FileProviderManagement.Failure(message: "Opening an item must start on the main thread")
        }
        guard let profile = FileProviderManagement.input(profilePointer,
            bytesMax: FileProviderManagement.profileBytesMax), profile.hasPrefix("/"),
            URL(fileURLWithPath: profile).standardizedFileURL.path == profile,
            let account = FileProviderManagement.input(accountPointer, bytesMax: 22), Identifier.valid(account),
            let item = FileProviderManagement.input(itemPointer, bytesMax: 43),
            Identifier.decode(item, bytes: 31) != nil else {
            throw FileProviderManagement.Failure(message: "Opening an item requires a canonical profile, account and item identity")
        }
        VolumeOpening.open(profile: profile, account: account, item: item, reveal: reveal,
            context: context, completion: completion)
    } catch { VolumeOpening.complete(context, completion, error) }
}

@_cdecl("playbit_open_entry")
public func openEntryWithDefaultApp(_ profilePointer: UnsafePointer<CChar>,
    _ accountPointer: UnsafePointer<CChar>, _ itemPointer: UnsafePointer<CChar>,
    _ context: UnsafeMutableRawPointer?,
    _ completion: @escaping @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void) {
    openEntry(profilePointer, accountPointer, itemPointer, reveal: false, context, completion)
}

/// Select a stable account-domain entry in Finder without reconstructing its filename.
@_cdecl("playbit_reveal_entry")
public func revealEntry(_ profilePointer: UnsafePointer<CChar>,
    _ accountPointer: UnsafePointer<CChar>, _ itemPointer: UnsafePointer<CChar>,
    _ context: UnsafeMutableRawPointer?,
    _ completion: @escaping @convention(c) (UnsafeMutableRawPointer?, UnsafePointer<CChar>?) -> Void) {
    openEntry(profilePointer, accountPointer, itemPointer, reveal: true, context, completion)
}
