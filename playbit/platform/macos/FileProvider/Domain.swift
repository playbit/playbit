import FileProvider

/// Public retains the original account domain. Testing has its own Finder
/// identity even when the two services have enrolled the same account key.
struct DriveIdentity: Equatable {
    enum Channel: String {
        case production = "public"
        case testing
    }

    let channel: Channel
    let account: String

    init?(channel: Channel, account: String) {
        guard Identifier.valid(account) else { return nil }
        self.channel = channel
        self.account = account
    }

    init?(_ id: NSFileProviderDomainIdentifier) {
        let text = id.rawValue
        guard text.utf8.count <= 30 else { return nil }
        if text.hasPrefix("testing-") {
            self.init(channel: .testing, account: String(text.dropFirst("testing-".count)))
        } else {
            self.init(channel: .production, account: text)
        }
    }

    var identifier: NSFileProviderDomainIdentifier {
        NSFileProviderDomainIdentifier(channel == .testing ? "testing-" + account : account)
    }

    func domain(profile: String) -> NSFileProviderDomain {
        let name = channel == .testing ? "Playbit (testing)" : "Playbit"
        let domain = NSFileProviderDomain(identifier: identifier, displayName: name)
        domain.userInfo = ["profile": profile]
        domain.supportsSyncingTrash = false
        return domain
    }
}

/// Re-adding an existing identifier updates its metadata in place. Removal is
/// reserved for signed-out or previous accounts within the same profile.
struct DriveRegistration {
    let remove: [NSFileProviderDomain]
    let active: NSFileProviderDomain?
    let needsRegistration: Bool

    init(current: [NSFileProviderDomain], identity: DriveIdentity, profile: String, ready: Bool) {
        remove = current.filter {
            $0.userInfo?["profile"] as? String == profile &&
                DriveIdentity($0.identifier)?.channel == identity.channel &&
                (!ready || $0.identifier != identity.identifier)
        }
        let existing = current.first { $0.identifier == identity.identifier }
        guard ready, existing == nil || existing?.userInfo?["profile"] as? String == profile else {
            active = nil
            needsRegistration = false
            return
        }
        let domain = identity.domain(profile: profile)
        active = domain
        needsRegistration = existing?.displayName != domain.displayName
    }
}
