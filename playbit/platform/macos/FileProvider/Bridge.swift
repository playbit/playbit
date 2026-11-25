import Foundation
import FileProvider
import Darwin

@objc protocol ProviderService {
    func call(_ request: Data, content: FileHandle?, reply: @escaping (Data) -> Void)
}

final class ProviderCompletion {
    let lock = NSLock()
    var completed = false
    var content: FileHandle?
    let completion: (Result<[String: Any], Error>) -> Void
    init(_ completion: @escaping (Result<[String: Any], Error>) -> Void) { self.completion = completion }
    func finish(_ result: Result<[String: Any], Error>) {
        lock.lock()
        if completed { lock.unlock(); return }
        completed = true
        lock.unlock()
        try? content?.close()
        completion(result)
    }
}

final class ProviderClient {
    let uploads = ProviderUploads()
    static let admissions = DispatchSemaphore(value: 8)
    let connection: NSXPCConnection
    let profile: String
    let identity: DriveIdentity?
    init(domain: NSFileProviderDomain) {
        identity = DriveIdentity(domain.identifier)
        profile = domain.userInfo?["profile"] as? String ?? ""
        let service = Bundle.main.object(forInfoDictionaryKey: "PlaybitProviderService") as? String ?? ""
        connection = NSXPCConnection(serviceName: service)
        if let requirement = Bundle.main.object(forInfoDictionaryKey: "PlaybitProviderServiceRequirement") as? String {
            connection.setCodeSigningRequirement(requirement)
        }
        connection.remoteObjectInterface = NSXPCInterface(with: ProviderService.self)
        connection.resume()
    }
    deinit { connection.invalidate() }
    func invalidate() { uploads.invalidate(); connection.invalidate() }
    func call(_ fields: [String: Any], completion: @escaping (Result<[String: Any], Error>) -> Void) {
        guard let identity else {
            completion(.failure(NSFileProviderError(.noSuchItem))); return
        }
        guard Self.admissions.wait(timeout: .now()) == .success else {
            completion(.failure(NSFileProviderError(.serverUnreachable))); return
        }
        let delivery = ProviderCompletion { result in
            Self.admissions.signal()
            self.uploads.observe(result)
            completion(result)
        }
        var request = fields
        request["account"] = identity.account
        request["profile"] = profile
        request["channel"] = identity.channel.rawValue
        do {
            if let path = request.removeValue(forKey: "path") as? String {
                let fetching = request["op"] as? String == "fetch"
                let flags = fetching ? O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW : O_RDONLY | O_NONBLOCK | O_NOFOLLOW
                let descriptor = open(path, flags, 0o600)
                guard descriptor >= 0 else { throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO) }
                delivery.content = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
                var metadata = stat()
                guard fstat(descriptor, &metadata) == 0 else {
                    throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
                }
                guard metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
                    throw CocoaError(.fileReadUnsupportedScheme)
                }
                if !fetching && (metadata.st_size < 0 || metadata.st_size > 64 * 1024 * 1024) {
                    throw CocoaError(.fileReadTooLarge)
                }
            }
            let data = try JSONSerialization.data(withJSONObject: request)
            guard data.count <= 65536 else { throw CocoaError(.fileReadTooLarge) }
            let proxy = connection.remoteObjectProxyWithErrorHandler { delivery.finish(.failure($0)) } as? ProviderService
            guard let proxy else { throw CocoaError(.xpcConnectionInvalid) }
            proxy.call(data, content: delivery.content) { bytes in
                do {
                    guard bytes.count <= 524288,
                        let value = try JSONSerialization.jsonObject(with: bytes) as? [String: Any] else {
                        throw CocoaError(.xpcConnectionReplyInvalid)
                    }
                    if let message = value["error"] as? String {
                        if value["code"] as? String == "permission_denied" {
                            let reading = ["root", "volumes", "catalog", "volume_item", "list", "scan", "item", "fetch", "anchor"].contains(fields["op"] as? String ?? "")
                            throw NSError(domain: NSCocoaErrorDomain,
                                code: reading ? CocoaError.fileReadNoPermission.rawValue : CocoaError.fileWriteNoPermission.rawValue,
                                userInfo: [NSLocalizedDescriptionKey: message])
                        }
                        if value["code"] as? String == "invalid_input" {
                            throw NSError(domain: NSCocoaErrorDomain, code: CocoaError.fileWriteInvalidFileName.rawValue,
                                userInfo: [NSLocalizedDescriptionKey: message])
                        }
                        if message.contains("provider directory not empty") {
                            throw NSFileProviderError(.directoryNotEmpty)
                        }
                        if message.contains("version changed"), fields["op"] as? String != "fetch" {
                            throw NSError(domain: NSCocoaErrorDomain, code: CocoaError.fileWriteFileExists.rawValue,
                                userInfo: [NSLocalizedDescriptionKey: "This item changed in Playbit. Your local changes have been kept; copy them to a new file to resolve the conflict."])
                        }
                        if message.contains("denied"), !message.contains("signed in") {
                            let reading = ["list", "scan", "item", "fetch", "anchor"].contains(fields["op"] as? String ?? "")
                            throw CocoaError(reading ? .fileReadNoPermission : .fileWriteNoPermission)
                        }
                        let code: NSFileProviderError.Code
                        if message.contains("not found") { code = .noSuchItem }
                        else if message.contains("version changed") { code = .versionNoLongerAvailable }
                        else if message.contains("signed in") { code = .notAuthenticated }
                        else if message.contains("already exists") {
                            throw CocoaError(.fileWriteFileExists)
                        }
                        else { code = .serverUnreachable }
                        throw NSError(domain: NSFileProviderErrorDomain, code: code.rawValue,
                            userInfo: [NSLocalizedDescriptionKey: message])
                    }
                    delivery.finish(.success(value))
                } catch { delivery.finish(.failure(error)) }
            }
        } catch { delivery.finish(.failure(error)) }
    }
}
