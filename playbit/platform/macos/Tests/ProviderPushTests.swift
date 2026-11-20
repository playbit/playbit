import Foundation
import FileProvider

@main
struct ProviderPushTests {
    static func main() throws {
        // Retirement selects one canonical legacy domain owned by this profile.
        // Current, malformed, foreign-profile and unknown domains stay ineligible.
        let account = Identifier.encode([UInt8](repeating: 1, count: 16))
        let volume = Identifier.encode([UInt8](repeating: 2, count: 16))
        let legacy = account + "-" + volume
        let profile = "/tmp/playbit-provider-management"
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(legacy), displayName: "Old Playbit")
        domain.userInfo = ["profile": profile]
        let current = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(account), displayName: "Playbit")
        current.userInfo = ["profile": profile]
        let foreign = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(legacy), displayName: "Other Playbit")
        foreign.userInfo = ["profile": profile + "-other"]
        let malformed = [account, "", account + "-", "-" + volume, legacy + "-" + volume,
            account + "-0", "1" + account + "-" + volume]
        precondition(FileProviderManagement.legacy(legacy))
        let selected = try FileProviderManagement.selection(legacy, profile: profile, domains: [current, domain])
        precondition(selected === domain)
        for identifier in malformed {
            precondition(!FileProviderManagement.legacy(identifier))
            do {
                _ = try FileProviderManagement.selection(identifier, profile: profile, domains: [domain, current])
                preconditionFailure("Ineligible identifier was selected")
            } catch {}
        }
        for domains in [[foreign], [current], [domain, foreign]] {
            do {
                _ = try FileProviderManagement.selection(legacy, profile: profile, domains: domains)
                preconditionFailure("Unknown, foreign or ambiguous domain was selected")
            } catch {}
        }
        // A token callback arriving behind a failed registration must dispatch
        // its newer state. Run the actual coordinator/bridge completion path.
        for invalidated in [false, true] {
            let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
            try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
            defer { try? FileManager.default.removeItem(at: root) }
            let captured = root.appendingPathComponent("requests.jsonl")
            let bridge = root.appendingPathComponent("bridge")
            let script = """
            #!/usr/bin/python3
            import json, sys
            request = json.loads(sys.stdin.buffer.read(65537))
            with open(\(String(reflecting: captured.path)), 'a') as output:
                output.write(json.dumps(request) + '\\n')
            failed = request['token'] == '01' and not request['invalidated']
            print(json.dumps({'error': 'held failure'} if failed else {'registered': True}))
            """
            try Data(script.utf8).write(to: bridge)
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: bridge.path)
            let drives = Drives(bridge: bridge)
            drives.connected = true
            drives.token = Data([1])
            drives.registerToken()
            precondition(drives.registrationPending)
            drives.token = Data([invalidated ? 1 : 2])
            drives.tokenInvalidated = invalidated
            drives.registerToken()
            let deadline = Date().addingTimeInterval(5)
            while drives.lastRegistration == nil && Date() < deadline {
                RunLoop.current.run(until: Date().addingTimeInterval(0.01))
            }
            precondition(drives.lastRegistration != nil)
            let lines = try String(contentsOf: captured, encoding: .utf8).split(separator: "\n")
            precondition(lines.count == 2)
            let latest = try JSONSerialization.jsonObject(with: Data(lines[1].utf8)) as! [String: Any]
            precondition(latest["token"] as? String == (invalidated ? "01" : "02"))
            precondition(latest["invalidated"] as? Bool == invalidated)
        }
        print("File Provider retirement selection, newer-token and invalidation completion races passed")
    }
}
