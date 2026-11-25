import Foundation
import Darwin

@objc protocol ProviderService {
    func call(_ request: Data, content: FileHandle?, reply: @escaping (Data) -> Void)
}

/// This unsandboxed, same-user XPC service is embedded in the extension. It has
/// no store or credentials: the packaged bridge uses authenticated engine IPC.
final class Service: NSObject, ProviderService, NSXPCListenerDelegate {
    let queue = OperationQueue()
    let admissions = DispatchSemaphore(value: 8)
    override init() { super.init(); queue.maxConcurrentOperationCount = 4 }
    func listener(_ listener: NSXPCListener, shouldAcceptNewConnection connection: NSXPCConnection) -> Bool {
        guard let requirement = Bundle.main.object(forInfoDictionaryKey: "PlaybitProviderClientRequirement") as? String else { return false }
        connection.setCodeSigningRequirement(requirement)
        connection.exportedInterface = NSXPCInterface(with: ProviderService.self)
        connection.exportedObject = self
        connection.resume()
        return true
    }
    func call(_ request: Data, content: FileHandle?, reply: @escaping (Data) -> Void) {
        guard request.count <= 65536, admissions.wait(timeout: .now()) == .success else {
            try? content?.close()
            reply(Data(#"{"error":"File Provider is busy"}"#.utf8)); return
        }
        queue.addOperation {
            defer { self.admissions.signal() }
            defer { try? content?.close() }
            var temporary: URL?
            defer { if let temporary { try? FileManager.default.removeItem(at: temporary) } }
            do {
                guard var fields = try JSONSerialization.jsonObject(with: request) as? [String: Any],
                    let profile = fields.removeValue(forKey: "profile") as? String,
                    profile.hasPrefix("/"), profile.utf8.count <= 4096 else {
                    throw CocoaError(.fileReadInvalidFileName)
                }
                // Requests from the original account-only extension are Public.
                let channel = fields.removeValue(forKey: "channel") ?? "public"
                guard let channel = channel as? String, ["public", "testing"].contains(channel) else {
                    throw CocoaError(.fileReadCorruptFile)
                }
                guard fields["path"] == nil else { throw CocoaError(.fileReadInvalidFileName) }
                let fetching = fields["op"] as? String == "fetch"
                if let content {
                    let directory = FileManager.default.temporaryDirectory.appendingPathComponent("playbit-file-provider-" + UUID().uuidString)
                    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false,
                        attributes: [.posixPermissions: 0o700])
                    temporary = directory
                    let path = directory.appendingPathComponent("content")
                    fields["path"] = path.path
                    if !fetching {
                        guard FileManager.default.createFile(atPath: path.path, contents: nil,
                            attributes: [.posixPermissions: 0o600]) else { throw CocoaError(.fileWriteUnknown) }
                        let destination = try FileHandle(forWritingTo: path)
                        defer { try? destination.close() }
                        try Self.copy(content, to: destination)
                    }
                } else if fetching { throw CocoaError(.fileWriteInvalidFileName) }
                var app = Bundle.main.bundleURL
                for _ in 0..<8 {
                    if FileManager.default.fileExists(atPath: app.appendingPathComponent("Contents/MacOS/playbit-file-provider").path) { break }
                    app.deleteLastPathComponent()
                }
                let process = Process()
                process.executableURL = app.appendingPathComponent("Contents/MacOS/playbit-file-provider")
                process.arguments = ["--domain", channel, profile]
                var environment = ProcessInfo.processInfo.environment
                environment.removeValue(forKey: "PLAYBIT_PROFILE")
                environment.removeValue(forKey: "PLAYBIT_CHANNEL")
                process.environment = environment
                let input = Pipe(), output = Pipe()
                process.standardInput = input; process.standardOutput = output
                try process.run()
                try input.fileHandleForWriting.write(contentsOf: JSONSerialization.data(withJSONObject: fields))
                try input.fileHandleForWriting.close()
                let result = try output.fileHandleForReading.readToEnd() ?? Data()
                process.waitUntilExit()
                guard result.count <= 524288, process.terminationStatus == 0 else {
                    throw CocoaError(.xpcConnectionReplyInvalid)
                }
                if fetching, let content, let path = fields["path"] as? String,
                    let value = try JSONSerialization.jsonObject(with: result) as? [String: Any], value["error"] == nil {
                    let source = try FileHandle(forReadingFrom: URL(fileURLWithPath: path))
                    defer { try? source.close() }
                    try Self.copy(source, to: content)
                    try content.synchronize()
                }
                reply(result)
            } catch {
                let response = ["error": error.localizedDescription]
                reply((try? JSONSerialization.data(withJSONObject: response)) ?? Data())
            }
        }
    }
    static func copy(_ source: FileHandle, to destination: FileHandle) throws {
        // Match the shared file ABI's 64 MiB extent; never accumulate contents.
        var metadata = stat()
        guard fstat(source.fileDescriptor, &metadata) == 0 else {
            throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
        }
        guard metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG) else {
            throw CocoaError(.fileReadUnsupportedScheme)
        }
        guard metadata.st_size >= 0 && metadata.st_size <= 64 * 1024 * 1024 else {
            throw CocoaError(.fileReadTooLarge)
        }
        var length = 0
        while let bytes = try source.read(upToCount: 65536), !bytes.isEmpty {
            length += bytes.count
            guard length <= 64 * 1024 * 1024 else { throw CocoaError(.fileReadTooLarge) }
            try destination.write(contentsOf: bytes)
        }
    }
}
let service = Service()
let listener = NSXPCListener.service()
listener.delegate = service
listener.resume()
