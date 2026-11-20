// Compile with Host/EngineHost.swift on macOS. The C bridge is a controlled fault
// fixture; these tests verify Swift admission/liveness, not runtime performance.
import Foundation
import Darwin

private final class BridgeFixture {
    static let shared = BridgeFixture()
    let lock = NSLock()
    let completion = DispatchSemaphore(value: 0)
    var pipeFD: [Int32] = [0, 0]
    var requests = 0
    var acknowledgments = 0
    var completions = 0
    var fail = false
    var immediate = false
    var stopFailure = false
    var opened: [(String, UInt8)] = []
    var closes = 0
    var stops = 0
    var credentialClears = 0
    var scenes: [OpaquePointer: SceneFixture] = [:]
    var replyCompletions: [OpaquePointer: IosCompletion] = [:]
    var generations: [OpaquePointer: UnsafeMutablePointer<UInt8>] = [:]
    var databases: [OpaquePointer: (UnsafeMutablePointer<UInt8>, Int)] = [:]
    init() { precondition(pipe(&pipeFD) == 0) }
    func read<T>(_ action: (BridgeFixture) -> T) -> T {
        lock.lock(); defer { lock.unlock() }; return action(self)
    }
    func notify() { var byte: UInt8 = 1; precondition(write(pipeFD[1], &byte, 1) == 1) }
}
private func reply(payload: [String: Any] = [:], id: UInt64? = nil, error: String? = nil, ownerStep: Bool = false) -> OpaquePointer? {
    let pointer = OpaquePointer(malloc(1))!
    BridgeFixture.shared.read {
        $0.replyCompletions[pointer] = IosCompletion(request_id: id ?? 0, has_response: id == nil ? 0 : 1,
            error_code: error == nil ? 0 : 1, error: error.flatMap { strdup($0) }.map { UnsafePointer($0) }, background_error: nil)
        if payload["first"] as? Bool == true {
            $0.scenes[pointer] = SceneFixture(kind: 3, text: "Native scene")
        }
        if ownerStep {
            let generation = UnsafeMutablePointer<UInt8>.allocate(capacity: 16)
            generation.initialize(repeating: 7, count: 16)
            $0.generations[pointer] = generation
        }
    }
    return pointer
}
private func step(_ response: [String: Any]? = nil) -> OpaquePointer? {
    reply(payload: response?["data"] as? [String: Any] ?? [:],
        id: response?["id"] as? UInt64, error: response?["error"] as? String, ownerStep: true)
}
func pb_ios_open(_ profile: UnsafePointer<CChar>, _ channel: UInt8, _ output: UnsafeMutablePointer<OpaquePointer?>) -> OpaquePointer? {
    BridgeFixture.shared.read { $0.opened.append((String(cString: profile), channel)) }
    output.pointee = OpaquePointer(bitPattern: 1)
    return reply()
}
func submitFixture(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    let first = BridgeFixture.shared.read { fixture in fixture.requests += 1; return fixture.requests == 1 || fixture.immediate }
    return first ? step(["id": id, "ok": true, "data": ["first": true]]) : step()
}
func pb_ios_app_input(_ handle: OpaquePointer?, _ id: UInt64, _ instance: UnsafePointer<UInt8>?, _ instanceLen: Int,
                  _ kind: UInt32, _ target: UInt32, _ data: UnsafePointer<UInt8>?, _ dataLen: Int) -> OpaquePointer? {
    precondition(Identifier.decode(String(decoding: UnsafeBufferPointer(start: instance, count: instanceLen), as: UTF8.self), bytes: 16) == [UInt8](repeating: 0xaa, count: 16) && kind == 14 && target == 7)
    precondition(Array(UnsafeBufferPointer(start: data, count: dataLen)) == Array(repeating: 65, count: GuestTextEdits.payloadBytesMax))
    return submitFixture(handle, id)
}
func pb_ios_status(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_accept_agreement(_ handle: OpaquePointer?, _ id: UInt64, _ account: UnsafePointer<UInt8>?, _ len: Int) -> OpaquePointer? { submitFixture(handle, id) }
struct DatabaseBytes {
    var bytes: UnsafePointer<UInt8>?
    var len: Int = 0
}
func pb_ios_database(_ handle: OpaquePointer?, _ id: UInt64, _ bytes: UnsafePointer<UInt8>?, _ len: Int) -> OpaquePointer? {
    precondition(Array(UnsafeBufferPointer(start: bytes, count: len)) == Array("{\"op\":\"volumes\"}".utf8))
    let payload = try! JSONSerialization.data(withJSONObject: ["name": "é\0🔑"])
    let storage = UnsafeMutablePointer<UInt8>.allocate(capacity: payload.count)
    payload.copyBytes(to: storage, count: payload.count)
    let pointer = reply(id: id, ownerStep: true)!
    BridgeFixture.shared.read { $0.requests += 1; $0.databases[pointer] = (storage, payload.count) }
    return pointer
}
func pb_ios_reply_database(_ pointer: OpaquePointer) -> DatabaseBytes {
    BridgeFixture.shared.read {
        guard let (bytes, len) = $0.databases[pointer] else { return DatabaseBytes(bytes: nil) }
        return DatabaseBytes(bytes: UnsafePointer(bytes), len: len)
    }
}
func pb_ios_saved_account_available() -> UInt8 { 0 }
func pb_ios_reset_profile(_ profile: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? { nil }
func pb_ios_saved_account(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? { submitFixture(handle, id) }
func pb_ios_retry_account_key_backup(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? { submitFixture(handle, id) }
func pb_ios_prepare_sign_in(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_transfer_status(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_cancel_transfer(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_recovery_status(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_prepare_recovery(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_save_recovery(_ handle: OpaquePointer?, _ id: UInt64) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_prepare_session(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_app_frame(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_open_app(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_quit_app(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_check_updates(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_personal_volumes(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_invitation(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_forget_session(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_watch_invitation(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_resume_invitation(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_pause_invitation(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_start_join(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_preview_invitation(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_set_display_name(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_sign_in(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_request_code(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_recover_credentials(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? {
    submitFixture(handle, id)
}
func pb_ios_completion_descriptor(_ handle: OpaquePointer?, _ output: UnsafeMutablePointer<Int32>) -> UnsafeMutablePointer<CChar>? {
    output.pointee = dup(BridgeFixture.shared.pipeFD[0])
    return nil
}
func pb_ios_complete_background(_ handle: OpaquePointer?) -> OpaquePointer? {
    let fixture = BridgeFixture.shared
    let failed = fixture.read { value in value.completions += 1; return value.fail }
    fixture.completion.signal()
    // Poisoning leaves the descriptor readable. The host must cancel its
    // source rather than spinning forever or leaving callbacks pending.
    if failed { return reply(error: "runtime lock poisoned") }
    var byte: UInt8 = 0
    precondition(read(fixture.pipeFD[0], &byte, 1) == 1)
    return step()
}
func pb_ios_delivery_ready(_ handle: OpaquePointer?) -> OpaquePointer? {
    BridgeFixture.shared.read { $0.acknowledgments += 1 }; return step()
}
func pb_ios_wake(_ handle: OpaquePointer?) -> OpaquePointer? { step() }
func pb_ios_suspend(_ handle: OpaquePointer?) -> UnsafeMutablePointer<CChar>? { nil }
func pb_ios_resume(_ handle: OpaquePointer?) -> OpaquePointer? { step() }
func pb_ios_close(_ handle: OpaquePointer?) { BridgeFixture.shared.read { $0.closes += 1 } }
func pb_ios_stop_for_replacement(_ handle: OpaquePointer?) -> UnsafeMutablePointer<CChar>? { nil }
func pb_ios_stop_for_sign_out(_ handle: OpaquePointer?) -> UnsafeMutablePointer<CChar>? {
    BridgeFixture.shared.read { $0.stops += 1; return $0.stopFailure ? strdup("Held accepted work") : nil }
}
func pb_ios_keep_profile(_ profile: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    BridgeFixture.shared.read { $0.credentialClears += 1 }; return nil
}
func pb_ios_resolve_replacement(_ profile: UnsafePointer<CChar>, _ replace: UInt8) -> UnsafeMutablePointer<CChar>? { nil }
func pb_ios_string_free(_ value: UnsafeMutablePointer<CChar>) { free(value) }
let PB_IOS_TRACE_WORKER_DELIVERY: UInt32 = 8
func pb_ios_trace_start() -> UInt64 { 0 }
func pb_ios_trace_end(_ stage: UInt32, _ source: UInt64) {}

@main enum EngineHostTests {
    static func until(_ predicate: () -> Bool) {
        let deadline = Date().addingTimeInterval(5)
        while !predicate() {
            precondition(Date() < deadline, "host callback did not arrive")
            RunLoop.main.run(until: Date().addingTimeInterval(0.005))
        }
    }
    static func main() {
        let referenceCode = Identifier.encode([UInt8](repeating: 5, count: 16))
        let copiedStatus = Array(referenceCode.utf8).withUnsafeBufferPointer { reference in
            Array("YcVfxkQb6JRzqk5kF2tNLv".utf8).withUnsafeBufferPointer { account in
                Array("é\0🔑".utf8).withUnsafeBufferPointer { bytes in
                    Array(repeating: UInt8(255), count: 16).withUnsafeBufferPointer { id -> [String: Any] in
                        let text = StatusText(bytes: bytes.baseAddress, len: bytes.count)
                        statusFields = StatusFields(account: StatusText(bytes: account.baseAddress, len: account.count), enrolled: 1, unlocked: 1, initialized: 1, agreement_required: 1,
                            display_name: text, sessions: 1, volumes: 1, examples: 1, invitations: 1)
                        statusSession = SessionFields(id: text, build: text, root: id.baseAddress, name: text, build_kind: text, build_message: text, reference: StatusText(bytes: reference.baseAddress, len: reference.count))
                        statusVolume = PersonalVolumeRow(id: id.baseAddress, name: bytes.baseAddress, name_len: bytes.count)
                        statusExample = ExampleFields(id: text, name: text, description: text, invitation: text, source: text)
                        statusInvitation = InvitationFields(category: 1, paused: 1, session: bytes.baseAddress, session_len: bytes.count)
                        statusWatch = InvitationFields(category: 2, state: 3, session: bytes.baseAddress, session_len: bytes.count)
                        defer { statusFields = StatusFields(); statusSession = SessionFields(); statusVolume = PersonalVolumeRow();
                            statusExample = ExampleFields(); statusInvitation = InvitationFields(); statusWatch = InvitationFields() }
                        let value = try! EngineHost.decodeStatus(OpaquePointer(bitPattern: 1)!)!
                        statusSession.build_message = StatusText()
                        let ready = try! EngineHost.decodeStatus(OpaquePointer(bitPattern: 1)!)!
                        precondition((ready["sessions"] as? [[String: Any]])?.first?["build_message"] == nil,
                            "A ready build has no optional status message")
                        statusFields.sessions = 257
                        do { _ = try EngineHost.decodeStatus(OpaquePointer(bitPattern: 1)!); preconditionFailure("accepted excessive sessions") } catch {}
                        return value
                    }
                }
            }
        }
        precondition((copiedStatus["account_profile"] as? [String: Any])?["agreement_required"] as? Bool == true)
        precondition((copiedStatus["account_profile"] as? [String: Any])?["display_name"] as? String == "é\0🔑")
        precondition((copiedStatus["account"] as? [String: Any])?["id"] as? String == Identifier.encode([UInt8](repeating: 0xff, count: 16)))
        precondition((copiedStatus["sessions"] as? [[String: Any]])?.first?["app_name"] as? String == "é\0🔑")
        precondition((copiedStatus["sessions"] as? [[String: Any]])?.first?["reference"] as? String == referenceCode)
        precondition((copiedStatus["invitations"] as? [[String: Any]])?.first?["paused"] as? Bool == true)
        precondition(try! EngineHost.decodeStatus(OpaquePointer(bitPattern: 1)!) == nil)

        // An unrelated volume must not prevent the account's session list from loading.
        // Decode and copy before the native reply's borrowed storage is released.
        let longName = String(repeating: "é", count: 129)
        let longVolumeStatus = Array(longName.utf8).withUnsafeBufferPointer { name in
            Array(repeating: UInt8(49), count: 16).withUnsafeBufferPointer { id -> [String: Any] in
                statusFields = StatusFields(account: StatusText(bytes: id.baseAddress, len: 16), volumes: 1)
                statusVolume = PersonalVolumeRow(id: id.baseAddress, name: name.baseAddress, name_len: name.count)
                defer { statusFields = StatusFields(); statusVolume = PersonalVolumeRow() }
                return try! EngineHost.decodeStatus(OpaquePointer(bitPattern: 1)!)!
            }
        }
        precondition((longVolumeStatus["volumes"] as? [[String: Any]])?.first?["name"] as? String == longName)

        let consentPointer = OpaquePointer(bitPattern: 1)!
        let copiedConsent = Array("é\0🔑".utf8).withUnsafeBufferPointer { text in
            Array(UInt8(0)...UInt8(15)).withUnsafeBufferPointer { id -> [String: Any] in
                consentFields = PersonalVolumeFields(session: text.baseAddress, session_len: text.count, volumes: 1, grants: 1, has_grants: 1)
                consentRow = PersonalVolumeRow(id: id.baseAddress, name: text.baseAddress, name_len: text.count)
                consentGrant = PersonalGrant(id: id.baseAddress, write: 1)
                defer { consentFields = PersonalVolumeFields(); consentRow = PersonalVolumeRow(); consentGrant = PersonalGrant() }
                let value = try! EngineHost.decodePersonalVolumes(consentPointer)!
                consentFields.grants = 0; consentFields.has_grants = 0
                precondition(try! EngineHost.decodePersonalVolumes(consentPointer)?["grants"] is NSNull)
                consentFields.has_grants = 1
                precondition((try! EngineHost.decodePersonalVolumes(consentPointer)?["grants"] as? [[String: Any]])?.isEmpty == true)
                consentFields.volumes = 129
                for counts in [(129, 0), (1, 129)] {
                    consentFields.volumes = counts.0; consentFields.grants = counts.1
                    do { _ = try EngineHost.decodePersonalVolumes(consentPointer); preconditionFailure("accepted too many consent rows") }
                    catch { precondition(error.localizedDescription == PersonalVolumeAccess.Failure.tooLarge.localizedDescription) }
                }
                return value
            }
        }
        let consentVolumes = copiedConsent["volumes"] as! [[String: Any]]
        precondition(consentVolumes[0]["id"] as? String == Identifier.encode(Array(0...15)))
        precondition(consentVolumes[0]["name"] as? String == "é\0🔑")
        let consentGrants = copiedConsent["grants"] as! [[String: Any]]
        precondition(consentGrants[0]["write"] as? Bool == true)
        precondition(try! EngineHost.decodePersonalVolumes(consentPointer) == nil)

        let copiedInvitation = Array("é\0🔑".utf8).withUnsafeBufferPointer { bytes -> [String: Any] in
            var fields = InvitationFields(category: 1, paused: 1, retained: 0,
                session: bytes.baseAddress, session_len: bytes.count,
                root_name: bytes.baseAddress, root_name_len: bytes.count,
                build: bytes.baseAddress, build_len: bytes.count,
                message: bytes.baseAddress, message_len: bytes.count,
                invitation: bytes.baseAddress, invitation_len: bytes.count,
                availability: bytes.baseAddress, availability_len: bytes.count)
            let preview = try! EngineHost.decodeInvitation(fields)!
            fields.category = 3
            for (index, kind) in ["paused", "checking", "sign_in", "waiting_registration", "waiting_app", "waiting_database", "waiting_peer", "ready", "denied", "failed"].enumerated() {
                fields.state = UInt32(index + 1)
                let value = try! EngineHost.decodeInvitation(fields)!
                precondition(value["paused"] as? Bool == true)
                let status = value["invitation_status"] as! [String: Any]
                let state = status["state"] as! [String: Any]
                precondition(state["kind"] as? String == kind)
                if index == 7 { precondition(state["build_id"] as? String == "é\0🔑") }
                if index >= 8 { precondition(state["message"] as? String == "é\0🔑") }
            }
            fields.category = 4
            precondition(try! EngineHost.decodeInvitation(fields)?["invitation"] as? String == "é\0🔑")
            fields.category = 2; fields.state = 99
            do { _ = try EngineHost.decodeInvitation(fields); preconditionFailure("accepted invalid invitation state") } catch {}
            return preview
        }
        precondition(copiedInvitation["session"] as? String == "é\0🔑")
        precondition((copiedInvitation["received"] as? [String: Any])?["root_name"] as? String == "é\0🔑")
        precondition(try! EngineHost.decodeInvitation(InvitationFields()) == nil)
        precondition(try! EngineHost.decodeInvitation(InvitationFields(category: 2))?.isEmpty == true)

        for (kind, key) in [(1, "personal_volumes_selected"), (3, "sign_in_available"), (4, "code_sent"), (5, "ordered")] {
            for flag in [UInt8(0), 1] {
                precondition(try! EngineHost.decodeControl(ControlFields(kind: UInt32(kind), flag: flag))?[key] as? Bool == (flag == 1))
            }
        }
        precondition(try! EngineHost.decodeControl(ControlFields(kind: 13, flag: 1))?["agreement_accepted"] as? Bool == true)
        do { _ = try EngineHost.decodeControl(ControlFields(kind: 13, flag: 0)); preconditionFailure("invalid acceptance acknowledgment") } catch {}
        for (kind, key) in [(2, "display_name"), (6, "forgotten")] {
            for text in ["", "é\0🔑"] {
                let copiedControl = Array(text.utf8).withUnsafeBufferPointer { bytes in
                    try! EngineHost.decodeControl(ControlFields(kind: UInt32(kind), text: bytes.baseAddress, text_len: bytes.count))!
                }
                precondition(copiedControl[key] as? String == text)
            }
        }
        let fill = Array("saved@example.test".utf8).withUnsafeBufferPointer { email in
            Array("fixture-key".utf8).withUnsafeBufferPointer { key in
                try! EngineHost.decodeControl(ControlFields(secret: key.baseAddress, secret_len: key.count,
                    kind: 12, flag: 1, text: email.baseAddress, text_len: email.count))!
            }
        }
        precondition((fill["saved_account"] as? [String: String])?["email"] == "saved@example.test")
        precondition((fill["saved_account"] as? [String: String])?["account_key"] == "fixture-key")
        precondition(try! EngineHost.decodeControl(ControlFields(kind: 12))!.isEmpty)
        for fields in [ControlFields(kind: 99), ControlFields(kind: 1, flag: 2), ControlFields(kind: 2), ControlFields(kind: 12, flag: 1), ControlFields(kind: 12, flag: 2)] {
            do { _ = try EngineHost.decodeControl(fields); preconditionFailure("accepted invalid control fields") } catch {}
        }
        precondition(try! EngineHost.decodeControl(ControlFields()) == nil)
        let opened = Array(Identifier.encode([UInt8](repeating: 0xaa, count: 16)).utf8).withUnsafeBufferPointer { bytes in
            try! EngineHost.decodeControl(ControlFields(kind: 9, text: bytes.baseAddress, text_len: bytes.count, generation: 3))!
        }
        precondition(opened["instance_id"] as? String == Identifier.encode([UInt8](repeating: 0xaa, count: 16)))
        precondition(opened["generation"] as? UInt64 == 3)
        for text in ["short", String(repeating: "A", count: 32)] {
            Array(text.utf8).withUnsafeBufferPointer { bytes in
                do { _ = try EngineHost.decodeControl(ControlFields(kind: 9, text: bytes.baseAddress, text_len: bytes.count, generation: 3)); preconditionFailure("invalid opened instance") } catch {}
            }
        }
        Array(Identifier.encode([UInt8](repeating: 0xaa, count: 16)).utf8).withUnsafeBufferPointer { bytes in
            do { _ = try EngineHost.decodeControl(ControlFields(kind: 9, text: bytes.baseAddress, text_len: bytes.count)); preconditionFailure("zero opened generation") } catch {}
            precondition(try! EngineHost.decodeControl(ControlFields(kind: 7, text: bytes.baseAddress, text_len: bytes.count))?["quit"] as? String == Identifier.encode([UInt8](repeating: 0xaa, count: 16)))
            for flag: UInt8 in [0, 1] {
                precondition(try! EngineHost.decodeControl(ControlFields(kind: 7, flag: flag, text: bytes.baseAddress, text_len: bytes.count))?["drain_failed"] as? Bool == (flag != 0))
            }
        }
        let pendingFrame = Array("build".utf8).withUnsafeBufferPointer { text in
            Array(repeating: UInt8(9), count: 16).withUnsafeBufferPointer { id -> [String: Any] in
                pendingPresentation = PresentationFields(present: 1, instance: id.baseAddress, generation: 1,
                    build: text.baseAddress, build_len: text.count, invitation: text.baseAddress, invitation_len: text.count,
                    availability: text.baseAddress, availability_len: text.count)
                defer { pendingPresentation = PresentationFields() }
                let frame = try! EngineHost.decodePresentation(consentPointer)!
                precondition(frame["invitation"] as? String == "build")
                pendingPresentation.invitation = nil
                pendingPresentation.invitation_len = 0
                let local = try! EngineHost.decodePresentation(consentPointer)!
                precondition(local["invitation"] == nil)
                pendingPresentation.invitation_len = 1
                do { _ = try EngineHost.decodePresentation(consentPointer); preconditionFailure("nonempty invitation with null storage") } catch {}
                pendingPresentation.invitation_len = 0
                pendingPresentation.generation = 0
                do { _ = try EngineHost.decodePresentation(consentPointer); preconditionFailure("zero frame generation") } catch {}
                return frame
            }
        }
        precondition(pendingFrame["scene"] == nil)
        precondition(pendingFrame["instance_id"] as? String == Identifier.encode([UInt8](repeating: 0x09, count: 16)))
        precondition(pendingFrame["generation"] as? UInt64 == 1)
        let created = Array(Identifier.encode([UInt8](repeating: 0xaa, count: 32)).utf8).withUnsafeBufferPointer { bytes in
            try! EngineHost.decodeControl(ControlFields(kind: 8, text: bytes.baseAddress, text_len: bytes.count))!
        }
        precondition(created["id"] as? String == Identifier.encode([UInt8](repeating: 0xaa, count: 32)))

        let copiedTransfer: [String: Any] = Array("é\0🔑".utf8).withUnsafeBufferPointer { bytes in
            var fields = TransferFields(kind: 6, session: bytes.baseAddress, session_len: bytes.count,
                message: bytes.baseAddress, message_len: bytes.count, update_kind: 2,
                update_build: bytes.baseAddress, update_build_len: bytes.count,
                update_message: bytes.baseAddress, update_message_len: bytes.count)
            let value = try! EngineHost.decodeTransfer(fields)!
            for (index, kind) in ["idle", "running", "complete", "paused", "canceling", "failed"].enumerated() {
                fields.kind = UInt32(index + 1)
                precondition(try! EngineHost.decodeTransfer(fields)?["kind"] as? String == kind)
            }
            fields.kind = 99
            do { _ = try EngineHost.decodeTransfer(fields); preconditionFailure("accepted invalid transfer kind") } catch {}
            return value
        }
        precondition(copiedTransfer["session"] as? String == "é\0🔑")
        precondition(copiedTransfer["message"] as? String == "é\0🔑")
        precondition((copiedTransfer["update"] as? [String: Any])?["kind"] as? String == "paused")
        precondition(try! EngineHost.decodeTransfer(TransferFields()) == nil)
        for kind in 1...4 {
            var fields = RecoveryFields()
            fields.category = 1; fields.kind = UInt32(kind)
            fields.revision = RecoveryNumber(value: UInt64.max, present: 1)
            fields.pending_save = RecoveryNumber(value: 0, present: 1)
            fields.restored_sessions = RecoveryNumber(value: 0, present: 1)
            let decoded = try! EngineHost.decodeRecovery(fields)!
            precondition(decoded["kind"] is String)
            precondition(decoded["revision"] as? UInt64 == UInt64.max)
            precondition(decoded["pending_save"] as? Bool == false)
            precondition(decoded["restored_sessions"] as? UInt64 == 0)
        }
        var fields = RecoveryFields(); fields.category = 2
        let secret = Array("clé\0🔑".utf8)
        let copied = secret.withUnsafeBufferPointer { bytes -> [String: Any] in
            fields.secret = RecoveryText(bytes: bytes.baseAddress, len: bytes.count)
            return try! EngineHost.decodeRecovery(fields)!
        }
        precondition(copied["secret"] as? String == "clé\0🔑")
        fields.category = 99
        do { _ = try EngineHost.decodeRecovery(fields); preconditionFailure("unknown recovery category") } catch {}

        let signedIn = secret.withUnsafeBufferPointer { bytes -> [String: Any] in
            var recovery = RecoveryFields(); recovery.category = 1; recovery.kind = 2
            recovery.secret = RecoveryText(bytes: bytes.baseAddress, len: bytes.count)
            var signIn = SignInFields(); signIn.present = 1; signIn.signed_in = 1
            signIn.recovery = recovery; signIn.recovery_error = StatusText(bytes: bytes.baseAddress, len: bytes.count)
            return try! EngineHost.decodeSignIn(signIn)!
        }
        precondition(signedIn["signed_in"] as? Bool == true)
        precondition(signedIn["recovery_error"] as? String == "clé\0🔑")
        precondition((signedIn["recovery"] as? [String: Any])?["secret"] as? String == "clé\0🔑")
        var minimalSignIn = SignInFields(); minimalSignIn.present = 1
        precondition(try! EngineHost.decodeSignIn(minimalSignIn)?["recovery"] == nil)
        precondition(try! EngineHost.decodeSignIn(SignInFields()) == nil)
        minimalSignIn.replacement_required = 1
        precondition(try! EngineHost.decodeSignIn(minimalSignIn)?["replacement_required"] as? Bool == true)
        minimalSignIn.signed_in = 1
        do { _ = try EngineHost.decodeSignIn(minimalSignIn); preconditionFailure("pending replacement cannot be signed in") } catch {}
        minimalSignIn.signed_in = 0; minimalSignIn.replacement_required = 2
        do { _ = try EngineHost.decodeSignIn(minimalSignIn); preconditionFailure("invalid replacement flag") } catch {}


        let appID = Array("notes.dev".utf8)
        let buildID = Array(Identifier.encode([UInt8](repeating: 0x22, count: 32)).utf8)
        let copiedApp = appID.withUnsafeBufferPointer { text in
            buildID.withUnsafeBufferPointer { build in
                Array(repeating: UInt8(1), count: 16).withUnsafeBufferPointer { publisher -> [String: Any] in
                    developerFields = DeveloperFields(kind: 1, count: 1, next: StatusText(bytes: text.baseAddress, len: text.count))
                    appFields = AppFields(publisher: publisher.baseAddress,
                        app_id: StatusText(bytes: text.baseAddress, len: text.count),
                        build: StatusText(bytes: build.baseAddress, len: build.count), state: 1)
                    defer { developerFields = DeveloperFields(); appFields = AppFields() }
                    let page = try! EngineHost.decodeDeveloper(consentPointer)!
                    precondition(try! AppPage(page, after: nil).rows.count == 1)
                    developerFields.count = 33
                    do { _ = try EngineHost.decodeDeveloper(consentPointer); preconditionFailure("excess app rows") } catch {}
                    developerFields.count = 1; appFields.state = 4
                    do { _ = try EngineHost.decodeDeveloper(consentPointer); preconditionFailure("invalid app state") } catch {}
                    return page
                }
            }
        }
        precondition((copiedApp["apps"] as? [[String: Any]])?.first?["identity"] as? [String: String] == ["publisher": Identifier.encode([UInt8](repeating: 0x01, count: 16)), "app_id": "notes.dev"])
        precondition(copiedApp["next"] as? String == "notes.dev")
        precondition(try! EngineHost.decodeDeveloper(consentPointer) == nil)

        // This standalone executable has no bundle identifier. The bridge is a
        // fixture, so it must not select the application's real profile either.
        let profile = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        let preferences = UserDefaults(suiteName: "playbit-engine-host-" + UUID().uuidString)!
        let host = EngineHost(profile: profile, preferences: preferences, environment: [:])
        precondition(host.profile == profile)
        let fixture = BridgeFixture.shared
        var rejected = 0
        for kind in [-1, Int(UInt32.max) + 1] {
            host.input(instance: Identifier.encode([UInt8](repeating: 0xaa, count: 16)), kind: kind, target: 7, data: Data()) { result in
                guard case .failure = result else { preconditionFailure("invalid kind admitted") }
                rejected += 1
            }
        }
        precondition(rejected == 2 && fixture.read { $0.requests } == 0)
        for control: EngineHost.Control in [
            .orderSessions(["short"]),
            .orderSessions(Array(repeating: Identifier.encode([UInt8](repeating: 0xaa, count: 32)), count: 257)),
            .selectPersonalVolumes(session: "short", grants: []),
            .selectPersonalVolumes(session: Identifier.encode([UInt8](repeating: 0xaa, count: 32)), grants: [(volume: "short", write: false)]),
            .selectPersonalVolumes(session: Identifier.encode([UInt8](repeating: 0xaa, count: 32)), grants: Array(repeating: (volume: Identifier.encode([UInt8](repeating: 0xbb, count: 16)), write: false), count: 129))
        ] {
            host.request(control) { result in
                guard case .failure = result else { preconditionFailure("invalid collection admitted") }
                rejected += 1
            }
        }
        precondition(rejected == 7 && fixture.read { $0.requests } == 0)
        for control: EngineHost.Control in [.appFrame("short"), .quitApp(String(repeating: "A", count: 32)), .appLogs(Identifier.encode([UInt8](repeating: 0xaa, count: 32)))] {
            host.request(control) { result in
                guard case .failure = result else { preconditionFailure("invalid instance admitted") }
                rejected += 1
            }
        }
        precondition(rejected == 10 && fixture.read { $0.requests } == 0)
        for (instance, data) in [(Identifier.encode([UInt8](repeating: 0xaa, count: 32)), Data()), (Identifier.encode([UInt8](repeating: 0xaa, count: 16)), Data(count: GuestTextEdits.textBytesMax + 1))] {
            host.input(instance: instance, kind: 3, target: 7, data: data) { result in
                guard case .failure = result else { preconditionFailure("invalid input admitted") }
                rejected += 1
            }
        }
        precondition(rejected == 12 && fixture.read { $0.requests } == 0)
        // Database calls share the worker admission limit, including exact byte
        // bounds before borrowing request data across the C bridge.
        for count in [0, 64 * 1024 + 1] {
            do { try EngineHost.Control.database(Data(count: count)).validate(); preconditionFailure("invalid database request admitted") }
            catch {}
        }
        try! EngineHost.Control.database(Data(count: 64 * 1024)).validate()
        var firstDelivered = false
        host.input(instance: Identifier.encode([UInt8](repeating: 0xaa, count: 16)), kind: 14, target: 7,
            data: Data(repeating: 65, count: GuestTextEdits.payloadBytesMax)) { result in




            guard case .success(let value) = result else { preconditionFailure("first response failed") }
            precondition((value["session"] as? [String: Any])?["resolved_build_id"] as? String == "Native scene")
            precondition((value["scene"] as? GuestScene)?.text == "Native scene")
            precondition(value["instance_id"] as? String == Identifier.encode([UInt8](repeating: 0x07, count: 16)))
            precondition(value["generation"] as? UInt64 == 3)
            precondition(fixture.read { $0.scenes.isEmpty }, "reply freed before main delivery")
            firstDelivered = true
        }
        // Deliberately prevent main callback delivery, while the runtime owner
        // receives a descriptor event on its independent queue.
        let ready = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            let deadline = Date().addingTimeInterval(2)
            while fixture.read({ $0.requests }) == 0 {
                precondition(Date() < deadline); Thread.sleep(forTimeInterval: 0.001)
            }
            fixture.notify()
            precondition(fixture.completion.wait(timeout: .now() + 2) == .success)
            ready.signal()
        }
        precondition(ready.wait(timeout: .now() + 3) == .success)
        precondition(!firstDelivered)
        // A response still awaiting UI delivery retains its write admission.
        var busySignOut = false
        host.signOut(retireApps: { preconditionFailure("busy sign-out retired apps") }) { result in
            guard case .failure = result else { preconditionFailure("signed out under an admitted action") }
            busySignOut = true
        }
        precondition(busySignOut)
        precondition(fixture.read { $0.completions } == 1)
        until { firstDelivered && fixture.read { $0.acknowledgments } == 1 }


        // Decode must own the JSON before the native reply's borrowed bytes are
        // released, and acknowledge the database response like every owner turn.
        var databaseDelivered = false
        host.request(.database(Data("{\"op\":\"volumes\"}".utf8))) { result in
            guard case .success(let value) = result else { preconditionFailure("database response failed") }
            precondition(value["name"] as? String == "é\0🔑")
            precondition(fixture.read { $0.databases.isEmpty }, "reply freed before main delivery")
            databaseDelivered = true
        }
        until { databaseDelivered && fixture.read { $0.acknowledgments } == 2 }
        var byteLimitRejected = false
        host.request(.previewInvitation(String(repeating: "x", count: 4 * 1024 * 1024))) { result in
            guard case .failure = result else { preconditionFailure("oversized request admitted") }
            byteLimitRejected = true
        }
        precondition(byteLimitRejected && fixture.read { $0.requests } == 2)
        var failures = 0
        for _ in 0..<33 {
            host.request(.status) { result in
                guard case .failure = result else { preconditionFailure("fault fixture unexpectedly succeeded") }
                failures += 1
            }
        }
        precondition(failures == 1, "33rd request must reject before dispatch")
        until { fixture.read { $0.requests } == 34 }
        fixture.read { $0.fail = true }
        fixture.notify()
        until { failures == 33 }
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        precondition(fixture.read { $0.completions } == 2, "poisoned readable descriptor must not spin")
        host.request(.status) { result in
            guard case .failure = result else { preconditionFailure("failed owner reopened silently") }
            failures += 1
        }
        until { failures == 34 }
        precondition(fixture.read { $0.requests } == 34)
        var failedSignOut = false
        host.signOut(retireApps: { preconditionFailure("failed runtime retired apps") }) { result in
            guard case .failure = result else { preconditionFailure("failed runtime signed out") }
            failedSignOut = true
        }
        until { failedSignOut }
        // A channel change closes the old owner but never signs that account out.
        // Invalid launch selection cannot open Public as an implicit fallback.
        fixture.read { $0.fail = false; $0.immediate = true }
        let channelPreferences = UserDefaults(suiteName: "playbit-channel-" + UUID().uuidString)!
        channelPreferences.set("testing", forKey: "PlaybitChannel")
        let channelHost = EngineHost(profile: profile, preferences: channelPreferences, environment: ["PLAYBIT_CHANNEL": "public"])
        precondition(channelHost.channel == .public)
        var loaded = false
        channelHost.request(.status) { _ in loaded = true }
        var busySwitch = false
        channelHost.switchChannel(.testing, retireApps: { preconditionFailure("busy switch retired apps") }) { result in
            guard case .failure = result else { preconditionFailure("switched under admitted input") }
            busySwitch = true
        }
        precondition(busySwitch)
        until { loaded }
        var retired = false
        var switched = false
        let closes = fixture.read { $0.closes }
        channelHost.switchChannel(.testing, retireApps: { retired = true }) { result in
            try! result.get(); switched = true
        }
        until { switched }
        precondition(retired && channelHost.channel == .testing)
        precondition(channelHost.profile != profile && channelHost.profile.lastPathComponent == profile.lastPathComponent + "-testing")
        precondition(fixture.read { $0.closes } == closes + 1)
        precondition(channelPreferences.string(forKey: "PlaybitChannel") == "testing")
        loaded = false
        channelHost.request(.status) { result in _ = try! result.get(); loaded = true }
        until { loaded }
        precondition(fixture.read { $0.opened.last!.0 } == channelHost.profile.path)
        precondition(fixture.read { $0.opened.last!.1 } == 1)
        fixture.read { $0.stopFailure = true }
        var failedSwitch = false
        channelHost.switchChannel(.public, retireApps: {}) { result in
            guard case .failure = result else { preconditionFailure("ignored shutdown failure") }
            failedSwitch = true
        }
        until { failedSwitch }
        precondition(channelHost.channel == .testing && channelPreferences.string(forKey: "PlaybitChannel") == "testing")
        fixture.read { $0.stopFailure = false }
        switched = false
        channelHost.switchChannel(.public, retireApps: {}) { result in try! result.get(); switched = true }
        until { switched }
        precondition(channelHost.profile == profile && fixture.read { $0.credentialClears } == 0)
        let invalid = EngineHost(profile: profile, preferences: channelPreferences, environment: ["PLAYBIT_CHANNEL": "internal"])
        precondition(invalid.channel == nil)
        let openCount = fixture.read { $0.opened.count }
        var invalidRejected = false
        invalid.request(.status) { result in
            guard case .failure = result else { preconditionFailure("invalid channel opened") }
            invalidRejected = true
        }
        until { invalidRejected }
        precondition(fixture.read { $0.opened.count } == openCount)
        print("EngineHostTests: blocked-main completion, one response acknowledgment, 32-request admission and fatal failure callbacks passed")
    }
}

func pb_ios_app_logs(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ len: Int) -> OpaquePointer? { submitFixture(handle, id) }

func pb_ios_list_apps(_ handle: OpaquePointer?, _ id: UInt64, _ text: UnsafePointer<UInt8>?, _ len: Int) -> OpaquePointer? { submitFixture(handle, id) }

func pb_ios_create_app_session(_ handle: OpaquePointer?, _ id: UInt64, _ publisher: UnsafePointer<UInt8>?, _ publisherLen: Int, _ appID: UnsafePointer<UInt8>?, _ appIDLen: Int) -> OpaquePointer? { submitFixture(handle, id) }

func pb_ios_get_app_display(_ handle: OpaquePointer?, _ id: UInt64, _ publisher: UnsafePointer<UInt8>?, _ publisherLen: Int, _ appID: UnsafePointer<UInt8>?, _ appIDLen: Int, _ build: UnsafePointer<UInt8>?, _ buildLen: Int) -> OpaquePointer? { submitFixture(handle, id) }

func pb_ios_sign_in_with_key(_ handle: OpaquePointer?, _ id: UInt64, _ first: UnsafePointer<UInt8>?, _ firstLen: Int, _ second: UnsafePointer<UInt8>?, _ secondLen: Int) -> OpaquePointer? { submitFixture(handle, id) }

func pb_ios_order_sessions(_ handle: OpaquePointer?, _ id: UInt64, _ sessions: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? { submitFixture(handle, id) }
func pb_ios_select_personal_volumes(_ handle: OpaquePointer?, _ id: UInt64, _ version: UnsafePointer<UInt8>?, _ versionLen: Int, _ volumes: UnsafePointer<UInt8>?, _ writes: UnsafePointer<UInt8>?, _ count: Int) -> OpaquePointer? { submitFixture(handle, id) }

struct IosCompletion {
    var request_id: UInt64
    var has_response: UInt8
    var error_code: UInt32
    var error: UnsafePointer<CChar>?
    var background_error: UnsafePointer<CChar>?
}
func pb_ios_reply_completion(_ pointer: OpaquePointer) -> IosCompletion {
    BridgeFixture.shared.read { $0.replyCompletions[pointer]! }
}
struct IosSchedule {
    var generation: UnsafePointer<UInt8>?
    var sequence: UInt64
    var wake_after_ns: UInt64
    var has_wake: UInt8
}
func pb_ios_reply_schedule(_ reply: OpaquePointer) -> IosSchedule {
    BridgeFixture.shared.read {
        IosSchedule(generation: $0.generations[reply].map { UnsafePointer($0) },
            sequence: 1, wake_after_ns: 0, has_wake: 0)
    }
}
func pb_ios_reply_scene(_ reply: OpaquePointer) -> OpaquePointer? { BridgeFixture.shared.read { $0.scenes[reply]?.pointer } }
func pb_ios_reply_free(_ reply: OpaquePointer) {
    BridgeFixture.shared.read {
        if let error = $0.replyCompletions.removeValue(forKey: reply)?.error { free(UnsafeMutableRawPointer(mutating: error)) }
        $0.scenes[reply] = nil
        $0.generations.removeValue(forKey: reply)?.deallocate()
        $0.databases.removeValue(forKey: reply)?.0.deallocate()
    }
    free(UnsafeMutableRawPointer(reply))
}

struct RecoveryText { var bytes: UnsafePointer<UInt8>? = nil; var len: Int = 0 }
struct RecoveryNumber { var value: UInt64 = 0; var present: UInt8 = 0 }
struct RecoveryFields {
    var category: UInt32 = 0; var kind: UInt32 = 0
    var revision = RecoveryNumber(); var pending_save = RecoveryNumber()
    var restored_sessions = RecoveryNumber(); var restored_volumes = RecoveryNumber()
    var secret = RecoveryText()
}
func pb_ios_reply_recovery(_ pointer: OpaquePointer) -> RecoveryFields { RecoveryFields() }

struct LogFields {
    var engine: UnsafePointer<UInt8>? = nil
    var instance: UInt64 = 0; var next: UInt64 = 0; var dropped: UInt64 = 0
    var count: Int = 0; var reset: UInt8 = 0
}
struct LogLine { var bytes: UnsafePointer<UInt8>? = nil; var len: Int = 0 }
func pb_ios_reply_logs(_ pointer: OpaquePointer) -> LogFields { LogFields() }
func pb_ios_reply_log_line(_ pointer: OpaquePointer, _ index: Int) -> LogLine { LogLine() }

struct PresentationFields {
    var present: UInt8 = 0
    var instance: UnsafePointer<UInt8>? = nil; var generation: UInt64 = 0
    var build: UnsafePointer<UInt8>? = nil; var build_len: Int = 0
    var invitation: UnsafePointer<UInt8>? = nil; var invitation_len: Int = 0
    var availability: UnsafePointer<UInt8>? = nil; var availability_len: Int = 0
    var update_kind: UInt32 = 0
    var update_build: UnsafePointer<UInt8>? = nil; var update_build_len: Int = 0
    var update_message: UnsafePointer<UInt8>? = nil; var update_message_len: Int = 0
}
private var pendingPresentation = PresentationFields()
func pb_ios_reply_presentation(_ pointer: OpaquePointer) -> PresentationFields {
    if pendingPresentation.present != 0 { return pendingPresentation }
    guard let scene = pb_ios_reply_scene(pointer) else { return PresentationFields() }
    let node = pb_ios_scene_node(scene)
    return PresentationFields(present: 1, instance: BridgeFixture.shared.read { $0.generations[pointer].map { UnsafePointer($0) } }, generation: 3, build: node.text, build_len: node.text_len,
        invitation: node.text, invitation_len: node.text_len,
        availability: node.text, availability_len: node.text_len)
}

struct TransferFields {
    var kind: UInt32 = 0
    var session: UnsafePointer<UInt8>? = nil; var session_len: Int = 0
    var message: UnsafePointer<UInt8>? = nil; var message_len: Int = 0
    var update_kind: UInt32 = 0
    var update_build: UnsafePointer<UInt8>? = nil; var update_build_len: Int = 0
    var update_message: UnsafePointer<UInt8>? = nil; var update_message_len: Int = 0
}
func pb_ios_reply_transfer(_ pointer: OpaquePointer) -> TransferFields { TransferFields() }

struct ControlFields {
    var secret: UnsafePointer<UInt8>? = nil
    var secret_len: Int = 0
    var kind: UInt32 = 0
    var flag: UInt8 = 0
    var text: UnsafePointer<UInt8>? = nil
    var text_len: Int = 0
    var generation: UInt64 = 0
}
func pb_ios_reply_control(_ pointer: OpaquePointer) -> ControlFields { ControlFields() }

struct InvitationFields {
    var category: UInt32 = 0
    var paused: UInt8 = 0; var retained: UInt8 = 0
    var state: UInt32 = 0
    var session: UnsafePointer<UInt8>? = nil; var session_len: Int = 0
    var root_name: UnsafePointer<UInt8>? = nil; var root_name_len: Int = 0
    var build: UnsafePointer<UInt8>? = nil; var build_len: Int = 0
    var message: UnsafePointer<UInt8>? = nil; var message_len: Int = 0
    var invitation: UnsafePointer<UInt8>? = nil; var invitation_len: Int = 0
    var availability: UnsafePointer<UInt8>? = nil; var availability_len: Int = 0
}
func pb_ios_reply_invitation(_ pointer: OpaquePointer) -> InvitationFields { InvitationFields() }

struct PersonalVolumeFields {
    var session: UnsafePointer<UInt8>? = nil; var session_len: Int = 0
    var volumes: Int = 0; var grants: Int = 0
    var has_grants: UInt8 = 0
}
struct PersonalVolumeRow { var id: UnsafePointer<UInt8>? = nil; var name: UnsafePointer<UInt8>? = nil; var name_len: Int = 0 }
struct PersonalGrant { var id: UnsafePointer<UInt8>? = nil; var write: UInt8 = 0 }
// Only the synchronous decoder test uses these spans; reset before host workers start.
private var consentFields = PersonalVolumeFields()
private var consentRow = PersonalVolumeRow()
private var consentGrant = PersonalGrant()
func pb_ios_reply_personal_volumes(_ pointer: OpaquePointer) -> PersonalVolumeFields { consentFields }
func pb_ios_reply_personal_volume(_ pointer: OpaquePointer, _ index: Int) -> PersonalVolumeRow { consentRow }
func pb_ios_reply_personal_grant(_ pointer: OpaquePointer, _ index: Int) -> PersonalGrant { consentGrant }

struct StatusText { var bytes: UnsafePointer<UInt8>? = nil; var len: Int = 0 }
private let backupStatusBytes = strdup("unavailable")!
struct StatusFields {
    var account = StatusText()
    var enrolled: UInt8 = 0; var unlocked: UInt8 = 0; var initialized: UInt8 = 0; var replacement_required: UInt8 = 0
    var agreement_required: UInt8 = 0
    var display_name = StatusText()
    var email = StatusText()
    var account_key_backup = StatusText(bytes: UnsafeRawPointer(backupStatusBytes).assumingMemoryBound(to: UInt8.self), len: 11)
    var sessions: Int = 0; var volumes: Int = 0; var examples: Int = 0; var invitations: Int = 0
}
struct SessionFields { var id = StatusText(); var build = StatusText(); var root: UnsafePointer<UInt8>? = nil; var name = StatusText(); var build_kind = StatusText(); var build_message = StatusText(); var reference = StatusText() }
struct ExampleFields { var id = StatusText(); var name = StatusText(); var description = StatusText(); var invitation = StatusText(); var source = StatusText() }
private var statusFields = StatusFields()
private var statusSession = SessionFields()
private var statusVolume = PersonalVolumeRow()
private var statusExample = ExampleFields()
private var statusInvitation = InvitationFields()
private var statusWatch = InvitationFields()
func pb_ios_reply_status(_ pointer: OpaquePointer) -> StatusFields { statusFields }
func pb_ios_status_session(_ pointer: OpaquePointer, _ index: Int) -> SessionFields { statusSession }
func pb_ios_status_volume(_ pointer: OpaquePointer, _ index: Int) -> PersonalVolumeRow { statusVolume }
func pb_ios_status_example(_ pointer: OpaquePointer, _ index: Int) -> ExampleFields { statusExample }
func pb_ios_status_invitation(_ pointer: OpaquePointer, _ index: Int) -> InvitationFields { statusInvitation }
func pb_ios_status_watch(_ pointer: OpaquePointer) -> InvitationFields { statusWatch }

struct SignInFields {
    var present: UInt8 = 0; var signed_in: UInt8 = 0; var replacement_required: UInt8 = 0
    var recovery = RecoveryFields(); var recovery_error = StatusText()
}
private var signInFields = SignInFields()
func pb_ios_reply_sign_in(_ pointer: OpaquePointer) -> SignInFields { signInFields }
struct DeveloperFields {
    var kind: UInt32 = 0; var count: Int = 0; var next = StatusText()
    var build = StatusText(); var name = StatusText()
}
struct AppFields {
    var publisher: UnsafePointer<UInt8>? = nil
    var app_id = StatusText(); var build = StatusText(); var state: UInt32 = 0
}
private var developerFields = DeveloperFields()
private var appFields = AppFields()
func pb_ios_reply_developer(_ pointer: OpaquePointer) -> DeveloperFields { developerFields }
func pb_ios_reply_app(_ pointer: OpaquePointer, _ index: Int) -> AppFields { appFields }
