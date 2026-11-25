import Foundation
import Darwin

enum ReleaseChannel: String, CaseIterable {
    case `public`, testing
    var title: String { self == .public ? "Public" : "Testing" }
    var bridgeValue: UInt8 { self == .public ? 0 : 1 }
    var website: String { self == .public ? "https://playbit.app" : "https://testing.playbit.app" }
    var hostname: String { self == .public ? "ap.playbit.app" : "ap.testing.playbit.app" }
    func profile(_ base: URL) -> URL {
        self == .public ? base : base.deletingLastPathComponent().appendingPathComponent(base.lastPathComponent + "-testing")
    }
    static func selected(environment: [String: String], preferences: UserDefaults) throws -> Self {
        let stored = preferences.object(forKey: "PlaybitChannel")
        if environment["PLAYBIT_CHANNEL"] == nil, stored == nil { return .public }
        guard let value = environment["PLAYBIT_CHANNEL"] ?? (stored as? String),
              let channel = Self(rawValue: value) else {
            throw NSError(domain: "Playbit", code: 1,
                userInfo: [NSLocalizedDescriptionKey: "The environment must be exactly public or testing. Choose an environment to continue."])
        }
        return channel
    }
}

final class EngineHost {
    static let shared: EngineHost = {
        var profile = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(Bundle.main.bundleIdentifier!)
        #if PHASE1_QUALIFICATION
        let arguments = ProcessInfo.processInfo.arguments
        if let index = arguments.firstIndex(of: "--phase1-profile") {
            precondition(index + 1 < arguments.count)
            let name = arguments[index + 1]
            precondition(!name.isEmpty && name.utf8.count <= 64 && name.utf8.allSatisfy {
                (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || $0 == 45
            })
            profile = profile.appendingPathComponent("qualification").appendingPathComponent(name)
        }
        #endif
        return EngineHost(profile: profile)
    }()
    static let backgroundChanged = Notification.Name("PlaybitBackgroundChanged")
    private var completionSource: DispatchSourceRead?
    private var deadlineSource: DispatchSourceTimer?
    private var changeSource: DispatchSourceUserDataAdd?
    private let worker = DispatchQueue(label: "app.playbit.engine")
    private var handle: OpaquePointer?
    private var suspended = false
    private var providerStatePending = true
    #if os(iOS)
    private var providerToken: (bytes: Data, invalidated: Bool)?
    private let providerTokenLock = NSLock()
    private var latestProviderToken: (bytes: Data, invalidated: Bool)?
    private var providerTokenSource: DispatchSourceUserDataAdd?
    private var providerPushError = false
    private var providerPushAttempt: String?
    private var providerPushRunning = false
    private var providerPushInFlightSignature: String?
    private var providerPushPending = false
    private let providerPushWorker = DispatchQueue(label: "app.playbit.provider-push", qos: .utility)
    #endif
    #if os(iOS)
    private var providerNotification: Int32 = -1
    deinit {
        if providerNotification >= 0 { notify_cancel(providerNotification) }
        providerTokenSource?.cancel()
    }
    #endif
    private var providerStateError: String?
    private var changingAccount = false // Main-thread admission gate.
    private let baseProfile: URL
    private let preferences: UserDefaults
    private(set) var channel: ReleaseChannel?
    var profile: URL { (channel ?? .public).profile(baseProfile) }
    enum Control {
        case database(Data)
        case status
        case prepareSignIn
        case savedAccount
        case retryAccountKeyBackup
        case transferStatus
        case cancelTransfer
        case recoveryStatus
        case prepareRecovery
        case saveRecovery
        case appFrame(String)
        case openApp(String)
        case prepareSession(String)
        case quitApp(String)
        case checkUpdates(String)
        case personalVolumes(String)
        case invitation(String)
        case forgetSession(String)
        case watchInvitation(String)
        case resumeInvitation(String)
        case pauseInvitation(String)
        case startJoin(String)
        case previewInvitation(String)
        case acceptAgreement(String)
        case setDisplayName(String)
        case signIn(String)
        case requestCode(String)
        case recoverCredentials(String)
        case appLogs(String)
        case listApps(String?)
        case getAppDisplay(publisher: String, appID: String, build: String)
        case createAppSession(publisher: String, appID: String)
        case signInWithKey(email: String, key: String)
        case orderSessions([String])
        case selectPersonalVolumes(session: String, grants: [(volume: String, write: Bool)])
        func validate() throws {
            switch self {
            case .database(let data):
                guard !data.isEmpty, data.count <= 64 * 1024 else {
                    throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Database edit exceeds the editor limit."])
                }
            case .appFrame(let instance), .quitApp(let instance), .appLogs(let instance):
                guard EngineHost.validInstance(instance) else {
                    throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid session instance"])
                }
            case .orderSessions(let sessions):
                guard sessions.count <= 256, sessions.allSatisfy({ Identifier.valid($0, bytes: 32) }) else {
                    throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid session order"])
                }
            case .selectPersonalVolumes(let version, let grants):
                guard Identifier.valid(version, bytes: 32), grants.count <= 128,
                      grants.allSatisfy({ Identifier.valid($0.volume) }) else {
                    throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid personal-volume selection"])
                }
            case .listApps(let cursor):
                guard cursor.map(AppPage.validAppID) ?? true else { throw AppPage.Failure.invalid }
            case .createAppSession(let publisher, let appID):
                guard Identifier.valid(publisher), AppPage.validAppID(appID) else { throw AppPage.Failure.invalid }
            case .getAppDisplay(let publisher, let appID, let build):
                guard Identifier.valid(publisher), AppPage.validAppID(appID), Identifier.valid(build, bytes: 32) else { throw AppPage.Failure.invalid }
            default: break
            }
        }
        var bytes: Int {
            switch self {
            case .database(let data): return data.count
            case .appFrame(let text): return text.utf8.count + 8
            case .openApp(let text), .prepareSession(let text): return text.utf8.count + 8
            case .quitApp(let text): return text.utf8.count + 8
            case .checkUpdates(let text): return text.utf8.count + 8
            case .personalVolumes(let text): return text.utf8.count + 8
            case .invitation(let text): return text.utf8.count + 8
            case .forgetSession(let text): return text.utf8.count + 8
            case .watchInvitation(let text): return text.utf8.count + 8
            case .resumeInvitation(let text): return text.utf8.count + 8
            case .pauseInvitation(let text): return text.utf8.count + 8
            case .startJoin(let text): return text.utf8.count + 8
            case .previewInvitation(let text): return text.utf8.count + 8
            case .acceptAgreement(let account): return account.utf8.count + 8
            case .setDisplayName(let text): return text.utf8.count + 8
            case .signIn(let text): return text.utf8.count + 8
            case .requestCode(let text): return text.utf8.count + 8
            case .recoverCredentials(let text): return text.utf8.count + 8
            case .appLogs(let text): return text.utf8.count + 8
            case .listApps(let cursor): return (cursor?.utf8.count ?? 0) + 8
            case .createAppSession(let publisher, let appID): return publisher.utf8.count + appID.utf8.count + 8
            case .getAppDisplay(let publisher, let appID, let build): return publisher.utf8.count + appID.utf8.count + build.utf8.count + 8
            case .signInWithKey(let first, let second):
                return first.utf8.count + second.utf8.count + 8
            case .orderSessions(let sessions): return sessions.count * 32 + 8
            case .selectPersonalVolumes(let version, let grants): return version.utf8.count + grants.count * 17 + 8
            default: return 8
            }
        }
        func submit(_ handle: OpaquePointer?, id: UInt64) -> OpaquePointer? {
            switch self {
            case .status: return pb_ios_status(handle, id)
            case .acceptAgreement(let account):
                return Array(account.utf8).withUnsafeBufferPointer { pb_ios_accept_agreement(handle, id, $0.baseAddress, $0.count) }
            case .database(let data):
                return data.withUnsafeBytes { bytes in
                    pb_ios_database(handle, id, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count)
                }
            case .savedAccount: return pb_ios_saved_account(handle, id)
            case .retryAccountKeyBackup: return pb_ios_retry_account_key_backup(handle, id)
            case .prepareSignIn: return pb_ios_prepare_sign_in(handle, id)
            case .transferStatus: return pb_ios_transfer_status(handle, id)
            case .cancelTransfer: return pb_ios_cancel_transfer(handle, id)
            case .recoveryStatus: return pb_ios_recovery_status(handle, id)
            case .prepareRecovery: return pb_ios_prepare_recovery(handle, id)
            case .saveRecovery: return pb_ios_save_recovery(handle, id)
            case .appFrame(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_app_frame(handle, id, $0.baseAddress, $0.count) }
            case .openApp(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_open_app(handle, id, $0.baseAddress, $0.count) }
            case .prepareSession(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_prepare_session(handle, id, $0.baseAddress, $0.count) }
            case .quitApp(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_quit_app(handle, id, $0.baseAddress, $0.count) }
            case .checkUpdates(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_check_updates(handle, id, $0.baseAddress, $0.count) }
            case .personalVolumes(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_personal_volumes(handle, id, $0.baseAddress, $0.count) }
            case .invitation(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_invitation(handle, id, $0.baseAddress, $0.count) }
            case .forgetSession(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_forget_session(handle, id, $0.baseAddress, $0.count) }
            case .watchInvitation(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_watch_invitation(handle, id, $0.baseAddress, $0.count) }
            case .resumeInvitation(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_resume_invitation(handle, id, $0.baseAddress, $0.count) }
            case .pauseInvitation(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_pause_invitation(handle, id, $0.baseAddress, $0.count) }
            case .startJoin(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_start_join(handle, id, $0.baseAddress, $0.count) }
            case .previewInvitation(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_preview_invitation(handle, id, $0.baseAddress, $0.count) }
            case .setDisplayName(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_set_display_name(handle, id, $0.baseAddress, $0.count) }
            case .signIn(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_sign_in(handle, id, $0.baseAddress, $0.count) }
            case .requestCode(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_request_code(handle, id, $0.baseAddress, $0.count) }
            case .recoverCredentials(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_recover_credentials(handle, id, $0.baseAddress, $0.count) }
            case .appLogs(let text):
                return Array(text.utf8).withUnsafeBufferPointer { pb_ios_app_logs(handle, id, $0.baseAddress, $0.count) }
            case .createAppSession(let publisher, let appID):
                return Array(publisher.utf8).withUnsafeBufferPointer { a in
                    Array(appID.utf8).withUnsafeBufferPointer { b in
                        pb_ios_create_app_session(handle, id, a.baseAddress, a.count, b.baseAddress, b.count)
                    }
                }
            case .listApps(let cursor):
                return Array((cursor ?? "").utf8).withUnsafeBufferPointer { pb_ios_list_apps(handle, id, $0.baseAddress, $0.count) }
            case .getAppDisplay(let publisher, let appID, let build):
                return Array(publisher.utf8).withUnsafeBufferPointer { a in
                    Array(appID.utf8).withUnsafeBufferPointer { b in
                        Array(build.utf8).withUnsafeBufferPointer { c in
                            pb_ios_get_app_display(handle, id, a.baseAddress, a.count, b.baseAddress, b.count, c.baseAddress, c.count)
                        }
                    }
                }
            case .signInWithKey(let first, let second):
                return Array(first.utf8).withUnsafeBufferPointer { a in
                    Array(second.utf8).withUnsafeBufferPointer { b in
                        pb_ios_sign_in_with_key(handle, id, a.baseAddress, a.count, b.baseAddress, b.count)
                    }
                }
            case .orderSessions(let sessions):
                return sessions.flatMap { Identifier.decode($0, bytes: 32)! }.withUnsafeBufferPointer {
                    pb_ios_order_sessions(handle, id, $0.baseAddress, sessions.count)
                }
            case .selectPersonalVolumes(let version, let grants):
                return Array(version.utf8).withUnsafeBufferPointer { version in
                    grants.flatMap { Identifier.decode($0.volume, bytes: 16)! }.withUnsafeBufferPointer { volumes in
                        grants.map { UInt8($0.write ? 1 : 0) }.withUnsafeBufferPointer { writes in
                            pb_ios_select_personal_volumes(handle, id, version.baseAddress, version.count,
                                volumes.baseAddress, writes.baseAddress, grants.count)
                        }
                    }
                }
            }
        }
    }
    private enum Payload {
        case control(Control)
        case input(instance: String, kind: UInt32, target: UInt32, objectID: UInt32, data: Data)
    }
    private struct Pending {
        let id: UInt64
        let payload: Payload
        let bytes: Int
        let completion: (Result<[String: Any], Error>) -> Void
    }
    // Worker-owned callbacks. Rust uses the same readiness/fairness policy as
    // IPC, so an unavailable network operation cannot hold up local input.
    private var pending: [UInt64: Pending] = [:]
    private var terminalError: NSError?
    private var nextID: UInt64 = 1
    private var admittedCount = 0
    private var admittedBytes = 0
    private struct ChangeToken: Equatable {
        let generation: Data
        let sequence: UInt64
    }
    private struct OwnerReply {
        let completion: (id: UInt64, result: Result<[String: Any], Error>)?
        let backgroundError: String?
        let changes: ChangeToken?
        let wakeAfter: UInt64?
    }
    private var changes: ChangeToken?
    private let noticeLock = NSLock()
    private var noticeError: String?
    #if PHASE1_QUALIFICATION
    private var stoppedLogFixture = false // Main-thread-only fault injection.
    #endif

    init(profile: URL, preferences: UserDefaults = .standard,
         environment: [String: String] = ProcessInfo.processInfo.environment) {
        self.baseProfile = profile
        self.preferences = preferences
        do { channel = try ReleaseChannel.selected(environment: environment, preferences: preferences) }
        catch { terminalError = error as NSError }
        let source = DispatchSource.makeUserDataAddSource(queue: .main)
        source.setEventHandler { [weak self] in
            guard let self else { return }
            self.noticeLock.lock()
            let error = self.noticeError
            self.noticeError = nil
            self.noticeLock.unlock()
            NotificationCenter.default.post(name: Self.backgroundChanged, object: nil,
                userInfo: error.map { ["error": $0] })
        }
        changeSource = source
        source.resume()
    }
    private func checkNativeError(_ pointer: UnsafeMutablePointer<CChar>?) throws {
        guard let pointer else { return }
        defer { pb_ios_string_free(pointer) }
        throw failure(String(cString: pointer))
    }
    private func decode(_ pointer: OpaquePointer?) throws -> OwnerReply {
        guard let pointer else { throw failure("No runtime response") }
        defer { pb_ios_reply_free(pointer) }
        let completed = pb_ios_reply_completion(pointer)
        let error = completed.error.map { failure(String(cString: $0), code: Int(completed.error_code)) }
        if completed.has_response == 0, let error { throw error }
        let result: Result<[String: Any], Error> = Result {
            if let error { throw error }
            guard completed.has_response != 0 else { return [:] }
            let database = pb_ios_reply_database(pointer)
            if let bytes = database.bytes {
                guard database.len <= 256 * 1024,
                      let value = try JSONSerialization.jsonObject(with: Data(bytes: bytes, count: database.len)) as? [String: Any] else {
                    throw failure("Invalid database browser response")
                }
                return value
            }
            if let signIn = try Self.decodeSignIn(pb_ios_reply_sign_in(pointer)) { return signIn }
            if let developer = try Self.decodeDeveloper(pointer) { return developer }
            if let presentation = try Self.decodePresentation(pointer) { return presentation }
            if let status = try Self.decodeStatus(pointer) { return status }
            if let volumes = try Self.decodePersonalVolumes(pointer) { return volumes }
            if let invitation = try Self.decodeInvitation(pb_ios_reply_invitation(pointer)) { return invitation }
            if let control = try Self.decodeControl(pb_ios_reply_control(pointer)) { return control }
            if let transfer = try Self.decodeTransfer(pb_ios_reply_transfer(pointer)) { return ["transfer": transfer] }
            if let logs = try Self.decodeLogs(pointer) { return ["logs": logs] }
            if let recovery = try Self.decodeRecovery(pb_ios_reply_recovery(pointer)) { return recovery }
            throw failure("Missing native runtime payload")
        }
        let schedule = pb_ios_reply_schedule(pointer)
        let changes = schedule.generation.map {
            ChangeToken(generation: Data(bytes: $0, count: 16), sequence: schedule.sequence)
        }
        return OwnerReply(completion: completed.has_response != 0 ? (completed.request_id, result) : nil,
            backgroundError: completed.background_error.map { String(cString: $0) },
            changes: changes, wakeAfter: schedule.has_wake != 0 ? schedule.wake_after_ns : nil)
    }
    static func decodeSignIn(_ fields: SignInFields) throws -> [String: Any]? {
        guard fields.present != 0 else { return nil }
        guard fields.present == 1, fields.signed_in <= 1, fields.replacement_required <= 1,
              !(fields.signed_in != 0 && fields.replacement_required != 0) else {
            throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native sign-in result"])
        }
        var value: [String: Any] = ["signed_in": fields.signed_in != 0, "replacement_required": fields.replacement_required != 0]
        if let recovery = try decodeRecovery(fields.recovery) { value["recovery"] = recovery }
        if let bytes = fields.recovery_error.bytes {
            guard fields.recovery_error.len >= 0, fields.recovery_error.len <= 4 * 1024 * 1024,
                let error = String(bytes: UnsafeBufferPointer(start: bytes, count: fields.recovery_error.len), encoding: .utf8) else {
                throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native recovery error"])
            }
            value["recovery_error"] = error
        }
        return value
    }
    private static func validInstance(_ text: String) -> Bool {
        Identifier.valid(text)
    }
    private static func nativeID(_ bytes: UnsafePointer<UInt8>?) throws -> String {
        guard let bytes else {
            throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Missing native identifier"])
        }
        return Identifier.encode(Array(UnsafeBufferPointer(start: bytes, count: 16)))
    }
    static func decodeDeveloper(_ pointer: OpaquePointer) throws -> [String: Any]? {
        let fields = pb_ios_reply_developer(pointer)
        guard fields.kind != 0 else { return nil }
        func text(_ field: StatusText, limit: Int) throws -> String {
            guard let bytes = field.bytes, field.len >= 0, field.len <= limit,
                let value = String(bytes: UnsafeBufferPointer(start: bytes, count: field.len), encoding: .utf8) else { throw AppPage.Failure.invalid }
            return value
        }
        switch fields.kind {
        case 1:
            guard (0...32).contains(fields.count) else { throw AppPage.Failure.invalid }
            var rows: [[String: Any]] = []; rows.reserveCapacity(fields.count)
            for index in 0..<fields.count {
                let row = pb_ios_reply_app(pointer, index)
                guard (1...3).contains(row.state) else { throw AppPage.Failure.invalid }
                let state = row.state == 1 ? "active" : (row.state == 2 ? "disabled" : "deleted")
                rows.append(["identity": ["publisher": try nativeID(row.publisher), "app_id": try text(row.app_id, limit: 64)],
                    "state": state, "build_id": try text(row.build, limit: 44)])
            }
            return ["apps": rows, "next": fields.next.bytes != nil ? try text(fields.next, limit: 64) as Any : NSNull()]
        case 2:
            return ["build_id": try text(fields.build, limit: 64), "name": try text(fields.name, limit: 256)]
        default: throw AppPage.Failure.invalid
        }
    }
    static func decodeStatus(_ pointer: OpaquePointer) throws -> [String: Any]? {
        let fields = pb_ios_reply_status(pointer)
        guard fields.account.bytes != nil else { return nil }
        func invalid() -> NSError {
            NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native status snapshot"])
        }
        func text(_ field: StatusText, limit: Int = 4 * 1024 * 1024) throws -> String {
            guard let bytes = field.bytes, field.len >= 0, field.len <= limit,
                let text = String(bytes: UnsafeBufferPointer(start: bytes, count: field.len), encoding: .utf8) else { throw invalid() }
            return text
        }
        guard fields.enrolled <= 1, fields.unlocked <= 1, fields.initialized <= 1, fields.replacement_required <= 1, fields.agreement_required <= 1,
            (0...256).contains(fields.sessions), (0...4).contains(fields.examples),
            (0...65536).contains(fields.volumes), (0...65536).contains(fields.invitations) else { throw invalid() }
        var sessions: [[String: Any]] = []; sessions.reserveCapacity(fields.sessions)
        for index in 0..<fields.sessions {
            let row = pb_ios_status_session(pointer, index)
            var session: [String: Any] = ["id": try text(row.id, limit: 64), "resolved_build_id": try text(row.build, limit: 64),
                "root_volume": try nativeID(row.root), "app_name": try text(row.name, limit: 256),
                "build_state": ["kind": try text(row.build_kind, limit: 32)]]
            if row.build_message.bytes != nil { session["build_message"] = try text(row.build_message, limit: 8192) }
            if row.reference.bytes != nil {
                let reference = try text(row.reference, limit: 22)
                guard Identifier.valid(reference) else { throw invalid() }
                session["reference"] = reference
            }
            sessions.append(session)
        }
        var volumes: [[String: Any]] = []; volumes.reserveCapacity(fields.volumes)
        for index in 0..<fields.volumes {
            let row = pb_ios_status_volume(pointer, index)
            volumes.append(["id": try nativeID(row.id), "name": try text(StatusText(bytes: row.name, len: row.name_len))])
        }
        var examples: [[String: Any]] = []; examples.reserveCapacity(fields.examples)
        for index in 0..<fields.examples {
            let row = pb_ios_status_example(pointer, index)
            examples.append(["id": try text(row.id, limit: 32), "name": try text(row.name, limit: 64),
                "description": try text(row.description, limit: 512), "invitation": try text(row.invitation, limit: 128),
                "source": try text(row.source, limit: 512)])
        }
        var invitations: [[String: Any]] = []; invitations.reserveCapacity(fields.invitations)
        for index in 0..<fields.invitations {
            guard let preview = try decodeInvitation(pb_ios_status_invitation(pointer, index)) else { throw invalid() }
            invitations.append(preview)
        }
        var profile: [String: Any] = ["agreement_required": fields.agreement_required != 0, "unlocked": fields.unlocked != 0, "initialized": fields.initialized != 0, "replacement_required": fields.replacement_required != 0]
        if fields.display_name.bytes != nil { profile["display_name"] = try text(fields.display_name) }
        let backup = try text(fields.account_key_backup, limit: 16)
        guard ["unavailable", "pending", "saved", "failed"].contains(backup) else { throw invalid() }
        var result: [String: Any] = ["account_key_backup": backup, "account": ["id": try text(fields.account, limit: 22)], "enrolled": fields.enrolled != 0,
            "account_profile": profile, "sessions": sessions, "volumes": volumes, "examples": examples, "invitations": invitations]
        if fields.email.bytes != nil { result["email"] = try text(fields.email, limit: 254) }
        if let watch = try decodeInvitation(pb_ios_status_watch(pointer)), let status = watch["invitation_status"] {
            result["invitation_status"] = status
        }
        return result
    }
    static func decodePersonalVolumes(_ pointer: OpaquePointer) throws -> [String: Any]? {
        let fields = pb_ios_reply_personal_volumes(pointer)
        guard let session = fields.session else { return nil }
        func invalid() -> NSError {
            NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native personal volume list"])
        }
        func text(_ bytes: UnsafePointer<UInt8>?, _ count: Int, limit: Int) throws -> String {
            guard let bytes, count >= 0, count <= limit,
                let value = String(bytes: UnsafeBufferPointer(start: bytes, count: count), encoding: .utf8) else { throw invalid() }
            return value
        }
        guard fields.volumes >= 0, fields.grants >= 0,
            fields.has_grants <= 1, fields.has_grants != 0 || fields.grants == 0 else { throw invalid() }
        guard fields.volumes <= 128, fields.grants <= 128 else { throw PersonalVolumeAccess.Failure.tooLarge }
        var volumes: [[String: Any]] = []; volumes.reserveCapacity(fields.volumes)
        for index in 0..<fields.volumes {
            let row = pb_ios_reply_personal_volume(pointer, index)
            volumes.append(["id": try nativeID(row.id), "name": try text(row.name, row.name_len, limit: 256)])
        }
        var grants: [[String: Any]] = []; grants.reserveCapacity(fields.grants)
        for index in 0..<fields.grants {
            let grant = pb_ios_reply_personal_grant(pointer, index)
            guard grant.write <= 1 else { throw invalid() }
            grants.append(["volume": try nativeID(grant.id), "write": grant.write != 0])
        }
        return ["session": try text(session, fields.session_len, limit: 64), "volumes": volumes,
            "grants": fields.has_grants != 0 ? grants as Any : NSNull()]
    }
    static func decodeInvitation(_ fields: InvitationFields) throws -> [String: Any]? {
        guard fields.category != 0 else { return nil }
        func invalid() -> NSError {
            NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native invitation result"])
        }
        func text(_ bytes: UnsafePointer<UInt8>?, _ count: Int) throws -> String {
            guard let bytes, count >= 0, count <= 4 * 1024 * 1024,
                let value = String(bytes: UnsafeBufferPointer(start: bytes, count: count), encoding: .utf8) else { throw invalid() }
            return value
        }
        guard fields.paused <= 1, fields.retained <= 1 else { throw invalid() }
        switch fields.category {
        case 1:
            var value: [String: Any] = ["session": try text(fields.session, fields.session_len),
                "paused": fields.paused != 0, "retained": fields.retained != 0]
            if let root = fields.root_name { value["received"] = ["root_name": try text(root, fields.root_name_len)] }
            return value
        case 2, 3:
            var value: [String: Any] = [:]
            if fields.category == 3 { value["paused"] = fields.paused != 0 }
            if fields.state != 0 {
                let kinds: [UInt32: String] = [1: "paused", 2: "checking", 3: "sign_in", 4: "waiting_registration",
                    5: "waiting_app", 6: "waiting_database", 7: "waiting_peer", 8: "ready", 9: "denied", 10: "failed"]
                guard let kind = kinds[fields.state] else { throw invalid() }
                var state: [String: Any] = ["kind": kind]
                if fields.state == 8 { state["build_id"] = try text(fields.build, fields.build_len) }
                if fields.state >= 9 { state["message"] = try text(fields.message, fields.message_len) }
                value["invitation_status"] = ["session": try text(fields.session, fields.session_len), "state": state]
            }
            return value
        case 4:
            return ["invitation": try text(fields.invitation, fields.invitation_len),
                "availability": try text(fields.availability, fields.availability_len)]
        default: throw invalid()
        }
    }
    static func decodeControl(_ fields: ControlFields) throws -> [String: Any]? {
        guard fields.kind != 0 else { return nil }
        func invalid() -> NSError {
            NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native control result"])
        }
        switch fields.kind {
        case 12:
            guard fields.flag <= 1 else { throw invalid() }
            if fields.flag == 0 { return [:] }
            guard let email = fields.text, fields.text_len >= 0, fields.text_len <= 254,
                  let key = fields.secret, fields.secret_len > 0, fields.secret_len <= 128,
                  let email = String(bytes: UnsafeBufferPointer(start: email, count: fields.text_len), encoding: .utf8),
                  let key = String(bytes: UnsafeBufferPointer(start: key, count: fields.secret_len), encoding: .utf8) else { throw invalid() }
            return ["saved_account": ["email": email, "account_key": key]]
        case 13:
            guard fields.flag == 1 else { throw invalid() }
            return ["agreement_accepted": true]
        case 1, 3, 4, 5:
            guard fields.flag <= 1 else { throw invalid() }
            let key: String
            switch fields.kind {
            case 1: key = "personal_volumes_selected"
            case 3: key = "sign_in_available"
            case 4: key = "code_sent"
            default: key = "ordered"
            }
            return [key: fields.flag != 0]
        case 2, 6, 7, 8, 9, 10, 11:
            guard let bytes = fields.text, fields.text_len >= 0, fields.text_len <= 4 * 1024 * 1024,
                let text = String(bytes: UnsafeBufferPointer(start: bytes, count: fields.text_len), encoding: .utf8) else { throw invalid() }
            switch fields.kind {
            case 7:
                guard validInstance(text) else { throw invalid() }
                return ["quit": text, "drain_failed": fields.flag != 0]
            case 9:
                guard validInstance(text), fields.generation > 0 else { throw invalid() }
                return ["instance_id": text, "generation": fields.generation]
            case 11:
                guard ["unavailable", "pending", "saved", "failed"].contains(text) else { throw invalid() }
                return ["account_key_backup": text]
            case 10:
                guard Identifier.valid(text, bytes: 32) else { throw invalid() }
                return ["prepared_session": text]
            default: return [fields.kind == 2 ? "display_name" : (fields.kind == 6 ? "forgotten" : "id"): text]
            }
        default: throw invalid()
        }
    }
    static func decodeTransfer(_ fields: TransferFields) throws -> [String: Any]? {
        guard fields.kind != 0 else { return nil }
        func invalid() -> NSError {
            NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native transfer status"])
        }
        func text(_ bytes: UnsafePointer<UInt8>?, _ count: Int) throws -> String {
            guard let bytes, count >= 0, count <= 4 * 1024 * 1024,
                let text = String(bytes: UnsafeBufferPointer(start: bytes, count: count), encoding: .utf8) else { throw invalid() }
            return text
        }
        let kinds: [UInt32: String] = [1: "idle", 2: "running", 3: "complete", 4: "paused", 5: "canceling", 6: "failed"]
        guard let kind = kinds[fields.kind] else { throw invalid() }
        var value: [String: Any] = ["kind": kind]
        if let session = fields.session { value["session"] = try text(session, fields.session_len) }
        if let message = fields.message { value["message"] = try text(message, fields.message_len) }
        if fields.update_kind != 0 {
            let kinds: [UInt32: String] = [1: "failed", 2: "paused"]
            guard let kind = kinds[fields.update_kind] else { throw invalid() }
            var update: [String: Any] = ["kind": kind, "message": try text(fields.update_message, fields.update_message_len)]
            if let build = fields.update_build { update["build_id"] = try text(build, fields.update_build_len) }
            value["update"] = update
        }
        return value
    }
    static func decodePresentation(_ pointer: OpaquePointer) throws -> [String: Any]? {
        let fields = pb_ios_reply_presentation(pointer)
        guard fields.present != 0 else { return nil }
        func text(_ bytes: UnsafePointer<UInt8>?, _ count: Int) throws -> String {
            guard let bytes, count >= 0, count <= 4 * 1024 * 1024,
                let value = String(bytes: UnsafeBufferPointer(start: bytes, count: count), encoding: .utf8) else {
                throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native presentation text"])
            }
            return value
        }
        guard fields.present == 1, fields.generation > 0 else {
            throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native session instance"])
        }
        var value: [String: Any] = [
            "session": ["resolved_build_id": try text(fields.build, fields.build_len)],
            "availability": try text(fields.availability, fields.availability_len),
            "instance_id": try nativeID(fields.instance), "generation": fields.generation,
        ]
        if fields.invitation != nil || fields.invitation_len != 0 {
            value["invitation"] = try text(fields.invitation, fields.invitation_len)
        }
        if let scene = pb_ios_reply_scene(pointer) { value["scene"] = try GuestScene.read(scene) }
        if fields.update_kind != 0 {
            let kinds: [UInt32: String] = [1: "failed", 2: "paused"]
            guard let kind = kinds[fields.update_kind] else {
                throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native update state"])
            }
            var update: [String: Any] = ["kind": kind, "message": try text(fields.update_message, fields.update_message_len)]
            if let build = fields.update_build { update["build_id"] = try text(build, fields.update_build_len) }
            value["update"] = update
        }
        return value
    }
    static func decodeLogs(_ pointer: OpaquePointer) throws -> LogSnapshot? {
        let fields = pb_ios_reply_logs(pointer)
        guard let engine = fields.engine else { return nil }
        guard fields.count >= 0, fields.count <= 256, fields.reset == 0 else { throw LogSnapshot.Failure.invalid }
        var lines: [String] = []
        lines.reserveCapacity(fields.count)
        var remaining = 64 * 1024
        for index in 0..<fields.count {
            let line = pb_ios_reply_log_line(pointer, index)
            guard let bytes = line.bytes, line.len >= 0, line.len <= remaining,
                let text = String(bytes: UnsafeBufferPointer(start: bytes, count: line.len), encoding: .utf8) else {
                throw LogSnapshot.Failure.invalid
            }
            remaining -= line.len
            lines.append(text)
        }
        return try LogSnapshot(lines: lines, dropped: fields.dropped, engine: Data(bytes: engine, count: 16),
            instance: fields.instance, next: fields.next, reset: false)
    }
    static func decodeRecovery(_ fields: RecoveryFields) throws -> [String: Any]? {
        guard fields.category != 0 else { return nil }
        var data: [String: Any] = [:]
        switch fields.category {
        case 1:
            let kinds: [UInt32: String] = [1: "ready", 2: "setup", 3: "locked", 4: "canceled"]
            guard let kind = kinds[fields.kind] else { throw recoveryBridgeError() }
            data["kind"] = kind
        case 2: break
        default: throw recoveryBridgeError()
        }
        for (key, text) in [("secret", fields.secret)] {
            if let bytes = text.bytes {
                guard text.len <= 4 * 1024 * 1024,
                    let value = String(bytes: UnsafeBufferPointer(start: bytes, count: text.len), encoding: .utf8) else {
                    throw recoveryBridgeError()
                }
                data[key] = value
            } else if text.len != 0 { throw recoveryBridgeError() }
        }
        for (key, number) in [("revision", fields.revision), ("restored_sessions", fields.restored_sessions),
            ("restored_volumes", fields.restored_volumes)] {
            if number.present != 0 { data[key] = number.value }
        }
        if fields.pending_save.present != 0 { data["pending_save"] = fields.pending_save.value != 0 }
        return data
    }
    private static func recoveryBridgeError() -> NSError {
        NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native recovery reply"])
    }
    private func failure(_ text: String, code: Int = 1) -> NSError {
        NSError(domain: "Playbit", code: code, userInfo: [NSLocalizedDescriptionKey: text])
    }
    private func signal(_ error: String? = nil) {
        if let error {
            // Keep one bounded error, never a payload or unbounded main-queue log.
            let text = String(decoding: error.utf8.prefix(508), as: UTF8.self)
            noticeLock.lock(); noticeError = text; noticeLock.unlock()
        }
        changeSource?.add(data: 1)
    }
    private func accept(_ reply: OwnerReply) throws {
        if let completion = reply.completion {
            guard let request = pending.removeValue(forKey: completion.id) else {
                throw failure("Unrecognized runtime response; an action must not be repeated automatically")
            }
            finish(request, completion.result, releaseDelivery: true)
        }
        if let error = reply.backgroundError { signal(error) }
        if let next = reply.changes, next != changes {
            changes = next
            providerStatePending = true
            signal()
        }
        retryProviderState()
        if !suspended, let nanoseconds = reply.wakeAfter {
            deadlineSource?.schedule(deadline: .now() + .nanoseconds(Int(min(nanoseconds, UInt64(Int.max)))), leeway: .nanoseconds(0))
        } else {
            deadlineSource?.schedule(deadline: .distantFuture)
        }
    }
    private func retryProviderState() {
        #if os(iOS)
        guard !suspended else { return }
        ProviderApplication.retryPending()
        guard let handle else { return }
        do {
            if providerStatePending {
                try ProviderApplication.update(handle)
                providerStatePending = false
            }
            retryProviderPush(handle)
            if providerStateError != nil { providerStateError = nil; signal() }
        } catch {
            let message = "Files access is unavailable: " + error.localizedDescription
            if providerStateError != message { providerStateError = message; signal(message) }
        }
        #endif
    }
    #if os(iOS)
    func providerTokenChanged(_ token: Data, invalidated: Bool = false) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !token.isEmpty, token.count <= 128 else { return }
        if providerTokenSource == nil {
            let source = DispatchSource.makeUserDataAddSource(queue: worker)
            source.setEventHandler { [weak self] in
                guard let self else { return }
                self.providerTokenLock.lock()
                let token = self.latestProviderToken
                self.latestProviderToken = nil
                self.providerTokenLock.unlock()
                guard let token else { return }
                self.providerToken = token
                self.providerPushAttempt = nil
                self.retryProviderState()
            }
            providerTokenSource = source
            source.resume()
        }
        providerTokenLock.lock(); latestProviderToken = (token, invalidated); providerTokenLock.unlock()
        providerTokenSource?.add(data: 1)
    }
    private func retryProviderPush(_ handle: OpaquePointer) {
        // Capture only one current cut. Updates while the remote job runs replace
        // the token/ledger state and are admitted after its completion.
        guard let token = providerToken, channel == .public else { return }
        let connection = pb_ios_provider_push_generation(handle)
        guard connection != 0 else { return }
        do {
            let ledger = try ProviderLedger.shared()
            guard let account = try ledger.withLock(exclusive: false, { try ledger.read() }) else {
                providerPushAttempt = nil; return
            }
            let signature = account.generation + ":" + String(connection) + ":" + token.bytes.base64EncodedString() + ":" + String(token.invalidated) + ":" + account.volumes.map(\.id).sorted().joined(separator: ":")
            if providerPushRunning {
                providerPushPending = signature != providerPushInFlightSignature
                return
            }
            guard signature != providerPushAttempt else { return }
            let bundle = Bundle.main.bundleIdentifier!
            let sandbox = Bundle.main.object(forInfoDictionaryKey: "PlaybitPushEnvironment") as? String == "development"
            var job: OpaquePointer?
            try checkNativeError(Array(token.bytes).withUnsafeBufferPointer { bytes in
                account.account.withCString { account in
                    bundle.withCString { bundle in
                        pb_ios_provider_push_prepare(handle, account, bytes.baseAddress, token.bytes.count, token.invalidated ? 1 : 0, bundle, sandbox ? 1 : 0, &job)
                    }
                }
            })
            // No exposed locations is a local no-op, never an invalidation of
            // a newer endpoint whose catalog may have arrived independently.
            guard let job else { providerPushAttempt = signature; return }
            providerPushError = false
            providerPushRunning = true
            providerPushAttempt = signature
            providerPushInFlightSignature = signature
            providerPushWorker.async {
                let error = pb_ios_provider_push_run(job)
                let succeeded = error == nil
                if let error { pb_ios_string_free(error) }
                self.worker.async {
                    self.providerPushRunning = false
                    self.providerPushInFlightSignature = nil
                    let pending = self.providerPushPending
                    self.providerPushPending = false
                    // A failed attempted cut retries on a new connection
                    // generation, changed catalog or explicit token callback.
                    // Ordinary local host steps cannot replay the same cut.
                    if succeeded || pending { self.retryProviderState() }
                }
            }
        } catch {
            if !providerPushError { NSLog("Playbit Files push registration remains pending") }
            providerPushError = true
        }
    }
    #endif
    private func observeCompletions() throws {
        #if os(iOS)
        if providerNotification < 0 {
            guard notify_register_dispatch(ProviderLedger.publicationNeeded, &providerNotification, worker, { [weak self] _ in
                self?.providerStatePending = true
                self?.retryProviderState()
            }) == NOTIFY_STATUS_OK else { throw failure("Files account notification is unavailable") }
        }
        #endif
        var descriptor: Int32 = -1
        try checkNativeError(pb_ios_completion_descriptor(handle, &descriptor))
        guard descriptor >= 0 else {
            throw failure("Missing runtime completion descriptor")
        }
        let source = DispatchSource.makeReadSource(fileDescriptor: descriptor, queue: worker)
        source.setEventHandler { [weak self] in
            guard let self, let handle = self.handle else { return }
            do { try self.accept(self.decode(pb_ios_complete_background(handle))) }
            catch { self.failRuntime(error) }
        }
        source.setCancelHandler { close(descriptor) }
        completionSource = source
        let deadline = DispatchSource.makeTimerSource(queue: worker)
        deadline.schedule(deadline: .distantFuture)
        deadline.setEventHandler { [weak self] in
            guard let self, !self.suspended, let handle = self.handle else { return }
            self.deadlineSource?.schedule(deadline: .distantFuture)
            do { try self.accept(self.decode(pb_ios_wake(handle))) }
            catch { self.failRuntime(error) }
        }
        deadlineSource = deadline
        deadline.resume()
        source.resume()
    }
    func suspend(completion: @escaping () -> Void) {
        worker.async {
            self.suspended = true
            self.deadlineSource?.schedule(deadline: .distantFuture)
            if let handle = self.handle {
                do { try self.checkNativeError(pb_ios_suspend(handle)) }
                catch { self.failRuntime(error) }
            }
            DispatchQueue.main.async { completion() }
        }
    }
    func resume() {
        worker.async {
            guard self.suspended else { return }
            self.suspended = false
            if let handle = self.handle {
                do { try self.accept(self.decode(pb_ios_resume(handle))) }
                catch { self.failRuntime(error) }
            }
        }
    }
    private func failRuntime(_ error: Error) {
        guard terminalError == nil else { return }
        let text = String(decoding: error.localizedDescription.utf8.prefix(508), as: UTF8.self)
        let failure = error as NSError
        let terminal = self.failure(text, code: failure.domain == "Playbit" ? failure.code : 1)
        terminalError = terminal
        completionSource?.cancel(); completionSource = nil
        deadlineSource?.cancel(); deadlineSource = nil
        signal(text)
        let failed = pending.values
        pending = [:]
        for request in failed { finish(request, .failure(terminal), releaseDelivery: false) }
        // Keep the handle alive until process shutdown. Already dispatched
        // writes are never replayed or described as rolled back.
    }
    private func finish(_ request: Pending, _ result: Result<[String: Any], Error>, releaseDelivery: Bool) {
        let completed = pb_ios_trace_start()
        let deliver = {

            pb_ios_trace_end(PB_IOS_TRACE_WORKER_DELIVERY, completed)
            self.admittedCount -= 1
            self.admittedBytes -= request.bytes
            request.completion(result)
            if releaseDelivery {
                self.worker.async {

                    guard self.terminalError == nil, let handle = self.handle else { return }
                    do { try self.accept(self.decode(pb_ios_delivery_ready(handle))) }
                    catch { self.failRuntime(error) }
                }
            }
            #if PHASE1_QUALIFICATION
            // Stop a real guest after returning its first populated log snapshot.
            // The following read must fail through the ordinary runtime path.
            if !self.stoppedLogFixture,
               ProcessInfo.processInfo.arguments.contains("--phase1-stop-logged-session"),
               case .success(let value) = result, let logs = value["logs"] as? LogSnapshot, !logs.lines.isEmpty,
               case .control(.appLogs(let session)) = request.payload {
                self.stoppedLogFixture = true
                self.request(.quitApp(session)) { _ in }
            }
            #endif
        }
        #if PHASE1_QUALIFICATION
        // Hold one real result/its existing delivery credit for the native
        // busy-navigation regression. This fault delay is absent from product builds.
        if ProcessInfo.processInfo.arguments.contains("--phase1-delay-consent-response"),
           case .control(.selectPersonalVolumes) = request.payload {
            DispatchQueue.main.asyncAfter(deadline: .now() + 3, execute: deliver)
            return
        }
        if ProcessInfo.processInfo.arguments.contains("--phase1-delay-invitation-preview"),
           case .control(.previewInvitation) = request.payload {
            DispatchQueue.main.asyncAfter(deadline: .now() + 3, execute: deliver)
            return
        }
        #endif
        DispatchQueue.main.async(execute: deliver)
    }
    private func submit(_ request: Pending) {

        if let error = terminalError {
            finish(request, .failure(error), releaseDelivery: false)
            return
        }
        guard !suspended else {
            finish(request, .failure(failure("Playbit is paused. Return to the app to continue.")), releaseDelivery: false)
            return
        }
        pending[request.id] = request
        do {
            if handle == nil {
                let started = Date()
                guard let channel else { throw failure("Choose an environment before opening Playbit.") }
                _ = try profile.path.withCString { try decode(pb_ios_open($0, channel.bridgeValue, &handle)) }
                preferences.set(channel.rawValue, forKey: "PlaybitChannel")
                try observeCompletions()
                #if PHASE1_QUALIFICATION
                if ProcessInfo.processInfo.arguments.contains("--phase1-qualify") {
                    guard let bundle = Bundle.main.url(forResource: "starter", withExtension: "pbapp") else { throw failure("Qualification starter.pbapp is missing") }
                    _ = try bundle.path.withCString { try decode(pb_ios_qualification_seed(handle, $0,
                        ProcessInfo.processInfo.arguments.contains("--phase1-database-browser") ? 2 : ProcessInfo.processInfo.arguments.contains("--phase1-personal-volumes") ? 1 : 0,
                        ProcessInfo.processInfo.arguments.contains("--phase1-account-profile")
                            || ProcessInfo.processInfo.arguments.contains("--phase1-account-settings") ? 1 : 0,
                        ProcessInfo.processInfo.arguments.contains("--phase1-session-list") ? 1 : 0)) }
                    if ProcessInfo.processInfo.arguments.contains("--phase1-database-browser") {
                        let source = profile.appendingPathComponent("qualification-settings.pbdb")
                        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
                        let destination = documents.appendingPathComponent("Settings.pbdb")
                        if FileManager.default.fileExists(atPath: source.path), !FileManager.default.fileExists(atPath: destination.path) {
                            try FileManager.default.createDirectory(at: documents, withIntermediateDirectories: true)
                            try FileManager.default.copyItem(at: source, to: destination)
                        }
                    }
                }
                #endif
                NSLog("Playbit local initialization: %.3f ms", Date().timeIntervalSince(started) * 1000)
            }
            switch request.payload {
            case .control(let control):
                try accept(decode(control.submit(handle, id: request.id)))
            case .input(let instance, let kind, let target, let objectID, let data):
                #if PHASE1_QUALIFICATION
                if ProcessInfo.processInfo.arguments.contains("--w7-panic-on-input") { pb_ios_qualification_panic() }
                #endif
                let instanceBytes = Array(instance.utf8)
                try accept(instanceBytes.withUnsafeBufferPointer { instance in
                    try data.withUnsafeBytes { bytes in
                        try decode(pb_ios_app_input(handle, request.id, instance.baseAddress, instance.count,
                            kind, target, objectID, bytes.bindMemory(to: UInt8.self).baseAddress, bytes.count))
                    }
                })
            }
        } catch { failRuntime(error) }
    }
    func request(_ control: Control, completion: @escaping (Result<[String: Any], Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        do { try control.validate(); try enqueue(.control(control), bytes: control.bytes, completion: completion) }
        catch { completion(.failure(error)) }
    }
    func input(instance: String, kind: Int, target: UInt32, objectID: UInt32 = 0, data: Data,
               completion: @escaping (Result<[String: Any], Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        do {
            guard let kind = UInt32(exactly: kind), Self.validInstance(instance),
                  data.count <= (kind == 14 ? GuestTextEdits.payloadBytesMax : GuestTextEdits.textBytesMax) else { throw failure("Invalid guest input") }
            try enqueue(.input(instance: instance, kind: kind, target: target, objectID: objectID, data: data),
                        bytes: 32 + 8 + data.count, completion: completion)
        } catch { completion(.failure(error)) }
    }
    private func enqueue(_ payload: Payload, bytes: Int,
                         completion: @escaping (Result<[String: Any], Error>) -> Void) throws {
        guard !changingAccount else { throw failure("An account change is in progress.") }
        guard admittedCount < 32, bytes <= 4 * 1024 * 1024 - admittedBytes else {
            throw failure("Playbit is busy. This action did not start; try again when current work finishes.")
        }
        guard nextID != UInt64.max else { throw failure("Runtime request identifiers exhausted; restart Playbit") }
        admittedCount += 1
        admittedBytes += bytes
        let request = Pending(id: nextID, payload: payload, bytes: bytes,
            completion: completion)
        nextID += 1
        worker.async { self.submit(request) }
    }

    func resolveReplacement(_ replace: Bool, completion: @escaping (Result<Void, Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !changingAccount, admittedCount == 0 else {
            completion(.failure(failure("Playbit is finishing another action. Try again.")))
            return
        }
        changingAccount = true
        worker.async {
            let result = Result<Void, Error> {
                if let handle = self.handle {
                    try self.checkNativeError(pb_ios_stop_for_replacement(handle))
                    #if os(iOS)
                    try ProviderApplication.disable()
                    #endif
                    self.completionSource?.cancel(); self.completionSource = nil
                    self.deadlineSource?.cancel(); self.deadlineSource = nil
                    pb_ios_close(handle)
                    self.handle = nil
                }
                try self.profile.path.withCString {
                    try self.checkNativeError(pb_ios_resolve_replacement($0, replace ? 1 : 0))
                }
                self.terminalError = nil
                self.changes = nil
                self.providerStatePending = true
            }
            DispatchQueue.main.async {
                self.changingAccount = false
                completion(result)
            }
        }
    }

    func signOut(retireApps: @escaping () -> Void, completion: @escaping (Result<Void, Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !changingAccount, admittedCount == 0 else {
            completion(.failure(failure("Playbit is finishing another action. Try again.")))
            return
        }
        changingAccount = true
        worker.async {
            var closed = false
            let result = Result<Void, Error> {
                if let error = self.terminalError { throw error }
                guard let handle = self.handle else { throw self.failure("Reopen Account and try again.") }
                try self.checkNativeError(pb_ios_stop_for_sign_out(handle))
                    #if os(iOS)
                    try ProviderApplication.disable()
                    #endif
                self.completionSource?.cancel(); self.completionSource = nil
                self.deadlineSource?.cancel(); self.deadlineSource = nil
                pb_ios_close(handle)
                self.handle = nil
                closed = true
                self.changes = nil
                self.providerStatePending = true
                try self.profile.path.withCString { try self.checkNativeError(pb_ios_keep_profile($0)) }
            }
            DispatchQueue.main.async {
                if closed { retireApps() }
                self.changingAccount = false
                completion(result)
            }
        }
    }

    func switchChannel(_ next: ReleaseChannel, retireApps: @escaping () -> Void,
                       completion: @escaping (Result<Void, Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard !changingAccount, admittedCount == 0 else {
            completion(.failure(failure("Playbit is finishing another action. Try again.")))
            return
        }
        guard next != channel else { completion(.success(())); return }
        changingAccount = true
        retireApps()
        worker.async {
            let result = Result<Void, Error> {
                if let handle = self.handle {
                    if let error = self.terminalError { throw error }
                    try self.checkNativeError(pb_ios_stop_for_sign_out(handle))
                    #if os(iOS)
                    try ProviderApplication.disable()
                    #endif
                    self.completionSource?.cancel(); self.completionSource = nil
                    self.deadlineSource?.cancel(); self.deadlineSource = nil
                    pb_ios_close(handle)
                    self.handle = nil
                }
                self.terminalError = nil
                self.changes = nil
                self.providerStatePending = true
                self.noticeLock.lock(); self.noticeError = nil; self.noticeLock.unlock()
                self.preferences.set(next.rawValue, forKey: "PlaybitChannel")
            }
            DispatchQueue.main.async {
                if case .success = result { self.channel = next }
                self.changingAccount = false
                completion(result)
            }
        }
    }

    func resetCorruptStore(completion: @escaping (Result<Void, Error>) -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        worker.async {
            let result: Result<Void, Error>
            if self.terminalError?.code != 2 || self.handle != nil {
                result = .failure(self.failure("The corrupt store is not closed. Reopen Playbit and try again."))
            } else {
                do {
                    try self.profile.path.withCString { try self.checkNativeError(pb_ios_reset_profile($0)) }
                    self.terminalError = nil
                    result = .success(())
                } catch {
                    result = .failure(error)
                }
            }
            DispatchQueue.main.async { completion(result) }
        }
    }
}
