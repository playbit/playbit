import Foundation
import FileProvider

/// Reconcile one account location, retaining unavailable locations because iOS
/// cannot preserve an OS-held edit while removing its File Provider domain.
final class ProviderDomains {
    static let shared = ProviderDomains()
    private var running = false
    private var pending: (ProviderLedger, ProviderAccount?)?
    private var activeAccount: ProviderAccount?
    private var signaling = false
    private var dirty = false
    private var recoveryPending = false
    private var acknowledge: (() -> Void)?

    func reconcile(ledger: ProviderLedger, account: ProviderAccount?, volumes: [ProviderVolume]) {
        dispatchPrecondition(condition: .onQueue(.main))
        if activeAccount?.account != account?.account || activeAccount?.generation != account?.generation {
            dirty = false; recoveryPending = false; acknowledge = nil
        }
        activeAccount = account
        pending = (ledger, account)
        run()
        retrySignals()
    }
    func signal(account: String, volumes: Set<String>) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard activeAccount?.account == account else { return }
        dirty = true
        retrySignals()
    }
    func signal(account: String, generation: String, acknowledge: @escaping () -> Void) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard activeAccount?.account == account, activeAccount?.generation == generation else { return }
        self.acknowledge = acknowledge
        dirty = true
        retrySignals()
    }
    func connectionResolved(account: String, generation: String) {
        dispatchPrecondition(condition: .onQueue(.main))
        guard activeAccount?.account == account, activeAccount?.generation == generation else { return }
        recoveryPending = true
        retrySignals()
    }
    func retryPending() {
        dispatchPrecondition(condition: .onQueue(.main))
        run(); retrySignals()
    }
    private func retrySignals() {
        guard !signaling, let account = activeAccount, dirty || recoveryPending else { return }
        let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(account.account), displayName: "Playbit")
        guard let manager = NSFileProviderManager(for: domain) else { return }
        signaling = true
        if recoveryPending {
            recoveryPending = false
            manager.signalErrorResolved(NSFileProviderError(.serverUnreachable)) { error in
                DispatchQueue.main.async {
                    self.signaling = false
                    guard self.activeAccount?.generation == account.generation else { self.retrySignals(); return }
                    if error != nil { self.recoveryPending = true }
                    else { self.retrySignals() }
                }
            }
            return
        }
        dirty = false
        let acknowledge = self.acknowledge
        self.acknowledge = nil
        manager.signalEnumerator(for: .workingSet) { error in
            DispatchQueue.main.async {
                self.signaling = false
                guard self.activeAccount?.generation == account.generation else { self.retrySignals(); return }
                if error != nil {
                    self.dirty = true
                    if self.acknowledge == nil { self.acknowledge = acknowledge }
                    NSLog("Playbit Files change signal failed")
                } else { acknowledge?(); self.retrySignals() }
            }
        }
    }
    private func run() {
        guard !running, let (ledger, account) = pending else { return }
        pending = nil; running = true
        NSFileProviderManager.getDomainsWithCompletionHandler { domains, error in
            DispatchQueue.main.async {
                guard error == nil else { self.finish(ledger, account, failed: true); return }
                do {
                    let current = try ledger.withLock(exclusive: false) { try ledger.read() }
                    guard current?.account == account?.account, current?.generation == account?.generation else {
                        self.running = false
                        self.reconcile(ledger: ledger, account: current, volumes: current?.volumes ?? [])
                        return
                    }
                    let adding = account.map { active in !domains.contains { $0.identifier.rawValue == active.account } } ?? false
                    guard domains.count + (adding ? 1 : 0) <= 256 else { throw CocoaError(.fileWriteOutOfSpace) }
                    let ids = Set(domains.map { $0.identifier.rawValue }.filter { Identifier.valid($0) })
                    try ledger.reserveDomains(ids.union(account.map { [$0.account] } ?? []))
                } catch { self.finish(ledger, account, failed: true); return }
                let group = DispatchGroup()
                var failed = false
                for domain in domains where domain.identifier.rawValue != account?.account {
                    let id = domain.identifier.rawValue
                    // Superseded per-volume domains remain recovery sources;
                    // never remove a location which may contain pending edits.
                    let name = Identifier.valid(id) ? "Playbit (Unavailable, " + id + ")" : domain.displayName + " (Unavailable)"
                    guard domain.displayName != name, !domain.displayName.hasSuffix(" (Unavailable)") else { continue }
                    group.enter()
                    NSFileProviderManager.add(NSFileProviderDomain(identifier: domain.identifier, displayName: name)) { error in
                        DispatchQueue.main.async { if error != nil { failed = true }; group.leave() }
                    }
                }
                if let account {
                    let domain = NSFileProviderDomain(identifier: NSFileProviderDomainIdentifier(account.account), displayName: "Playbit")
                    domain.supportsSyncingTrash = false
                    group.enter()
                    NSFileProviderManager.add(domain) { error in
                        DispatchQueue.main.async { if error != nil { failed = true }; group.leave() }
                    }
                    if let manager = NSFileProviderManager(for: domain) {
                        group.enter()
                        manager.signalErrorResolved(NSFileProviderError(.notAuthenticated)) { error in
                            DispatchQueue.main.async { if error != nil { failed = true }; group.leave() }
                        }
                    }
                }
                group.notify(queue: .main) { self.finish(ledger, account, failed: failed) }
            }
        }
    }
    private func finish(_ ledger: ProviderLedger, _ account: ProviderAccount?, failed: Bool) {
        running = false
        do {
            let current = try ledger.withLock(exclusive: false) { try ledger.read() }
            if current?.account != account?.account || current?.generation != account?.generation {
                pending = (ledger, current)
            } else if failed && pending == nil { pending = (ledger, account); return }
        } catch { if pending == nil { pending = (ledger, account) }; return }
        run()
    }
}
