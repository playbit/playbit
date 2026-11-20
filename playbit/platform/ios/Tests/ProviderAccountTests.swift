import XCTest
import Foundation

final class ProviderAccountTests: XCTestCase {
    private func snapshot(account: UInt8 = 1, name: String = "Personal") throws -> Data {
        try JSONSerialization.data(withJSONObject: [
            "account": Identifier.encode([UInt8](repeating: account, count: 16)),
            "email": "provider@example.invalid", "secret": [UInt8](repeating: 7, count: 16),
            "volumes": [["id": Identifier.encode([UInt8](repeating: 2, count: 16)), "name": name]],
            "retired": [String](),
        ])
    }
    private func ledger() throws -> ProviderLedger {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return ProviderLedger(root: root)
    }
    func testFullCatalogWithEscapedNamesSurvivesReopening() throws {
        let ledger = try ledger()
        var fields = try XCTUnwrap(JSONSerialization.jsonObject(with: snapshot()) as? [String: Any])
        let name = String(repeating: "\u{0001}", count: 256)
        fields["volumes"] = (0..<256).map { n in
            ["id": Identifier.encode([UInt8(n), 1] + [UInt8](repeating: 0, count: 14)), "name": name]
        }
        let bytes = try JSONSerialization.data(withJSONObject: fields)
        XCTAssertGreaterThan(bytes.count, 256 * 1024)
        XCTAssertLessThan(bytes.count, ProviderLedger.accountBytesMax)
        let published = try XCTUnwrap(ledger.publish(bytes))
        let reopened = try ProviderLedger(root: ledger.root).read()
        XCTAssertEqual(reopened, published)
        XCTAssertEqual(reopened?.volumes.count, 256)
    }
    func testSignOutRevokesStaleMutationAdmissionAndReenrollmentUsesNewGeneration() throws {
        // Publish, rename, revoke and reenable one account through the real
        // atomic ledger; stale native clients must never regain write admission.
        let ledger = try ledger()
        let first = try XCTUnwrap(ledger.publish(snapshot()))
        let renamed = try XCTUnwrap(ledger.publish(snapshot(name: "Renamed")))
        XCTAssertEqual(first.generation, renamed.generation)
        try ledger.authorize(first, mutation: true) {}
        try ledger.publish(nil)
        XCTAssertThrowsError(try ledger.authorize(first, mutation: true) {})
        let second = try XCTUnwrap(ledger.publish(snapshot()))
        XCTAssertNotEqual(first.generation, second.generation)
        XCTAssertThrowsError(try ledger.authorize(first, mutation: true) {}) {
            XCTAssertEqual(($0 as NSError).domain, NSCocoaErrorDomain)
            XCTAssertEqual(($0 as NSError).code, CocoaError.userCancelled.rawValue)
        }
        try ledger.authorize(second, mutation: true) {}
        let other = try XCTUnwrap(ledger.publish(snapshot(account: 3)))
        XCTAssertNotEqual(other.generation, second.generation)
        XCTAssertThrowsError(try ledger.authorize(second, mutation: false) {})
    }
    func testActiveMutationLeaseExcludesAccountPublicationWithoutWaiting() throws {
        // A second ledger descriptor must fail exclusive admission while a real
        // mutation owns a shared lock, then succeed after the commit completes.
        let ledger = try ledger()
        let account = try XCTUnwrap(ledger.publish(snapshot()))
        let peer = ProviderLedger(root: ledger.root)
        let admitted = DispatchSemaphore(value: 0)
        let finish = DispatchSemaphore(value: 0)
        let done = expectation(description: "mutation completes")
        DispatchQueue.global().async {
            defer { done.fulfill() }
            do {
                try peer.authorize(account, mutation: true) {
                    admitted.signal()
                    XCTAssertEqual(finish.wait(timeout: .now() + 5), .success)
                }
            } catch { XCTFail("Mutation admission failed") }
        }
        XCTAssertEqual(admitted.wait(timeout: .now() + 5), .success)
        XCTAssertThrowsError(try ledger.publish(nil))
        finish.signal()
        wait(for: [done], timeout: 5)
        try ledger.publish(nil)
        XCTAssertThrowsError(try peer.authorize(account, mutation: true) {})
    }
    func testAdmittedHydrationDoesNotHoldSignOutBehindNetworkWork() throws {
        // Hydration authorization releases its ledger lock before slow I/O.
        let ledger = try ledger()
        let account = try XCTUnwrap(ledger.publish(snapshot()))
        let peer = ProviderLedger(root: ledger.root)
        try peer.authorize(account, mutation: false) {
            _ = try ledger.publish(nil)
        }
        XCTAssertThrowsError(try peer.authorize(account, mutation: false) {})
    }
    func testMalformedAndOversizedLedgerFailBeforeAnyOperation() throws {
        // Corrupt private state is an explicit failure, not an empty account or
        // permission to use a previous cache.
        let ledger = try ledger()
        _ = try ledger.publish(snapshot())
        for bytes in [Data("{}".utf8), Data(repeating: 1, count: 65537)] {
            try bytes.write(to: ledger.root.appendingPathComponent("account.json"))
            XCTAssertThrowsError(try ledger.withLock(exclusive: false) { try ledger.read() })
        }
    }
    func testDeletedVolumeRevokesStaleReplicaAndCannotBeReenabledByAnOlderCatalog() throws {
        let ledger = try ledger()
        let account = try XCTUnwrap(ledger.publish(snapshot()))
        let volume = account.volumes[0].id
        let peer = ProviderLedger(root: ledger.root)
        try peer.retire(account, volumes: [volume])
        XCTAssertThrowsError(try ledger.authorize(account, volumes: [volume], mutation: true) {})
        XCTAssertThrowsError(try ledger.authorize(account, volumes: [volume], mutation: false) {})
        let stale = try XCTUnwrap(ledger.publish(snapshot()))
        XCTAssertTrue(stale.volumes.isEmpty)
        XCTAssertEqual(stale.retired, [volume])
        XCTAssertThrowsError(try peer.authorize(stale, volumes: [volume], mutation: true) {})
        try ledger.publish(nil)
        _ = try ledger.publish(snapshot(account: 4))
        try ledger.publish(nil)
        let returned = try XCTUnwrap(ledger.publish(snapshot()))
        XCTAssertTrue(returned.volumes.isEmpty)
        XCTAssertEqual(returned.retired, [volume])
        XCTAssertThrowsError(try peer.authorize(returned, volumes: [volume], mutation: true) {})
        let other = Identifier.encode([UInt8](repeating: 3, count: 16))
        try peer.authorize(returned, volumes: [other], mutation: true) {}
        XCTAssertThrowsError(try peer.authorize(returned, volumes: [volume, other], mutation: true) {})
    }
    func testEmptyAccountRetainsOneLocationIndependentlyOfItsVolumeCatalog() throws {
        // Account location admission must not disappear when its last volume
        // disappears, and retained accounts consume a separate bounded ledger.
        let ledger = try ledger()
        var value = try XCTUnwrap(JSONSerialization.jsonObject(with: snapshot()) as? [String: Any])
        value["volumes"] = [[String: String]]()
        let account = try XCTUnwrap(ledger.publish(JSONSerialization.data(withJSONObject: value)))
        XCTAssertTrue(account.volumes.isEmpty)
        try ledger.reserveDomains([account.account])
        try ledger.authorize(account, mutation: false) {}
        try ledger.publish(nil)
        let peer = ProviderLedger(root: ledger.root)
        try peer.reserveDomains([account.account])
        let retained = Set((1...256).map { Identifier.encode($0 <= 128 ? [UInt8](repeating: UInt8($0), count: 16) : [UInt8(truncatingIfNeeded: $0), 1] + [UInt8](repeating: 0, count: 14)) })
        try peer.reserveDomains(retained)
        XCTAssertThrowsError(try peer.reserveDomains([Identifier.encode([UInt8](repeating: 255, count: 16))]))
    }

    func testCreationReservationChecksHistoricalCapacityAndGenerationBeforeAdmission() throws {
        // The App Group can be full after account checkpoints are evicted.
        // Capacity, stale generations and retired IDs must fail before a commit.
        let ledger = try ledger()
        let account = try XCTUnwrap(ledger.publish(snapshot()))
        let historical = (1...256).map { Identifier.encode($0 <= 128 ? [UInt8](repeating: UInt8($0), count: 16) : [UInt8(truncatingIfNeeded: $0), 1] + [UInt8](repeating: 0, count: 14)) }
        try ledger.exposeVolumes(account: account.account, volumes: historical)
        var admitted = false
        func create(_ id: String, expected: ProviderAccount) throws {
            try ledger.exposeVolumes(account: expected.account, volumes: [id], expected: expected)
            try ledger.authorize(expected, volumes: [id], mutation: true) { admitted = true }
        }
        let existing = account.volumes[0].id
        try create(existing, expected: account)
        try create(existing, expected: account)
        XCTAssertTrue(admitted, "An exact reservation replay needs no additional slot")
        admitted = false
        let extra = Identifier.encode([UInt8](repeating: 255, count: 16))
        XCTAssertThrowsError(try create(extra, expected: account)) {
            XCTAssertEqual(($0 as NSError).code, CocoaError.fileWriteOutOfSpace.rawValue)
        }
        XCTAssertFalse(admitted)
        try ledger.publish(nil)
        let reenrolled = try XCTUnwrap(ledger.publish(snapshot()))
        XCTAssertThrowsError(try create(existing, expected: account)) {
            XCTAssertEqual(($0 as NSError).code, CocoaError.userCancelled.rawValue)
        }
        XCTAssertFalse(admitted)
        try create(existing, expected: reenrolled)
        admitted = false
        try ledger.retire(reenrolled, volumes: [existing])
        XCTAssertThrowsError(try create(existing, expected: reenrolled)) {
            XCTAssertEqual(($0 as NSError).code, CocoaError.fileWriteNoPermission.rawValue)
        }
        XCTAssertFalse(admitted)
        XCTAssertEqual(try ledger.knownVolumes(account: account.account).count, 256)
    }

    func testVolumeExposureReservationsSurviveAccountTurnoverAndAreBounded() throws {
        let ledger = try ledger()
        let account = try XCTUnwrap(ledger.publish(snapshot()))
        let reserved = (1...256).map { Identifier.encode($0 <= 128 ? [UInt8](repeating: UInt8($0), count: 16) : [UInt8(truncatingIfNeeded: $0), 1] + [UInt8](repeating: 0, count: 14)) }
        try ledger.exposeVolumes(account: account.account, volumes: reserved)
        let peer = ProviderLedger(root: ledger.root)
        // No OS completion has occurred. A different process still sees every
        // reservation, and sign-out cannot erase those in-flight identities.
        try ledger.publish(nil)
        try peer.exposeVolumes(account: account.account, volumes: reserved)
        let other = Identifier.encode([UInt8](repeating: 3, count: 16))
        XCTAssertThrowsError(try peer.exposeVolumes(account: other, volumes: [account.volumes[0].id]))
        try peer.reserveDomains([account.account, other])
    }
}
