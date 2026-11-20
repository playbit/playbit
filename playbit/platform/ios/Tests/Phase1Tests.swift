import XCTest
import UIKit

final class Phase1Tests: XCTestCase {
    func testReleaseChannelSelectionIsRememberedWithoutEnrollment() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_CHANNEL_QUALIFICATION"] == "1" else {
            throw XCTSkip("Requires a fresh isolated installation for runtime channel checks")
        }
        let app = XCUIApplication()
        app.launchEnvironment["PLAYBIT_CHANNEL"] = "invalid"
        defer { app.terminate() }
        app.launch()
        func openSelector() {
            let available = NSPredicate { _, _ in
                app.buttons["environment-menu"].exists || app.buttons["Sign in"].firstMatch.exists
            }
            expectation(for: available, evaluatedWith: app)
            waitForExpectations(timeout: 20)
            if !app.buttons["environment-menu"].exists { app.buttons["Sign in"].firstMatch.tap() }
            XCTAssertTrue(app.buttons["environment-menu"].waitForExistence(timeout: 20))
        }
        func select(_ channel: String) {
            openSelector()
            let menu = app.buttons["environment-menu"]
            XCTAssertTrue(menu.waitForExistence(timeout: 20))
            menu.tap()
            let choice = app.buttons[channel].firstMatch
            XCTAssertTrue(choice.waitForExistence(timeout: 5))
            choice.tap()
            let completed = NSPredicate { _, _ in
                app.buttons["environment-menu"].label == "Environment: \(channel)"
                    || (!app.buttons["environment-menu"].exists && app.buttons["Sign in"].firstMatch.exists)
            }
            expectation(for: completed, evaluatedWith: app)
            waitForExpectations(timeout: 20)
            openSelector()
            let selected = NSPredicate(format: "exists == true AND label == %@", "Environment: \(channel)")
            expectation(for: selected, evaluatedWith: app.buttons["environment-menu"])
            waitForExpectations(timeout: 20)
            XCTAssertEqual(app.buttons["environment-menu"].label, "Environment: \(channel)")
        }
        openSelector()
        XCTAssertFalse(app.textFields["sign-in-email"].isEnabled)
        XCTAssertTrue(app.buttons["User agreement"].isEnabled)
        XCTAssertTrue(app.buttons["Privacy policy"].isEnabled)
        XCTAssertTrue(app.staticTexts["Choose an environment to continue."].exists)
        select("Public")
        XCTAssertTrue(app.textFields["sign-in-email"].isEnabled)
        select("Testing")
        XCTAssertTrue(app.staticTexts["testing-channel"].waitForExistence(timeout: 5))
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Ordinary Testing sign-in before enrollment"
        capture.lifetime = .keepAlways
        add(capture)
        app.terminate()
        app.launchEnvironment.removeValue(forKey: "PLAYBIT_CHANNEL")
        app.launch()
        openSelector()
        XCTAssertEqual(app.buttons["environment-menu"].label, "Environment: Testing")
        select("Public")
        app.terminate(); app.launch()
        openSelector()
        XCTAssertEqual(app.buttons["environment-menu"].label, "Environment: Public")
    }

    func testDatabaseReferenceOpensFromFilesCold() throws {
        // Use a simulator with only this Playbit build installed; all builds own the same UTI.
        // The unique display name also identifies this fixture's Documents folder in Files.
        guard let appName = ProcessInfo.processInfo.environment["PLAYBIT_DOCUMENT_APP_NAME"] else {
            throw XCTSkip("Requires an isolated simulator installation with a unique display name")
        }
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-database-browser"]
        app.launch()
        XCTAssertTrue(app.buttons["Volumes"].waitForExistence(timeout: 20))
        app.terminate()
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        defer { files.terminate(); app.terminate() }
        files.launch()
        let browse = files.tabBars.buttons["Browse"]
        if browse.waitForExistence(timeout: 10) { browse.tap() }
        let onDevice = files.cells.containing(.staticText, identifier: "On My iPhone").firstMatch
        let folder = files.cells.containing(.staticText, identifier: appName).firstMatch
        for _ in 0..<4 {
            if onDevice.exists { onDevice.tap(); break }
            if folder.exists { break }
            let back = files.buttons["DOC.navBarButton.backInHistory"]
            guard back.exists && back.isEnabled else { break }
            back.tap()
        }
        XCTAssertTrue(folder.waitForExistence(timeout: 10)); folder.tap()
        let reference = files.cells.containing(.staticText, identifier: "Settings.pbdb").firstMatch
        XCTAssertTrue(reference.waitForExistence(timeout: 10)); reference.tap()
        XCTAssertTrue(app.navigationBars["Settings"].waitForExistence(timeout: 20))
        XCTAssertTrue(app.cells.containing(.staticText, identifier: "greeting").firstMatch.waitForExistence(timeout: 10))
        app.navigationBars.buttons["Personal notes"].tap()
        XCTAssertTrue(app.cells.containing(.staticText, identifier: "Settings.pbdb").firstMatch.waitForExistence(timeout: 10))
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Database selected after a cold Files document open"; capture.lifetime = .keepAlways; add(capture)
    }

    func testDatabaseBrowserCreatesAndEditsObjectRecordOffline() throws {
        // Native field editing must publish only the selected field and survive a restart.
        let savedTitle = "Offline record \(UUID().uuidString.prefix(8))"
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-database-browser"]
        func openRecords() {
            XCTAssertTrue(app.buttons["Volumes"].waitForExistence(timeout: 20))
            app.buttons["Volumes"].tap()
            let personal = app.cells.containing(.staticText, identifier: "Personal notes").firstMatch
            XCTAssertTrue(personal.waitForExistence(timeout: 10)); personal.tap()
            let database = app.cells.containing(.staticText, identifier: "data.pbdb").firstMatch
            XCTAssertTrue(database.waitForExistence(timeout: 10)); database.tap()
            let type = app.cells.containing(.staticText, identifier: "Todo").firstMatch
            XCTAssertTrue(type.waitForExistence(timeout: 10)); type.tap()
        }
        app.launch()
        openRecords()
        app.buttons["Add"].tap()
        let field = app.cells.containing(.staticText, identifier: "title").firstMatch
        XCTAssertTrue(field.waitForExistence(timeout: 10)); field.tap()
        app.cells.containing(.staticText, identifier: "Add value").firstMatch.tap()
        let value = app.textViews["database-field-value"]
        XCTAssertTrue(value.waitForExistence(timeout: 10)); value.tap(); value.typeText(savedTitle)
        app.buttons["Done"].tap(); app.buttons["Save"].tap()
        XCTAssertTrue(app.staticTexts[savedTitle].waitForExistence(timeout: 10))
        let capture = XCTAttachment(screenshot: app.screenshot()); capture.name = "Native object record after offline edit"; capture.lifetime = .keepAlways; add(capture)
        app.terminate(); app.launch()
        openRecords()
        XCTAssertTrue(app.cells.containing(.staticText, identifier: savedTitle).firstMatch.waitForExistence(timeout: 10))
    }
    func testDatabaseBrowserEditsOfflineAndRetainsSavedValue() throws {
        // Exercise the real local store, native editor, and process restart without AP access.
        let savedValue = "Saved offline \(UUID().uuidString.prefix(8))"
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-database-browser"]
        func openValue() {
            let volumes = app.buttons["Volumes"]
            XCTAssertTrue(volumes.waitForExistence(timeout: 20))
            volumes.tap()
            let personal = app.cells.containing(.staticText, identifier: "Personal notes").firstMatch
            XCTAssertTrue(personal.waitForExistence(timeout: 10)); personal.tap()
            let database = app.cells.containing(.staticText, identifier: "Settings.pbdb").firstMatch
            XCTAssertTrue(database.waitForExistence(timeout: 10)); database.tap()
            let key = app.cells.containing(.staticText, identifier: "greeting").firstMatch
            XCTAssertTrue(key.waitForExistence(timeout: 10)); key.tap()
        }
        app.launch()
        openValue()
        let value = app.textViews["database-value"]
        XCTAssertTrue(value.waitForExistence(timeout: 10))
        value.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.025)).press(forDuration: 1.1)
        let selectAll = app.buttons["Select All"].exists ? app.buttons["Select All"] : app.menuItems["Select All"]
        XCTAssertTrue(selectAll.waitForExistence(timeout: 5)); selectAll.tap()
        value.typeText(savedValue)
        app.buttons["Save"].tap()
        XCTAssertTrue(app.cells.containing(.staticText, identifier: "greeting").firstMatch.waitForExistence(timeout: 10))
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = "Database browser after offline save"; attachment.lifetime = .keepAlways; add(attachment)
        app.terminate(); app.launch()
        openValue()
        XCTAssertTrue(app.textViews["database-value"].waitForExistence(timeout: 10))
        XCTAssertEqual(app.textViews["database-value"].value as? String, savedValue)
    }
    // Seed a synthetic conflicting-account profile in an isolated test bundle first.
    func testExistingAccountReplacementDecision() throws {
        guard let choice = ProcessInfo.processInfo.environment["PLAYBIT_REPLACEMENT_CHOICE"],
              ["Sign Out", "Replace"].contains(choice) else {
            throw XCTSkip("Requires an isolated replacement fixture and an explicit decision")
        }
        let app = XCUIApplication()
        app.launch()
        let alert = app.alerts["Replace Existing Data?"]
        guard alert.waitForExistence(timeout: 10) else {
            XCTFail("Seed an isolated conflicting-account profile before this test"); return
        }
        XCTAssertTrue(alert.staticTexts["This installation contains data for a different account. Continuing with signing in will replace that information."].exists)
        XCTAssertTrue(alert.buttons["Sign Out"].exists)
        XCTAssertTrue(alert.buttons["Replace"].exists)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Replace existing data"; capture.lifetime = .keepAlways; add(capture)
        alert.buttons[choice].tap()
        XCTAssertTrue(alert.waitForNonExistence(timeout: 10))
        XCTAssertTrue(app.navigationBars["Sign in"].waitForExistence(timeout: 10))
        if choice == "Sign Out" {
            XCTAssertTrue(app.textFields["sign-in-email"].exists)
        }
        XCTAssertFalse(app.alerts["Couldn’t finish sign-in"].exists)
        app.terminate(); app.launch()
        XCTAssertTrue(app.navigationBars.firstMatch.waitForExistence(timeout: 10))
        XCTAssertFalse(alert.exists)
    }

    // Opt-in reference-device cohort; XCTest launch metrics exclude test-driver waits.
    func testReplacementStartupTiming() throws {
        guard let mode = ProcessInfo.processInfo.environment["PLAYBIT_REPLACEMENT_TIMING"],
              ["pending", "signed-out"].contains(mode) else {
            throw XCTSkip("Requires an isolated reference-device timing fixture")
        }
        let app = XCUIApplication()
        app.launch()
        if mode == "pending" {
            XCTAssertTrue(app.alerts["Replace Existing Data?"].waitForExistence(timeout: 10))
        } else {
            XCTAssertTrue(app.navigationBars.firstMatch.waitForExistence(timeout: 10))
            XCTAssertFalse(app.alerts["Replace Existing Data?"].exists)
        }
        app.terminate()
        let options = XCTMeasureOptions()
        options.iterationCount = 10
        measure(metrics: [XCTApplicationLaunchMetric(waitUntilResponsive: true)], options: options) {
            app.launch()
        }
        app.terminate()
    }

    func testSavedAccountBackupSurvivesRestart() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_SAVED_ACCOUNT_QUALIFICATION"] == "1" else {
            throw XCTSkip("Requires an isolated provisioned distribution bundle")
        }
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-account-profile"]
        app.launchEnvironment["PLAYBIT_KEYCHAIN_QUALIFICATION"] = "seed"
        defer {
            app.terminate()
            app.launchEnvironment["PLAYBIT_KEYCHAIN_QUALIFICATION"] = "cleanup"
            app.launch()
            XCTAssertTrue(app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "Qualification recovery copy removed")).firstMatch.waitForExistence(timeout: 30))
            app.terminate()
        }
        app.launch()
        let saved = app.staticTexts["Account key recovery copy saved"]
        XCTAssertTrue(saved.waitForExistence(timeout: 30))
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Distribution account recovery copy saved"
        capture.lifetime = .keepAlways
        add(capture)
        app.terminate()
        app.launch()
        XCTAssertTrue(saved.waitForExistence(timeout: 30))
    }

    func testSavedAccountPrefillsWithoutSigningIn() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_SAVED_ACCOUNT_QUALIFICATION"] == "1" else {
            throw XCTSkip("Requires an isolated provisioned distribution bundle")
        }
        let app = XCUIApplication()
        app.launchEnvironment["PLAYBIT_KEYCHAIN_QUALIFICATION"] = "fill"
        defer {
            app.terminate()
            app.launchEnvironment["PLAYBIT_KEYCHAIN_QUALIFICATION"] = "cleanup"
            app.launch()
            XCTAssertTrue(app.staticTexts.containing(NSPredicate(format: "label CONTAINS %@", "Qualification recovery copy removed")).firstMatch.waitForExistence(timeout: 30))
            app.terminate()
        }
        app.launch()
        openSignIn(app)
        let email = app.textFields["sign-in-email"]
        let filled = NSPredicate(format: "value == %@", "offline@example.invalid")
        expectation(for: filled, evaluatedWith: email)
        waitForExpectations(timeout: 15)
        let key = app.secureTextFields["sign-in-account-key"]
        XCTAssertTrue(key.isEnabled)
        XCTAssertFalse((key.value as? String ?? "").isEmpty)
        XCTAssertNotEqual(key.value as? String, "Account key (optional)")
        XCTAssertTrue(app.buttons["sign-in-submit"].isEnabled)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Saved account filled; waiting for user sign-in"
        capture.lifetime = .keepAlways
        add(capture)
        app.terminate()
        app.launch()
        openSignIn(app)
        expectation(for: filled, evaluatedWith: email)
        waitForExpectations(timeout: 15)
    }

    // Run only with a fresh, separately provisioned distribution test bundle.
    func testSavedAccountMissingKeepsManualSignInAvailable() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_SAVED_ACCOUNT_QUALIFICATION"] == "1" else {
            throw XCTSkip("Requires an isolated provisioned distribution bundle")
        }
        let app = XCUIApplication()
        app.launch()
        openSignIn(app)
        let email = app.textFields["sign-in-email"]
        XCTAssertTrue(email.isEnabled)
        email.tap()
        email.typeText("manual@example.test")
        XCTAssertEqual(email.value as? String, "manual@example.test")
        XCTAssertTrue(app.buttons["sign-in-submit"].isEnabled)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Saved account missing; manual sign-in available"
        capture.lifetime = .keepAlways
        add(capture)
        app.terminate()
        app.launch()
        openSignIn(app)
        XCTAssertTrue(app.textFields["sign-in-email"].isEnabled)
        app.terminate()
    }

    private func openSignIn(_ app: XCUIApplication) {
        let signIn = app.navigationBars["Sessions"].buttons["Sign in"]
        if signIn.waitForExistence(timeout: 5) { signIn.tap() }
        XCTAssertTrue(app.navigationBars["Sign in"].waitForExistence(timeout: 30))
    }

    func testCorruptStoreCanResetAndOpenAgain() {
        let app = XCUIApplication()
        app.launchArguments.append("--phase1-corrupt-store")
        app.launch()
        XCTAssertTrue(app.staticTexts["Oh no…"].waitForExistence(timeout: 30))
        XCTAssertTrue(app.staticTexts["The local store has become corrupt. Some data may not have been backed up."].exists)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Corrupt local store recovery"; capture.lifetime = .keepAlways; add(capture)
        let reset = app.buttons["reset-corrupt-store"]
        XCTAssertTrue(reset.exists)
        reset.tap()
        XCTAssertTrue(app.navigationBars["Sign in"].waitForExistence(timeout: 30))
        XCTAssertTrue(app.textFields["sign-in-email"].exists)
        XCTAssertFalse(reset.exists)
        app.terminate()
        app.launchArguments.removeAll()
        app.launch()
        XCTAssertTrue(app.navigationBars["Sign in"].waitForExistence(timeout: 30))
        XCTAssertFalse(reset.exists)
    }

    private func sessionRows(_ app: XCUIApplication) -> XCUIElementQuery {
        app.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "session-row:"))
    }
    private func retainedID(_ row: XCUIElement) -> String {
        String(row.identifier.dropFirst("session-row:".count))
    }

    private func openInvitation(_ app: XCUIApplication) {
        app.buttons["new-session"].tap()
        let join = app.cells["new-session-join"]
        XCTAssertTrue(join.waitForExistence(timeout: 5)); join.tap()
    }

    // Operator-driven ordinary AP qualification. The source-watching Mac must
    // publish the requested heading after this participant has added its task.
    func testLiveInvitationAndSourceReload() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let invitation = environment["PLAYBIT_LIVE_INVITATION"],
              let session = environment["PLAYBIT_LIVE_SESSION"],
              let heading = environment["PLAYBIT_LIVE_UPDATED_HEADING"],
              let task = environment["PLAYBIT_LIVE_TASK"] else {
            throw XCTSkip("Requires a live invitation, session, updated heading and unique task")
        }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.navigationBars["Sessions"].waitForExistence(timeout: 30))
        openInvitation(app)
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText(invitation)
        app.buttons["Open"].firstMatch.tap()
        let join = app.buttons["invitation-action"]
        let ready = XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "exists == true AND enabled == true AND label == 'Join'"), object: join)
        guard XCTWaiter.wait(for: [ready], timeout: 60) == .completed else {
            XCTFail("Live invitation did not become ready to join"); return
        }
        join.tap()
        let input = app.textFields["guest-input-1"]
        guard input.waitForExistence(timeout: 60) else {
            XCTFail("Live join did not open the shared TODO"); return
        }
        input.tap(); input.typeText(task + "\n")
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: row)], timeout: 10), .completed)
        let joined = XCTAttachment(screenshot: app.screenshot())
        joined.name = "Live AP invitation joined and task committed"; joined.lifetime = .keepAlways; add(joined)
        // This is an operator coordination deadline, not a reload latency test.
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 180))
        XCTAssertEqual(row.value as? String, "Done")
        let updated = XCTAttachment(screenshot: app.screenshot())
        updated.name = "Source update retains shared task"; updated.lifetime = .keepAlways; add(updated)
        app.terminate(); app.launch()
        let retained = app.cells["session-row:" + session]
        XCTAssertTrue(retained.waitForExistence(timeout: 30)); retained.tap()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 30))
        XCTAssertEqual(row.value as? String, "Done")
    }

    func testLiveBackgroundParticipantResumesSourceUpdate() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let session = environment["PLAYBIT_LIVE_SESSION"],
              let heading = environment["PLAYBIT_LIVE_UPDATED_HEADING"],
              let retainedTask = environment["PLAYBIT_LIVE_RETAINED_TASK"],
              let task = environment["PLAYBIT_LIVE_TASK"] else {
            throw XCTSkip("Requires a live development session and operator-coordinated source update")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let backgrounded = directory.appendingPathComponent("playbit-live-backgrounded")
        let resume = directory.appendingPathComponent("playbit-live-resume")
        try? FileManager.default.removeItem(at: backgrounded)
        try? FileManager.default.removeItem(at: resume)
        defer { try? FileManager.default.removeItem(at: backgrounded); try? FileManager.default.removeItem(at: resume) }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.navigationBars["Sessions"].waitForExistence(timeout: 30))
        let sessionRow = app.cells["session-row:" + session]
        XCTAssertTrue(sessionRow.waitForExistence(timeout: 30)); sessionRow.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 60))
        let existing = app.buttons.matching(NSPredicate(format: "label == %@", retainedTask)).firstMatch
        XCTAssertEqual(existing.value as? String, "Done")
        XCUIApplication(bundleIdentifier: "com.apple.springboard").activate()
        // XCTest/debugger attachment may inhibit OS suspension. Record the
        // observed state; this test only claims background/foreground coverage.
        let background = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        guard XCTWaiter.wait(for: [background], timeout: 30) == .completed else {
            XCTFail("The app did not enter the background"); return
        }
        try Data(String(describing: app.state).utf8).write(to: backgrounded)
        let published = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: resume.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [published], timeout: 180) == .completed else {
            XCTFail("Operator did not confirm the headless Mac source update"); return
        }
        app.activate()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 60))
        XCTAssertEqual(existing.value as? String, "Done")
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        input.tap(); input.typeText(task + "\n")
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: row)], timeout: 10), .completed)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Background participant resumes source update and commits"; capture.lifetime = .keepAlways; add(capture)
        app.terminate(); app.launch()
        let retained = app.cells["session-row:" + session]
        XCTAssertTrue(retained.waitForExistence(timeout: 30)); retained.tap()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 30))
        XCTAssertEqual(row.value as? String, "Done")
    }

    // Install the runner first, then launch the app without a debugger.
    // UseDestinationArtifacts avoids reinstalling that process.
    func testLiveSuspendedPeerCatchUp() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let session = environment["PLAYBIT_LIVE_SESSION"],
              let heading = environment["PLAYBIT_LIVE_UPDATED_HEADING"],
              let retainedTask = environment["PLAYBIT_LIVE_RETAINED_TASK"],
              let task = environment["PLAYBIT_LIVE_TASK"] else {
            throw XCTSkip("Requires an externally launched live participant without a debugger")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let suspended = directory.appendingPathComponent("playbit-live-suspended")
        let resume = directory.appendingPathComponent("playbit-live-resume")
        try? FileManager.default.removeItem(at: suspended)
        try? FileManager.default.removeItem(at: resume)
        defer { try? FileManager.default.removeItem(at: suspended); try? FileManager.default.removeItem(at: resume) }
        let app = XCUIApplication()
        guard app.state == .runningForeground || app.state == .runningBackground
            || app.state == .runningBackgroundSuspended else {
            XCTFail("Launch the installed app independently before starting this test"); return
        }
        // activate preserves the externally launched process; launch would
        // replace it with one whose debugger can inhibit OS suspension.
        app.activate()
        let retained = app.cells["session-row:" + session]
        XCTAssertTrue(retained.waitForExistence(timeout: 30)); retained.tap()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 60))
        let earlier = app.buttons.matching(NSPredicate(format: "label == %@", retainedTask)).firstMatch
        XCTAssertTrue(earlier.waitForExistence(timeout: 10)); XCTAssertEqual(earlier.value as? String, "Done")
        XCUIApplication(bundleIdentifier: "com.apple.springboard").activate()
        let stopped = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackgroundSuspended
        }, object: nil)
        guard XCTWaiter.wait(for: [stopped], timeout: 30) == .completed else {
            XCTFail("Expected OS suspension; observed state \(app.state.rawValue)"); return
        }
        try Data("runningBackgroundSuspended".utf8).write(to: suspended)
        let published = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: resume.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [published], timeout: 180) == .completed else {
            XCTFail("Operator did not confirm the peer edit"); return
        }
        XCTAssertEqual(app.state, .runningBackgroundSuspended)
        app.activate()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 60))
        XCTAssertTrue(earlier.waitForExistence(timeout: 10)); XCTAssertEqual(earlier.value as? String, "Done")
        let incoming = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(incoming.waitForExistence(timeout: 30)); incoming.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: incoming)], timeout: 10), .completed)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "OS-suspended participant catches up and commits"
        capture.lifetime = .keepAlways; add(capture)
        app.terminate(); app.launch()
        XCTAssertTrue(retained.waitForExistence(timeout: 30)); retained.tap()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 30))
        XCTAssertEqual(incoming.value as? String, "Done")
    }

    // Explicit live qualification only. The operator supplies an isolated
    // account and the code actually delivered to its mailbox; no auth fixtures.
    private func signInLive() throws -> XCUIApplication {
        guard let address = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_EMAIL"] else {
            throw XCTSkip("Requires an isolated live account")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let codeFile = directory.appendingPathComponent("playbit-live-code.txt")
        let readyFile = directory.appendingPathComponent("playbit-live-code-requested")
        try? FileManager.default.removeItem(at: codeFile)
        try? FileManager.default.removeItem(at: readyFile)
        defer { try? FileManager.default.removeItem(at: codeFile); try? FileManager.default.removeItem(at: readyFile) }
        let app = XCUIApplication()
        app.launch()
        if ProcessInfo.processInfo.environment["PLAYBIT_TESTFLIGHT_ACCEPTANCE"] == "1" {
            let sessions = app.navigationBars["Sessions"]
            if sessions.buttons["Account"].exists && !app.textFields["sign-in-email"].exists {
                guard app.staticTexts["testing-channel"].exists else {
                    throw NSError(domain: "LiveQualification", code: 10, userInfo: [NSLocalizedDescriptionKey: "Refusing to sign out outside Testing"])
                }
                sessions.buttons["Account"].tap()
                let email = app.cells["account-email"]
                guard email.waitForExistence(timeout: 10), email.staticTexts[address].exists,
                      let identity = app.cells["account-id"].value as? String, Identifier.valid(identity) else {
                    throw NSError(domain: "LiveQualification", code: 11, userInfo: [NSLocalizedDescriptionKey: "Refusing to sign out an unknown or different account"])
                }
                let table = app.tables["account-settings"]
                let signOut = app.cells["account-sign-out"].staticTexts["Sign Out…"]
                guard signOut.waitForExistence(timeout: 10) else {
                    throw NSError(domain: "LiveQualification", code: 12, userInfo: [NSLocalizedDescriptionKey: "Ordinary Testing sign-out is unavailable"])
                }
                for _ in 0..<6 {
                    let frame = signOut.frame
                    if !frame.isEmpty && table.frame.contains(frame) && app.frame.contains(frame) { break }
                    table.swipeUp()
                }
                let frame = signOut.frame
                guard !frame.isEmpty, table.frame.contains(frame), app.frame.contains(frame) else {
                    throw NSError(domain: "LiveQualification", code: 12, userInfo: [NSLocalizedDescriptionKey: "Ordinary Testing sign-out is outside the visible table"])
                }
                signOut.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)).tap()
                let confirmation = app.alerts["Sign out?"]
                guard confirmation.waitForExistence(timeout: 5) else {
                    throw NSError(domain: "LiveQualification", code: 13, userInfo: [NSLocalizedDescriptionKey: "Ordinary sign-out confirmation is unavailable"])
                }
                confirmation.buttons["Sign Out"].tap()
                guard app.textFields["sign-in-email"].waitForExistence(timeout: 30) else {
                    throw NSError(domain: "LiveQualification", code: 14, userInfo: [NSLocalizedDescriptionKey: "Ordinary Testing sign-out did not complete"])
                }
            }
            func openSelector() throws {
                if !app.buttons["environment-menu"].exists {
                    guard app.buttons["Sign in"].firstMatch.waitForExistence(timeout: 20) else {
                        throw NSError(domain: "LiveQualification", code: 6, userInfo: [NSLocalizedDescriptionKey: "No ordinary environment selector before sign-in"])
                    }
                    app.buttons["Sign in"].firstMatch.tap()
                }
                guard app.buttons["environment-menu"].waitForExistence(timeout: 20) else {
                    throw NSError(domain: "LiveQualification", code: 7, userInfo: [NSLocalizedDescriptionKey: "Environment selector did not open"])
                }
            }
            try openSelector()
            if app.buttons["environment-menu"].label != "Environment: Testing" {
                app.buttons["environment-menu"].tap()
                guard app.buttons["Testing"].firstMatch.waitForExistence(timeout: 5) else {
                    throw NSError(domain: "LiveQualification", code: 8, userInfo: [NSLocalizedDescriptionKey: "Testing environment is unavailable"])
                }
                app.buttons["Testing"].firstMatch.tap()
                try openSelector()
            }
            let selected = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == true AND label == %@", "Environment: Testing"), object: app.buttons["environment-menu"])
            guard XCTWaiter.wait(for: [selected], timeout: 20) == .completed,
                  app.staticTexts["testing-channel"].exists else {
                throw NSError(domain: "LiveQualification", code: 9, userInfo: [NSLocalizedDescriptionKey: "Testing must be confirmed before any enrollment request"])
            }
        }
        let email = app.textFields["sign-in-email"]
        XCTAssertTrue(email.waitForExistence(timeout: 30))
        // The first edit cancels asynchronous saved-account prefill. Replace
        // both fields so this check always requests a delivered email code.
        func replace(_ field: XCUIElement, with text: String) throws {
            field.tap(); field.typeText(" ")
            guard let value = field.value as? String, value.count <= 257 else {
                throw NSError(domain: "LiveQualification", code: 16, userInfo: [NSLocalizedDescriptionKey: "Saved sign-in field exceeds its bound"])
            }
            field.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).press(forDuration: 1.1)
            let selectAll = app.buttons["Select All"].exists ? app.buttons["Select All"] : app.menuItems["Select All"]
            guard selectAll.waitForExistence(timeout: 5) else {
                throw NSError(domain: "LiveQualification", code: 18, userInfo: [NSLocalizedDescriptionKey: "Ordinary sign-in field selection is unavailable"])
            }
            selectAll.tap()
            field.typeText(text.isEmpty ? XCUIKeyboardKey.delete.rawValue : text)
        }
        let key = app.secureTextFields["sign-in-account-key"]
        guard key.exists else {
            throw NSError(domain: "LiveQualification", code: 15, userInfo: [NSLocalizedDescriptionKey: "Ordinary email sign-in fields are unavailable"])
        }
        try replace(email, with: address)
        try replace(key, with: "")
        guard email.value as? String == address else {
            XCTFail("The designated email did not replace the saved input")
            throw NSError(domain: "LiveQualification", code: 17, userInfo: [NSLocalizedDescriptionKey: "Ordinary email sign-in field was not replaced"])
        }
        app.buttons["sign-in-submit"].tap()
        let field = app.textFields["sign-in-code"]
        guard field.waitForExistence(timeout: 30) else { throw NSError(domain: "LiveQualification", code: 1, userInfo: [NSLocalizedDescriptionKey: "Code request did not complete"]) }
        try Data("ready".utf8).write(to: readyFile)
        let delivered = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: codeFile.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [delivered], timeout: 180) == .completed else {
            throw NSError(domain: "LiveQualification", code: 2, userInfo: [NSLocalizedDescriptionKey: "No delivered code supplied within the live-check deadline"])
        }
        let bytes = try Data(contentsOf: codeFile)
        try FileManager.default.removeItem(at: codeFile)
        guard bytes.count == 6, bytes.allSatisfy({ (48...57).contains($0) }),
              let code = String(data: bytes, encoding: .utf8) else { throw NSError(domain: "LiveQualification", code: 3, userInfo: [NSLocalizedDescriptionKey: "Expected one six-digit code"]) }
        field.tap()
        // Typing preserves zeroes; neither partial nor retired-length input can submit.
        for (candidate, enabled) in [("000123", true), ("12345", false), ("12345678", false)] {
            field.typeText(candidate)
            XCTAssertEqual(field.value as? String, candidate)
            XCTAssertEqual(app.buttons["sign-in-submit"].isEnabled, enabled)
            field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: candidate.count))
        }
        field.typeText(code)
        app.buttons["sign-in-submit"].tap()
        // Authentication and private-key recovery are separate. Do not copy or
        // acknowledge a account key merely to reach the app catalog.
        let recovery = app.navigationBars["Private data recovery"]
        let signedIn = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            recovery.exists || (app.navigationBars["Sessions"].exists && !field.exists)
        }, object: nil)
        guard XCTWaiter.wait(for: [signedIn], timeout: 30) == .completed else {
            throw NSError(domain: "LiveQualification", code: 4, userInfo: [NSLocalizedDescriptionKey: "Sign-in did not reach recovery or authenticated sessions"])
        }
        return app
    }

    func testSixDigitEmailSignIn() throws {
        let app = try signInLive()
        XCTAssertTrue(app.navigationBars["Private data recovery"].exists || app.navigationBars["Sessions"].exists)
    }

    func testLiveSignInListsApps() throws {
        guard let appIdentity = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_APP"] else {
            throw XCTSkip("Requires an isolated published app")
        }
        let app = try signInLive()
        let recovery = app.navigationBars["Private data recovery"]
        if recovery.exists { recovery.buttons.element(boundBy: 0).tap() }
        let add = app.buttons["new-session"]
        XCTAssertTrue(add.waitForExistence(timeout: 10)); add.tap()
        let row = app.cells["app-row:" + appIdentity]
        XCTAssertTrue(row.waitForExistence(timeout: 30))
        XCTAssertTrue(row.isEnabled)
        XCTAssertFalse(app.cells["new-session-sign-in"].exists)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Authenticated AP app list before private recovery"
        capture.lifetime = .keepAlways; self.add(capture)
        app.buttons["new-session-close"].tap()
        XCTAssertTrue(add.waitForExistence(timeout: 5))
        if ProcessInfo.processInfo.environment["PLAYBIT_TESTFLIGHT_ACCEPTANCE"] == "1" {
            let account = try testFlightAccount(app)
            let evidence = try JSONSerialization.data(withJSONObject: ["method": "ios-testflight-shared-native-v2",
                "case": "testLiveSignInListsApps",
                "channel": "testing", "account": account, "steps": ["launch", "sign_in"]], options: .sortedKeys)
            let result = XCTAttachment(data: evidence, uniformTypeIdentifier: "public.json")
            result.name = "TestFlight sign-in evidence.json"; result.lifetime = .keepAlways; self.add(result)
        }
    }

    private func testFlightAccount(_ app: XCUIApplication) throws -> String {
        XCTAssertTrue(app.staticTexts["testing-channel"].waitForExistence(timeout: 5))
        app.navigationBars["Sessions"].buttons["Account"].tap()
        let row = app.cells["account-id"]
        guard row.waitForExistence(timeout: 10), let account = row.value as? String, account != "Unavailable" else {
            throw NSError(domain: "LiveQualification", code: 5,
                          userInfo: [NSLocalizedDescriptionKey: "No ordinary Testing account identity"])
        }
        app.navigationBars["Account"].buttons.element(boundBy: 0).tap()
        return account
    }

    // Runs only against the already installed ordinary TestFlight app. The
    // separate sign-in case precedes any operator recovery of the same account.
    // No fixture, app install, key export or forced OS suspension is involved.
    func testLiveTestFlightAcceptance() throws {
        #if targetEnvironment(simulator)
        throw XCTSkip("Physical TestFlight installation is required")
        #else
        let environment = ProcessInfo.processInfo.environment
        guard environment["PLAYBIT_TESTFLIGHT_ACCEPTANCE"] == "1",
              let reference = environment["PLAYBIT_LIVE_SESSION_REFERENCE"],
              let task = environment["PLAYBIT_LIVE_TASK"],
              let peerTask = environment["PLAYBIT_LIVE_PEER_TASK"], task != peerTask,
              !task.isEmpty, !peerTask.isEmpty, task.utf8.count <= 256, peerTask.utf8.count <= 256,
              !task.unicodeScalars.contains(where: { $0.value < 32 }),
              !peerTask.unicodeScalars.contains(where: { $0.value < 32 }) else {
            throw XCTSkip("Requires the exact installed TestFlight build, isolated account/session and distinct peer task")
        }
        continueAfterFailure = false
        // The app resolves this secret code. Observe its internal ID below.
        XCTAssertTrue(reference.utf8.count == 22 && reference.allSatisfy {
            "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains($0)
        }, "Requires a complete Testing session reference")
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let intent = directory.appendingPathComponent("playbit-testflight-acceptance-intent")
        let ready = directory.appendingPathComponent("playbit-testflight-peer-ready")
        let edited = directory.appendingPathComponent("playbit-testflight-peer-edited")
        let check = directory.appendingPathComponent("playbit-testflight-peer-check")
        let confirmed = directory.appendingPathComponent("playbit-testflight-peer-confirmed")
        let peerEvidence = directory.appendingPathComponent("playbit-testflight-shared-peer-evidence.json")
        for file in [intent, ready, edited, check, confirmed, peerEvidence] {
            XCTAssertFalse(FileManager.default.fileExists(atPath: file.path), "Retain and inspect the original attempt before another run")
        }
        try Data((reference + "\n" + task + "\n" + peerTask).utf8).write(to: intent, options: .atomic)
        let app = XCUIApplication()
        defer { app.terminate() }
        app.launch()
        let account = try testFlightAccount(app)
        openInvitation(app)
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5)); field.tap(); field.typeText(reference)
        app.buttons["Open"].firstMatch.tap()
        let action = app.buttons["invitation-action"]
        let guestReady = app.textFields["guest-input-1"]
        let actionable = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            guestReady.exists || (action.exists && action.isEnabled && action.label == "Join")
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [actionable], timeout: 60), .completed)
        let entryAction: String
        if guestReady.exists {
            entryAction = "retained-open"
        } else {
            XCTAssertTrue(action.exists && action.isEnabled && action.label == "Join")
            entryAction = "Join"
            action.tap()
        }
        XCTAssertTrue(guestReady.waitForExistence(timeout: 60))
        app.terminate(); app.launch()
        XCTAssertTrue(app.navigationBars["Sessions"].waitForExistence(timeout: 30))
        app.navigationBars["Sessions"].buttons["Edit sessions"].tap()
        let rows = sessionRows(app)
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 10))
        let count = rows.count
        XCTAssertTrue((1...32).contains(count), "Use a bounded isolated session list")
        var matchedSession: String?
        for index in 0..<min(count, 32) {
            let row = rows.element(boundBy: index)
            let identity = retainedID(row)
            row.tap()
            let details = app.cells["session-detail-0-0"]
            let loaded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                guard details.exists, let value = details.value as? String else { return false }
                return value.utf8.count == 22 && value.allSatisfy {
                    "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".contains($0)
                }
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [loaded], timeout: 10), .completed,
                           "Session details did not load a complete reference")
            let matches = (details.value as? String) == reference
            app.navigationBars.buttons.element(boundBy: 0).tap()
            XCTAssertTrue(app.navigationBars["Editing Sessions"].waitForExistence(timeout: 5))
            if matches { matchedSession = identity; break }
        }
        guard let session = matchedSession else {
            XCTFail("The supplied reference did not match a retained native session"); return
        }
        app.navigationBars["Editing Sessions"].buttons["Done"].tap()
        let sessionRow = app.cells["session-row:" + session]
        func openSession() {
            XCTAssertTrue(sessionRow.waitForExistence(timeout: 30)); sessionRow.tap()
            XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 30))
        }
        openSession()
        let input = app.textFields["guest-input-1"]
        XCTAssertFalse(app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch.exists,
                       "Use a fresh unique local task")
        input.tap(); input.typeText(task + "\n")
        let local = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(local.waitForExistence(timeout: 10)); local.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: local)], timeout: 10), .completed)
        app.terminate(); app.launch(); openSession()
        XCTAssertTrue(local.waitForExistence(timeout: 10)); XCTAssertEqual(local.value as? String, "Done")
        let incoming = app.buttons.matching(NSPredicate(format: "label == %@", peerTask)).firstMatch
        XCTAssertFalse(incoming.exists, "Peer task must be created after this connection retires")
        app.terminate()
        let binding = Data((session + "\n" + peerTask).utf8)
        try binding.write(to: ready, options: .atomic)
        func waitForPeer(_ path: URL) throws {
            let observed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                FileManager.default.fileExists(atPath: path.path)
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [observed], timeout: 180), .completed)
            XCTAssertTrue(try Data(contentsOf: path) == binding, "Peer acknowledgment did not match this journey")
        }
        try waitForPeer(edited)
        app.launch(); openSession()
        XCTAssertTrue(incoming.waitForExistence(timeout: 30)); incoming.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: incoming)], timeout: 10), .completed)
        try binding.write(to: check, options: .atomic)
        try waitForPeer(confirmed)
        let attributes = try FileManager.default.attributesOfItem(atPath: peerEvidence.path)
        XCTAssertTrue(attributes[.type] as? FileAttributeType == .typeRegular)
        let evidenceSize = (attributes[.size] as? NSNumber)?.intValue ?? 0
        XCTAssertTrue((1...65536).contains(evidenceSize), "Retain bounded original Shared peer evidence")
        let peerBytes: Data
        do {
            let file = try FileHandle(forReadingFrom: peerEvidence)
            defer { try? file.close() }
            peerBytes = try file.read(upToCount: 65537) ?? Data()
        }
        XCTAssertTrue((1...65536).contains(peerBytes.count) && peerBytes.count == evidenceSize)
        let peerResult = XCTAttachment(data: peerBytes, uniformTypeIdentifier: "public.json")
        peerResult.name = "TestFlight Shared peer evidence.json"; peerResult.lifetime = .keepAlways; add(peerResult)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "TestFlight native edit and peer reconnect"; capture.lifetime = .keepAlways; add(capture)
        let evidence = try JSONSerialization.data(withJSONObject: ["method": "ios-testflight-shared-native-v2",
            "case": "testLiveTestFlightAcceptance",
            "channel": "testing", "account": account, "session": session,
            "session_reference": reference, "local_task": task, "peer_task": peerTask, "entry_action": entryAction,
            "steps": ["edit", "reopen", "reconnect"], "peer": "observed-inbound-and-outbound"], options: .sortedKeys)
        let result = XCTAttachment(data: evidence, uniformTypeIdentifier: "public.json")
        result.name = "TestFlight journey evidence.json"; result.lifetime = .keepAlways; add(result)
        #endif
    }

    // Ordinary two-client qualification. Operator edits the peer after the
    // local Leave marker, then acknowledges through the runner's Documents.
    func testLiveLeaveAndRejoinRetainsSharedData() throws {
        let env = ProcessInfo.processInfo.environment
        guard let session = env["PLAYBIT_LIVE_SESSION"], let invitation = env["PLAYBIT_LIVE_INVITATION"],
            let task = env["PLAYBIT_LIVE_TASK"], let retained = env["PLAYBIT_LIVE_RETAINED_TASK"] else {
            throw XCTSkip("Requires isolated live session, invitation and peer task coordination")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let left = directory.appendingPathComponent("playbit-live-left")
        let peer = directory.appendingPathComponent("playbit-live-peer-done")
        try? FileManager.default.removeItem(at: left)
        try? FileManager.default.removeItem(at: peer)
        let app = XCUIApplication()
        app.launch()
        let row = app.cells["session-row:" + session]
        XCTAssertTrue(row.waitForExistence(timeout: 30))
        app.navigationBars["Sessions"].buttons["Edit sessions"].tap()
        row.tap()
        let leave = app.cells["session-detail-0-4"]
        XCTAssertTrue(leave.waitForExistence(timeout: 5))
        let details = XCTAttachment(screenshot: app.screenshot())
        details.name = "Live retained session details"; details.lifetime = .keepAlways; add(details)
        app.cells["session-detail-0-3"].tap()
        let closeShare = app.buttons["Close"]
        let dismissPopover = app.otherElements["PopoverDismissRegion"]
        let presented = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            closeShare.exists || dismissPopover.exists
        }, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [presented], timeout: 5), .completed)
        let share = XCTAttachment(screenshot: app.screenshot())
        share.name = "Live native invitation sharing"; share.lifetime = .keepAlways; add(share)
        if dismissPopover.exists {
            dismissPopover.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.1)).tap()
        } else { closeShare.tap() }
        XCTAssertTrue(leave.waitForExistence(timeout: 5))
        leave.tap(); app.alerts.buttons["Leave Session"].tap()
        XCTAssertTrue(app.navigationBars["Editing Sessions"].waitForExistence(timeout: 5))
        XCTAssertFalse(row.exists)
        app.terminate(); app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        XCTAssertFalse(row.exists)
        try Data("left\n".utf8).write(to: left, options: .atomic)
        let peerReady = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: peer.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [peerReady], timeout: 180) == .completed else {
            XCTFail("Peer edit was not acknowledged; session stays locally forgotten"); return
        }
        XCTAssertFalse(row.exists, "Remote edits must not reverse local Leave")
        openInvitation(app)
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText(invitation)
        app.buttons["Open"].firstMatch.tap()
        let join = app.buttons["invitation-action"]
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "exists == true AND enabled == true AND label == 'Join'"), object: join)], timeout: 60), .completed)
        join.tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 60))
        let saved = app.buttons.matching(NSPredicate(format: "label == %@", retained)).firstMatch
        XCTAssertTrue(saved.waitForExistence(timeout: 10)); XCTAssertEqual(saved.value as? String, "Done")
        let incoming = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(incoming.waitForExistence(timeout: 10)); incoming.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: incoming)], timeout: 10), .completed)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Live rejoin retains old data and peer edit after Leave"; capture.lifetime = .keepAlways; add(capture)
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 30)); row.tap()
        XCTAssertTrue(incoming.waitForExistence(timeout: 30)); XCTAssertEqual(incoming.value as? String, "Done")
    }

    // Normal package, existing isolated account; no seed or private-key export.
    func testLiveSessionsNavigation() throws {
        guard let session = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_SESSION"] else {
            throw XCTSkip("Requires a retained session on an enrolled isolated account")
        }
        let app = XCUIApplication()
        // A preceding live test may leave a guest open. Launch returns to the
        // retained Sessions screen without resetting the enrolled account.
        app.launch()
        let row = app.cells["session-row:" + session]
        XCTAssertTrue(row.waitForExistence(timeout: 30))
        let ids = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        func assertRetainedSessions(file: StaticString = #filePath, line: UInt = #line) {
            let current = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
            // Other devices may add sessions while this live account catches
            // up. Navigation must preserve our original identities and order.
            XCTAssertEqual(current.filter { ids.contains($0) }, ids, file: file, line: line)
        }
        let account = app.navigationBars["Sessions"].buttons["Account"]
        XCTAssertTrue(account.isHittable); account.tap()
        XCTAssertTrue(app.cells["account-sign-out"].waitForExistence(timeout: 5))
        app.cells["account-recovery"].tap()
        let recovery = app.navigationBars["Private data recovery"]
        XCTAssertTrue(recovery.waitForExistence(timeout: 30))
        recovery.buttons.element(boundBy: 0).tap()
        app.navigationBars["Account"].buttons.element(boundBy: 0).tap()
        let addSession = app.buttons["new-session"]
        XCTAssertTrue(addSession.waitForExistence(timeout: 5)); addSession.tap()
        XCTAssertTrue(app.cells["new-session-join"].waitForExistence(timeout: 5))
        app.buttons["new-session-close"].tap()
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        assertRetainedSessions()
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Live retained sessions and account navigation"; capture.lifetime = .keepAlways; add(capture)
        row.tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 30))
        app.navigationBars.buttons["Sessions"].tap()
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        assertRetainedSessions()
    }

    // Run against the same retained account before and after another test device
    // releases AP subscription capacity. The expected state is supplied by the
    // operator; the app receives no injected runtime state or credentials.
    func testLiveRetainedSessionStatusAndInput() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let session = environment["PLAYBIT_LIVE_SESSION"],
              let expected = environment["PLAYBIT_LIVE_AVAILABILITY"],
              let task = environment["PLAYBIT_LIVE_TASK"] else {
            throw XCTSkip("Requires a retained live session, expected availability and unique task")
        }
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launch()
        let row = app.cells["session-row:" + session]
        XCTAssertTrue(row.waitForExistence(timeout: 30)); row.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 30))
        // A live capacity fixture may recover as other devices disconnect.
        let status = app.staticTexts.matching(NSPredicate(
            format: "label IN %@", [expected, "Connected"])).firstMatch
        XCTAssertTrue(status.waitForExistence(timeout: 90))
        let before = XCTAttachment(screenshot: app.screenshot())
        before.name = "Retained app synchronization status"; before.lifetime = .keepAlways; add(before)
        input.tap(); input.typeText(task + "\n")
        let added = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(added.waitForExistence(timeout: 10)); added.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "value == 'Done'"), object: added)], timeout: 10), .completed)
        XCTAssertTrue(status.exists)
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 30)); row.tap()
        XCTAssertTrue(added.waitForExistence(timeout: 30))
        XCTAssertEqual(added.value as? String, "Done")
        // Restart can release the previous connection's subscriptions and
        // recover capacity. Either the specific refusal or healthy replication
        // is valid; a generic failure or indefinitely pending state is not.
        XCTAssertTrue(status.waitForExistence(timeout: 90))
        let retained = XCTAttachment(screenshot: app.screenshot())
        retained.name = "Local edit retained after restart"; retained.lifetime = .keepAlways; add(retained)
    }

    func testLivePreparedRecoveryControls() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_LIVE_RECOVERY_READY"] == "1" else {
            throw XCTSkip("Requires an isolated signed-in account with an unsaved account key")
        }
        let app = XCUIApplication()
        app.launch()
        let recovery = app.navigationBars["Sessions"].buttons["Account"]
        XCTAssertTrue(recovery.waitForExistence(timeout: 30)); recovery.tap()
        app.cells["account-recovery"].tap()
        let copy = app.buttons["recovery-copy"]
        let save = app.buttons["recovery-save"]
        let acknowledgment = app.switches["recovery-saved"]
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: copy)], timeout: 30), .completed)
        XCTAssertFalse(save.isEnabled)
        func captureControls(_ name: String) {
            // Save only the controls below the secret, never the credential.
            let region = copy.frame.union(acknowledgment.frame).union(save.frame)
            let screen = app.screenshot().image
            let image = UIGraphicsImageRenderer(size: region.size).image { _ in
                screen.draw(at: CGPoint(x: -region.minX, y: -region.minY))
            }
            let capture = XCTAttachment(image: image)
            capture.name = name; capture.lifetime = .keepAlways; add(capture)
        }
        captureControls("Prepared recovery controls, acknowledgment off")
        acknowledgment.tap()
        XCTAssertTrue(save.isEnabled)
        captureControls("Prepared recovery controls, acknowledgment on")
        acknowledgment.tap()
        XCTAssertFalse(save.isEnabled)
        app.navigationBars["Private data recovery"].buttons.element(boundBy: 0).tap()
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
        recovery.tap()
        app.cells["account-recovery"].tap()
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertFalse(save.isEnabled)
        XCTAssertEqual(acknowledgment.value as? String, "0")
        app.navigationBars["Private data recovery"].buttons.element(boundBy: 0).tap()
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
    }

    // Explicitly authorized operator check only. Export confirmation is supplied
    // after the operator has saved the key outside the app's data container.
    func testLiveSaveRecoveryBackup() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_LIVE_SAVE_RECOVERY"] == "1" else {
            throw XCTSkip("Requires authorization to export and save the isolated account's recovery key")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let keyFile = directory.appendingPathComponent("playbit-live-recovery-key.txt")
        let confirmed = directory.appendingPathComponent("playbit-live-backup-exported")
        try? FileManager.default.removeItem(at: confirmed)
        defer { try? FileManager.default.removeItem(at: keyFile); try? FileManager.default.removeItem(at: confirmed) }
        let app = XCUIApplication()
        app.launch()
        let recovery = app.navigationBars["Sessions"].buttons["Account"]
        XCTAssertTrue(recovery.waitForExistence(timeout: 30)); recovery.tap()
        app.cells["account-recovery"].tap()
        let copy = app.buttons["recovery-copy"]
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: copy)], timeout: 30), .completed)
        let secret = try XCTUnwrap(app.textViews["recovery-secret"].value as? String)
        guard !secret.isEmpty && secret.utf8.count <= 1024 else { XCTFail("Invalid prepared recovery key"); return }
        XCTAssertTrue(FileManager.default.createFile(atPath: keyFile.path, contents: Data(secret.utf8),
            attributes: [.posixPermissions: 0o600, .protectionKey: FileProtectionType.complete]))
        let exported = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: confirmed.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [exported], timeout: 180) == .completed else {
            XCTFail("Recovery key export was not confirmed; backup remains unacknowledged"); return
        }
        let save = app.buttons["recovery-save"]
        XCTAssertFalse(save.isEnabled)
        app.switches["recovery-saved"].tap()
        XCTAssertTrue(save.isEnabled); save.tap()
        let name = app.textFields["account-display-name"]
        if name.waitForExistence(timeout: 30) {
            name.tap(); name.typeText("Recovery test")
            app.buttons["account-display-name-save"].tap()
        }
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
        XCTAssertTrue(recovery.waitForExistence(timeout: 30)); recovery.tap()
        app.cells["account-recovery"].tap()
        XCTAssertTrue(app.staticTexts["Recovery is set up. Keep your account key somewhere safe."].waitForExistence(timeout: 30))
        XCTAssertFalse(app.buttons["Retry backup"].exists)
        let complete = XCTAttachment(screenshot: app.screenshot())
        complete.name = "Recovery backup saved through ordinary AP"; complete.lifetime = .keepAlways; add(complete)
    }

    // Ordinary package, a fresh installation and an operator-supplied isolated
    // account. The key is read from the runner container, never a launch argument.
    func testLiveAccountKeySignIn() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["PLAYBIT_LIVE_KEY_SIGN_IN"] == "1",
              let address = environment["PLAYBIT_LIVE_EMAIL"],
              let session = environment["PLAYBIT_LIVE_SESSION"],
              let task = environment["PLAYBIT_LIVE_RETAINED_TASK"] else {
            throw XCTSkip("Requires an isolated live account key and retained TODO")
        }
        let keyFile = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("playbit-live-recovery-key.txt")
        let bytes = try Data(contentsOf: keyFile)
        try FileManager.default.removeItem(at: keyFile)
        guard bytes.count == 23, let key = String(data: bytes, encoding: .utf8), key.first == "a" else {
            XCTFail("Expected a complete operator-supplied account key"); return
        }
        let app = XCUIApplication()
        app.launch()
        let email = app.textFields["sign-in-email"]
        XCTAssertTrue(email.waitForExistence(timeout: 30))
        email.tap(); email.typeText(address)
        let field = app.secureTextFields["sign-in-account-key"]
        XCTAssertTrue(field.exists)
        let wrong = key == "a" + String(repeating: "1", count: 22)
            ? "a" + String(repeating: "1", count: 21) + "2" : "a" + String(repeating: "1", count: 22)
        field.tap(); field.typeText(wrong)
        app.buttons["sign-in-submit"].tap()
        let failed = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "label CONTAINS %@", "Email or account key is incorrect"), object: app.staticTexts["sign-in-status"])
        XCTAssertEqual(XCTWaiter.wait(for: [failed], timeout: 30), .completed)
        XCTAssertFalse(app.textFields["sign-in-code"].exists)
        XCTAssertEqual(sessionRows(app).count, 0)
        field.tap(); field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: wrong.count))
        field.typeText(key)
        app.buttons["sign-in-submit"].tap()
        let retained = app.cells["session-row:" + session]
        XCTAssertTrue(retained.waitForExistence(timeout: 60), "Key sign-in must unlock the existing catalog without a code")
        XCTAssertFalse(app.textFields["sign-in-code"].exists)
        XCTAssertFalse(app.buttons["recovery-save"].exists)
        retained.tap()
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 60))
        XCTAssertEqual(row.value as? String, "Done")
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Account key unlocks retained live TODO without email"
        capture.lifetime = .keepAlways; add(capture)
        app.terminate(); app.launch()
        XCTAssertTrue(retained.waitForExistence(timeout: 30))
        retained.tap()
        XCTAssertTrue(row.waitForExistence(timeout: 30))
        XCTAssertEqual(row.value as? String, "Done")
    }

    func testLiveRestoreRecoveryBackup() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["PLAYBIT_LIVE_RESTORE_RECOVERY"] == "1",
              let session = environment["PLAYBIT_LIVE_SESSION"],
              let heading = environment["PLAYBIT_LIVE_UPDATED_HEADING"],
              let task = environment["PLAYBIT_LIVE_TASK"] else {
            throw XCTSkip("Requires an authorized saved backup and retained live session")
        }
        let app = try signInLive()
        let keyFile = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("playbit-live-recovery-key.txt")
        let bytes = try Data(contentsOf: keyFile)
        try FileManager.default.removeItem(at: keyFile)
        guard !bytes.isEmpty && bytes.count <= 1024, let secret = String(data: bytes, encoding: .utf8) else {
            XCTFail("Invalid operator-supplied recovery key"); return
        }
        XCTAssertFalse(app.buttons["recovery-copy"].exists, "Existing backup must not be replaced by new setup")
        let field = app.textViews["recovery-secret"]
        XCTAssertTrue(field.waitForExistence(timeout: 30))
        let zeros = "a" + String(repeating: "1", count: 22)
        let wrongSecret = secret == zeros
            ? "a" + String(repeating: "1", count: 21) + "2" : zeros
        field.tap(); field.typeText(wrongSecret)
        app.buttons["Recover private data"].tap()
        let failure = app.staticTexts["recovery-status"]
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "label CONTAINS %@", "account key or credential backup does not match"),
            object: failure)], timeout: 30), .completed)
        XCTAssertEqual(field.value as? String, wrongSecret)
        XCTAssertFalse(app.buttons["recovery-copy"].exists)
        let rejected = XCTAttachment(screenshot: app.screenshot())
        rejected.name = "Wrong recovery key rejected without replacing backup"
        rejected.lifetime = .keepAlways; add(rejected)
        app.navigationBars["Private data recovery"].buttons.element(boundBy: 0).tap()
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
        XCTAssertEqual(sessionRows(app).count, 0, "Wrong key must not expose private sessions")
        app.terminate(); app.launch()
        let recovery = app.navigationBars["Sessions"].buttons["Account"]
        XCTAssertTrue(recovery.waitForExistence(timeout: 30)); recovery.tap()
        app.cells["account-recovery"].tap()
        XCTAssertTrue(field.waitForExistence(timeout: 30))
        XCTAssertFalse(app.buttons["recovery-copy"].exists, "Failed recovery must preserve the existing backup across restart")
        field.tap(); field.typeText(secret)
        app.buttons["Recover private data"].tap()
        if app.navigationBars["Account"].waitForExistence(timeout: 5) {
            app.navigationBars["Account"].buttons.element(boundBy: 0).tap()
        }
        let retained = app.cells["session-row:" + session]
        XCTAssertTrue(retained.waitForExistence(timeout: 60))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "exists == false"), object: app.textFields["account-display-name"])],
            timeout: 5), .completed, "Recovered account continues without another name save")
        retained.tap()
        XCTAssertTrue(app.staticTexts[heading].waitForExistence(timeout: 60))
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10)); XCTAssertEqual(row.value as? String, "Done")
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Fresh install recovered shared session and task"; capture.lifetime = .keepAlways; add(capture)
    }

    // Normal package and real AP preparation; never saves or copies the secret.
    func testLiveRecoveryPreparationRemainsUnsavedAcrossBackAndRestart() throws {
        let app = try signInLive()
        let copy = app.buttons["recovery-copy"]
        let save = app.buttons["recovery-save"]
        let acknowledgment = app.switches["recovery-saved"]
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: copy)], timeout: 30), .completed)
        XCTAssertFalse(save.isEnabled)
        XCTAssertEqual(acknowledgment.value as? String, "0")
        XCTAssertFalse(app.buttons["recovery-retry"].exists)
        let field = app.textViews["recovery-secret"]
        let prepared = try XCTUnwrap(field.value as? String)
        XCTAssertFalse(prepared.isEmpty)
        // Do not attach the credential or include it in assertion output.
        app.navigationBars["Private data recovery"].buttons.element(boundBy: 0).tap()
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
        app.navigationBars["Sessions"].buttons["Account"].tap()
        app.cells["account-recovery"].tap()
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: copy)], timeout: 30), .completed)
        XCTAssertTrue((field.value as? String) == prepared)
        XCTAssertFalse(save.isEnabled)
        app.terminate(); app.launch()
        let recovery = app.navigationBars["Sessions"].buttons["Account"]
        XCTAssertTrue(recovery.waitForExistence(timeout: 30)); recovery.tap()
        app.cells["account-recovery"].tap()
        XCTAssertTrue(copy.waitForExistence(timeout: 30))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: copy)], timeout: 30), .completed)
        XCTAssertTrue((field.value as? String) == prepared)
        XCTAssertFalse(save.isEnabled)
        XCTAssertEqual(acknowledgment.value as? String, "0")
        app.navigationBars["Private data recovery"].buttons.element(boundBy: 0).tap()
        if app.navigationBars["Account"].exists { app.navigationBars["Account"].buttons.element(boundBy: 0).tap() }
    }

    func testNewSessionOffersSignInAndJoinWithoutChangingLocalSessions() throws {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 30))
        let ids = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        app.buttons["new-session"].tap()
        let join = app.cells["new-session-join"]
        XCTAssertTrue(join.waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["new-session-instruction"].exists)
        XCTAssertTrue(app.buttons["new-session-close"].isHittable)
        let example = app.cells["example-open:shared-todo"]
        XCTAssertTrue(example.exists)
        XCTAssertTrue(app.buttons["example-source:shared-todo"].exists)
        XCTAssertTrue(example.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "other beta testers share this list")).firstMatch.exists)
        XCTAssertGreaterThan(example.frame.maxY, app.frame.midY, "Short native list should be bottom aligned")
        XCTAssertTrue(app.cells["new-session-sign-in"].exists)
        XCTAssertEqual(app.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "app-row:")).count, 0)
        let sheet = XCTAttachment(screenshot: app.screenshot())
        sheet.name = "New Session signed out"; sheet.lifetime = .keepAlways; add(sheet)
        try app.performAccessibilityAudit()
        join.tap()
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText("discard this entry draft")
        app.navigationBars["Join Session"].buttons["New Session"].tap()
        XCTAssertTrue(join.waitForExistence(timeout: 5))
        join.tap()
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        XCTAssertEqual(field.value as? String, field.placeholderValue)
        app.buttons["Cancel"].firstMatch.tap()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
        app.buttons["new-session"].tap()
        app.cells["new-session-sign-in"].tap()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 5))
        XCUIDevice.shared.press(.home)
        XCTAssertTrue(app.wait(for: .runningBackground, timeout: 10))
        app.terminate()
    }

    private func catalogPublisher() throws -> String {
        guard let publisher = ProcessInfo.processInfo.environment["PLAYBIT_CATALOG_PUBLISHER"],
              Identifier.valid(publisher) else {
            throw XCTSkip("Set PLAYBIT_CATALOG_PUBLISHER from the signed catalog fixture identity")
        }
        return publisher
    }

    func testLocalAPCatalogNamesArtworkRefreshAndDismissal() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_LOCAL_CATALOG"] == "1" else {
            throw XCTSkip("Requires the isolated local AP catalog fixture and installed profile")
        }
        let app = XCUIApplication()
        app.launch()
        let newSession = app.buttons["new-session"]
        XCTAssertTrue(newSession.waitForExistence(timeout: 30))
        let ids = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        newSession.tap()
        let rows = app.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "app-row:"))
        let ready = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            rows.count == 4 && rows.descendants(matching: .staticText).matching(identifier: "TODO").count == 2
                && app.staticTexts["No artwork"].exists && app.staticTexts["Offline notes"].exists
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 30), .completed)
        XCTAssertTrue(app.staticTexts["catalog-001"].exists)
        XCTAssertTrue(app.staticTexts["catalog-002"].exists)
        let disabled = rows.containing(.staticText, identifier: "Offline notes").firstMatch
        XCTAssertFalse(disabled.isEnabled)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Real AP catalog names and artwork"; capture.lifetime = .keepAlways; add(capture)
        try app.performAccessibilityAudit()
        let before = rows.allElementsBoundByIndex.map(\.identifier)
        app.buttons["new-session-refresh"].tap()
        let refreshed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            rows.count == 4 && rows.descendants(matching: .staticText).matching(identifier: "TODO").count == 2
                && app.buttons["new-session-refresh"].isEnabled
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [refreshed], timeout: 30), .completed)
        XCTAssertEqual(rows.allElementsBoundByIndex.map(\.identifier), before)
        app.buttons["new-session-close"].tap()
        XCTAssertTrue(newSession.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
        XCUIDevice.shared.press(.home)
        let backgrounded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [backgrounded], timeout: 10), .completed)
        app.terminate()
    }

    func testLocalAPCatalogEightPagesAndSessionLaunch() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["PLAYBIT_LOCAL_CATALOG"] == "8" else {
            throw XCTSkip("Requires the eight-page local AP fixture and its original session")
        }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        let retained = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        app.buttons["new-session"].tap()
        let table = app.tables["new-session-list"]
        let rows = table.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "app-row:"))
        let publisher = try catalogPublisher()
        func rowID(_ index: Int) -> String {
            "app-row:" + publisher + ":catalog-" + String(format: "%03d", index) + (index == 256 ? String(repeating: "x", count: 53) : "")
        }
        func awaitPage(_ first: Int) {
            let ready = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                rows.count == 32 && rows.element(boundBy: 0).identifier == rowID(first)
                    && app.buttons["new-session-refresh"].isEnabled
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 30), .completed)
            XCTAssertEqual(rows.allElementsBoundByIndex.map(\.identifier), (first..<(first + 32)).map(rowID))
        }
        func scrollTo(_ element: XCUIElement) {
            for _ in 0..<24 {
                if element.isHittable { return }
                table.swipeUp()
            }
            XCTFail("Catalog action is not reachable: \(element.identifier)")
        }
        awaitPage(1)
        XCTAssertFalse(table.cells[rowID(3)].isEnabled)
        XCTAssertFalse(table.cells[rowID(5)].isEnabled)
        let first = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        first.name = "32-row catalog including unavailable entries"; first.lifetime = .keepAlways; add(first)
        for page in 0..<8 {
            if page > 0 { awaitPage(page * 32 + 1) }
            XCTAssertTrue(rows.element(boundBy: 0).isHittable, "A newly selected page starts at its first entry")
            if page == 1 {
                let next = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
                next.name = "Next page begins with its first app"; next.lifetime = .keepAlways; add(next)
            }
            let expected = rows.allElementsBoundByIndex.map(\.identifier)
            app.buttons["new-session-refresh"].tap()
            awaitPage(page * 32 + 1)
            XCTAssertEqual(rows.allElementsBoundByIndex.map(\.identifier), expected)
            let action = table.cells[page == 7 ? "app-page-First page" : "app-page-Next page"]
            scrollTo(action)
            if page == 7 {
                XCTAssertFalse(table.cells["app-page-Next page"].exists)
                let last = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
                last.name = "Last catalog page with maximum-length label"; last.lifetime = .keepAlways; add(last)
                try app.performAccessibilityAudit()
            }
            action.tap()
        }
        awaitPage(1)
        XCTAssertTrue(rows.element(boundBy: 0).isHittable)
        app.buttons["new-session-close"].tap()
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, retained)
        app.buttons["new-session"].tap()
        awaitPage(1)
        // Select the complete app identity, never a label or a root chosen by the UI.
        table.cells[rowID(1)].tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 30))
        let task = "Catalog bound-root launch " + UUID().uuidString
        input.tap(); input.typeText(task + "\n")
        let taskRow = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(taskRow.waitForExistence(timeout: 10))
        app.navigationBars.buttons["Sessions"].tap()
        XCTAssertEqual(sessionRows(app).count, retained.count + 1)
        let created = Set(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }).subtracting(retained)
        XCTAssertEqual(created.count, 1)
        app.cells["session-row:" + (try XCTUnwrap(created.first))].tap()
        XCTAssertTrue(input.waitForExistence(timeout: 30))
        XCTAssertTrue(taskRow.waitForExistence(timeout: 10), "The new session must retain its Session data")
        XCUIDevice.shared.press(.home)
        let backgrounded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [backgrounded], timeout: 10), .completed)
        app.terminate()
    }

    func testLocalAPCatalogLossRetainsPreviousPage() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_LOCAL_CATALOG"] == "stop" else {
            throw XCTSkip("Requires the live local catalog fixture and operator-controlled AP shutdown")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let ready = directory.appendingPathComponent("catalog-stop-ready")
        let stopped = directory.appendingPathComponent("catalog-stopped")
        try? FileManager.default.removeItem(at: ready)
        try? FileManager.default.removeItem(at: stopped)
        defer { try? FileManager.default.removeItem(at: ready); try? FileManager.default.removeItem(at: stopped) }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        let retained = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        app.buttons["new-session"].tap()
        let table = app.tables["new-session-list"]
        let rows = table.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "app-row:"))
        let loaded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            rows.count == 32 && table.staticTexts["No artwork"].exists && app.buttons["new-session-refresh"].isEnabled
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [loaded], timeout: 30), .completed)
        let before = rows.allElementsBoundByIndex.map(\.identifier)
        try Data("ready".utf8).write(to: ready)
        // Explicit orchestration only; the shipped app has no test-control path.
        let shutdown = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            FileManager.default.fileExists(atPath: stopped.path)
        }, object: nil)
        guard XCTWaiter.wait(for: [shutdown], timeout: 120) == .completed else {
            XCTFail("Operator did not stop the isolated AP"); return
        }
        app.buttons["new-session-refresh"].tap()
        let feedback = app.staticTexts["new-session-feedback"]
        let failed = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "exists == true AND label CONTAINS %@", "Couldn’t load apps."), object: feedback)
        XCTAssertEqual(XCTWaiter.wait(for: [failed], timeout: 40), .completed)
        XCTAssertEqual(rows.allElementsBoundByIndex.map(\.identifier), before)
        XCTAssertTrue(app.buttons["new-session-refresh"].isEnabled)
        for _ in 0..<24 {
            if feedback.isHittable { break }
            table.swipeUp()
        }
        XCTAssertTrue(feedback.isHittable)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Failed refresh retains its previous page"; capture.lifetime = .keepAlways; add(capture)
        app.buttons["new-session-close"].tap()
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, retained)
        app.terminate()
    }

    func testLocalAPCatalogStaleSelectionCloseDuringCreateAndEmpty() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_LOCAL_CATALOG"] == "races" else {
            throw XCTSkip("Requires the four-entry local AP and its coordinated lifecycle/storage controls")
        }
        let directory = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        func checkpoint(_ name: String) throws {
            let ready = directory.appendingPathComponent("catalog-" + name + "-ready")
            let done = directory.appendingPathComponent("catalog-" + name + "-done")
            try? FileManager.default.removeItem(at: done)
            defer { try? FileManager.default.removeItem(at: ready); try? FileManager.default.removeItem(at: done) }
            try Data(name.utf8).write(to: ready)
            let completed = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                FileManager.default.fileExists(atPath: done.path)
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [completed], timeout: 60), .completed)
        }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        let retained = Set(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) })
        app.buttons["new-session"].tap()
        let table = app.tables["new-session-list"]
        let selected = table.cells["app-row:" + (try catalogPublisher()) + ":catalog-001"]
        let loaded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            table.staticTexts["No artwork"].exists && selected.isEnabled
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [loaded], timeout: 30), .completed)
        try checkpoint("disable")
        // The visible row is stale; AP authority must be checked on selection.
        selected.tap()
        let feedback = app.staticTexts["new-session-feedback"]
        let failed = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "exists == true AND label CONTAINS %@", "Couldn’t create the session."), object: feedback)
        XCTAssertEqual(XCTWaiter.wait(for: [failed], timeout: 20), .completed)
        XCTAssertFalse(app.textFields["guest-input-1"].exists)
        let failure = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        failure.name = "Stale app selection fails clearly"; failure.lifetime = .keepAlways; add(failure)
        app.buttons["new-session-refresh"].tap()
        let disabled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            selected.exists && !selected.isEnabled && app.buttons["new-session-refresh"].isEnabled
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [disabled], timeout: 20), .completed)
        app.buttons["new-session-close"].tap()
        XCTAssertEqual(Set(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }), retained)
        try checkpoint("enable")
        app.buttons["new-session"].tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            selected.isEnabled && table.staticTexts["No artwork"].exists
        }, object: nil)], timeout: 30), .completed)
        try checkpoint("hold")
        selected.tap()
        XCTAssertTrue(feedback.label.hasPrefix("Creating a session"))
        app.buttons["new-session-close"].tap()
        XCTAssertTrue(app.navigationBars["Sessions"].exists)
        try checkpoint("release")
        let created = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            self.sessionRows(app).count == retained.count + 1
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [created], timeout: 30), .completed)
        XCTAssertTrue(app.navigationBars["Sessions"].exists, "A late result must not navigate after Close")
        XCTAssertFalse(app.textFields["guest-input-1"].exists)
        let remaining = Set(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) })
        let newSession = remaining.subtracting(retained)
        XCTAssertEqual(newSession.count, 1)
        let id = try XCTUnwrap(newSession.first)
        app.cells["session-row:" + id].tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 30))
        app.navigationBars.buttons["Sessions"].tap()
        try checkpoint("empty")
        app.buttons["new-session"].tap()
        let empty = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "exists == true AND label == %@", "You haven’t published any apps yet."), object: feedback)
        XCTAssertEqual(XCTWaiter.wait(for: [empty], timeout: 20), .completed)
        XCTAssertEqual(table.cells.matching(NSPredicate(format: "identifier BEGINSWITH %@", "app-row:")).count, 0)
        XCTAssertTrue(table.cells["new-session-join"].isEnabled)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Empty catalog preserves Join and retained sessions"; capture.lifetime = .keepAlways; add(capture)
        try app.performAccessibilityAudit()
        app.buttons["new-session-close"].tap()
        XCTAssertEqual(Set(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }), remaining)
        XCUIDevice.shared.press(.home)
        let backgrounded = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [backgrounded], timeout: 10), .completed)
        app.terminate()
    }

    func testLocalAPCatalogUnavailableKeepsSavedSessionUsable() throws {
        let environment = ProcessInfo.processInfo.environment
        guard environment["PLAYBIT_LOCAL_CATALOG"] == "offline",
              let rootSession = environment["PLAYBIT_CATALOG_ROOT_SESSION"] else {
            throw XCTSkip("Requires a stopped local AP and the retained initialized catalog fixture")
        }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        let retained = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        app.buttons["new-session"].tap()
        let feedback = app.staticTexts["new-session-feedback"]
        let failed = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "exists == true AND label CONTAINS %@", "Couldn’t load apps."), object: feedback)
        XCTAssertEqual(XCTWaiter.wait(for: [failed], timeout: 40), .completed)
        XCTAssertTrue(app.buttons["new-session-refresh"].isEnabled)
        XCTAssertTrue(app.cells["new-session-join"].isEnabled)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Unavailable AP with explicit retry and local sessions retained"; capture.lifetime = .keepAlways; add(capture)
        try app.performAccessibilityAudit()
        app.buttons["new-session-refresh"].tap()
        app.buttons["new-session-close"].tap()
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, retained)
        app.cells["session-row:" + rootSession].tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 30))
        let task = "Offline catalog retry " + UUID().uuidString
        input.tap(); input.typeText(task + "\n")
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
        XCTAssertEqual(row.value as? String, "Done")
        app.terminate(); app.launch()
        app.cells["session-row:" + rootSession].tap()
        XCTAssertTrue(row.waitForExistence(timeout: 30))
        XCTAssertEqual(row.value as? String, "Done")
        XCTAssertFalse(app.navigationBars["New Session"].exists)
        app.terminate()
    }

    func testNewSessionLargeTextScrollsAndClosePreservesSessions() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "-UIPreferredContentSizeCategoryName",
            "UICTContentSizeCategoryAccessibilityXXXL"]
        app.launch()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 30))
        let ids = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        app.buttons["new-session"].tap()
        let join = app.cells["new-session-join"]
        XCTAssertTrue(join.waitForExistence(timeout: 5))
        let close = app.buttons["new-session-close"]
        XCTAssertTrue(close.isHittable)
        // Long native rows remain scrollable on the small screen; the close
        // control and instruction belong to the fixed navigation area.
        let signIn = app.cells["new-session-sign-in"]
        if !signIn.isHittable { app.tables["new-session-list"].swipeUp() }
        XCTAssertTrue(signIn.isHittable)
        let large = XCTAttachment(screenshot: app.screenshot())
        large.name = "New Session largest Dynamic Type"; large.lifetime = .keepAlways; add(large)
        let feedback = app.staticTexts["new-session-feedback"]
        let table = app.tables["new-session-list"]
        for _ in 0..<4 where feedback.frame.maxY > table.frame.maxY + 1 { table.swipeUp() }
        XCTAssertLessThanOrEqual(feedback.frame.maxY, table.frame.maxY + 1, "Full explanation must be reachable by scrolling")
        let scrolled = XCTAttachment(screenshot: app.screenshot())
        scrolled.name = "New Session large text explanation"; scrolled.lifetime = .keepAlways; add(scrolled)
        close.tap()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        app.buttons["new-session"].tap()
        XCTAssertTrue(join.waitForExistence(timeout: 5))
        XCTAssertTrue(close.isHittable)
        XCTAssertTrue(join.isHittable)
        XCTAssertGreaterThan(app.frame.width, app.frame.height)
        let geometry = XCTAttachment(string: "app=\(app.frame) table=\(table.frame) join=\(join.frame) close=\(close.frame)")
        geometry.name = "New Session landscape geometry"; geometry.lifetime = .keepAlways; add(geometry)
        let landscape = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        landscape.name = "New Session landscape largest text"; landscape.lifetime = .keepAlways; add(landscape)
        close.tap()
        XCUIDevice.shared.orientation = .portrait
        app.buttons["new-session"].tap()
        XCTAssertTrue(join.waitForExistence(timeout: 5))
        app.navigationBars["New Session"].swipeDown()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
    }

    func testInvitationSheetKeepsFailedDraftAndCancels() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 30))
        let ids = sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }
        openInvitation(app)
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText("not-a-session")
        app.buttons["Open"].firstMatch.tap()
        let error = app.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "session code must be 22 base58 characters")).firstMatch
        XCTAssertTrue(error.waitForExistence(timeout: 5))
        let failed = XCTAttachment(screenshot: app.screenshot())
        failed.name = "Invalid invitation keeps draft"; failed.lifetime = .keepAlways; add(failed)
        guard field.waitForExistence(timeout: 3) else {
            XCTFail("Invalid invitation discarded its editable draft"); return
        }
        XCTAssertEqual(field.value as? String, "not-a-session")
        app.buttons["Cancel"].firstMatch.tap()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
        openInvitation(app)
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        XCTAssertTrue((field.value as? String).map { $0.isEmpty || $0 == field.placeholderValue } ?? false)
        XCTAssertFalse(app.buttons["Open"].firstMatch.isEnabled)
        app.buttons["Cancel"].firstMatch.tap()
    }

    func testInvitationSheetCancelSuppressesLatePreview() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-delay-invitation-preview"]
        app.launch()
        XCTAssertTrue(sessionRows(app).firstMatch.waitForExistence(timeout: 30))
        openInvitation(app)
        let field = app.textFields["invitation-link"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText("not-a-session")
        app.buttons["Open"].firstMatch.tap()
        XCTAssertFalse(app.buttons["invitation-open"].isEnabled)
        app.buttons["Cancel"].firstMatch.tap()
        app.navigationBars.buttons["Sign in"].tap()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 5))
        let reopened = XCTNSPredicateExpectation(predicate: NSPredicate(format: "exists == true"), object: field)
        reopened.isInverted = true
        XCTAssertEqual(XCTWaiter.wait(for: [reopened], timeout: 4), .completed)
        XCTAssertTrue(app.textFields["sign-in-email"].exists)
        XCTAssertFalse(app.alerts["Couldn’t open invitation"].exists)
    }

    func testNamedSessionsKeepRootDistinctionsAndIdentityAfterRestart() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-session-list"]
        app.launch()
        let rows = sessionRows(app)
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        XCTAssertEqual(rows.count, 3)
        let addSession = app.buttons["new-session"]
        XCTAssertTrue(addSession.isHittable)
        XCTAssertGreaterThan(rows.firstMatch.frame.minY, app.frame.midY)
        XCTAssertGreaterThan(addSession.frame.minY, rows.element(boundBy: 2).frame.maxY)
        XCTAssertLessThan(addSession.frame.maxY, app.frame.maxY)
        XCTAssertFalse(app.staticTexts["Data"].exists)
        let ids = rows.allElementsBoundByIndex.map { retainedID($0) }
        XCTAssertEqual(Set(ids).count, 3)
        XCTAssertTrue(ids.allSatisfy { Identifier.valid($0, bytes: 32) })
        for row in rows.allElementsBoundByIndex { XCTAssertTrue(row.staticTexts["todo"].exists) }
        XCTAssertEqual(rows.allElementsBoundByIndex.filter { ($0.value as? String)?.contains("Production") == true }.count, 2)
        XCTAssertEqual(rows.allElementsBoundByIndex.filter { ($0.value as? String)?.contains("Development") == true }.count, 1)
        let named = XCTAttachment(screenshot: app.screenshot())
        named.name = "Native named sessions with distinct roots"; named.lifetime = .keepAlways; add(named)
        rows.firstMatch.tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 20))
        app.navigationBars.buttons["Sessions"].tap()
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 10))
        XCTAssertEqual(rows.allElementsBoundByIndex.map { retainedID($0) }, ids)
        XCUIDevice.shared.press(.home)
        XCTAssertTrue(app.wait(for: .runningBackground, timeout: 5))
        app.terminate(); app.launch()
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        XCTAssertEqual(rows.allElementsBoundByIndex.map { retainedID($0) }, ids)
        XCTAssertEqual(rows.allElementsBoundByIndex.filter { ($0.value as? String)?.contains("Production") == true }.count, 2)
        let retained = XCTAttachment(screenshot: app.screenshot())
        retained.name = "Named sessions retained after restart"; retained.lifetime = .keepAlways; add(retained)
        XCUIDevice.shared.press(.home)
        XCTAssertTrue(app.wait(for: .runningBackground, timeout: 5))
    }

    func testSessionEditingReordersAndLeavesOnlyOneLocalEntry() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-session-list"]
        app.launch()
        let rows = sessionRows(app)
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        XCTAssertEqual(rows.count, 3)
        let original = rows.allElementsBoundByIndex.map { retainedID($0) }
        app.navigationBars["Sessions"].buttons["Edit sessions"].tap()
        XCTAssertTrue(app.navigationBars["Editing Sessions"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.buttons["new-session"].isHittable)
        let source = rows.element(boundBy: 2).coordinate(withNormalizedOffset: CGVector(dx: 0.94, dy: 0.5))
        let target = rows.element(boundBy: 0).coordinate(withNormalizedOffset: CGVector(dx: 0.94, dy: 0.5))
        source.press(forDuration: 0.2, thenDragTo: target)
        let reordered = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            self.retainedID(rows.firstMatch) == original[2]
        }, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [reordered], timeout: 5), .completed)
        let edit = XCTAttachment(screenshot: app.screenshot())
        edit.name = "Native session editing and saved order"; edit.lifetime = .keepAlways; add(edit)
        app.navigationBars["Editing Sessions"].buttons["Done"].tap()
        app.terminate(); app.launch()
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        XCTAssertEqual(retainedID(rows.firstMatch), original[2])
        app.navigationBars["Sessions"].buttons["Edit sessions"].tap()
        rows.firstMatch.tap()
        let leave = app.cells["session-detail-0-4"]
        XCTAssertTrue(leave.waitForExistence(timeout: 5))
        XCTAssertEqual(app.cells["session-detail-0-0"].value as? String, original[2])
        XCTAssertTrue(app.staticTexts["Application"].exists)
        XCTAssertTrue(app.staticTexts["Root volume"].exists)
        let details = XCTAttachment(screenshot: app.screenshot())
        details.name = "Actual session identity and volume details"; details.lifetime = .keepAlways; add(details)
        app.cells["session-detail-0-3"].tap()
        let closeShare = app.buttons["Close"]
        XCTAssertTrue(closeShare.waitForExistence(timeout: 5))
        let share = XCTAttachment(screenshot: app.screenshot())
        share.name = "Native Live Share invitation sheet"; share.lifetime = .keepAlways; add(share)
        closeShare.tap()
        XCTAssertTrue(leave.waitForExistence(timeout: 5))
        leave.tap()
        XCTAssertTrue(app.alerts["Leave this session?"].waitForExistence(timeout: 5))
        app.alerts.buttons["Cancel"].tap()
        XCTAssertTrue(leave.exists)
        leave.tap(); app.alerts.buttons["Leave Session"].tap()
        XCTAssertTrue(app.navigationBars["Editing Sessions"].waitForExistence(timeout: 5))
        XCTAssertEqual(rows.count, 2)
        XCTAssertFalse(app.cells["session-row:" + original[2]].exists)
        app.navigationBars["Editing Sessions"].buttons["Done"].tap()
        app.terminate(); app.launch()
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        XCTAssertEqual(rows.count, 2)
        XCTAssertFalse(app.cells["session-row:" + original[2]].exists)
        rows.firstMatch.tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 30))
    }

    func testSessionsLargeTextAndLandscapeKeepActionsReachable() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-session-list",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryAccessibilityXXXL"]
        app.launch()
        let rows = sessionRows(app)
        XCTAssertTrue(rows.firstMatch.waitForExistence(timeout: 30))
        guard rows.count == 3 else {
            XCTFail("Requires a fresh app profile for the three-session fixture")
            return
        }
        XCTAssertGreaterThan(rows.firstMatch.frame.height, 100, "Accessibility text size must be active")
        let ids = rows.allElementsBoundByIndex.map { retainedID($0) }
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        let landscape = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.frame.width > app.frame.height
                && app.windows.firstMatch.frame.width > app.windows.firstMatch.frame.height
        }, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [landscape], timeout: 5), .completed)
        let addSession = app.buttons["new-session"]
        for _ in 0..<6 where !addSession.isHittable { app.tables["sessions-list"].swipeUp() }
        XCTAssertTrue(addSession.isHittable)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Large text landscape Sessions remains scrollable"; capture.lifetime = .keepAlways; add(capture)
        addSession.tap()
        XCTAssertTrue(app.cells["new-session-join"].waitForExistence(timeout: 5))
        let close = app.buttons["new-session-close"]
        XCTAssertTrue(close.isHittable)
        let sheet = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        sheet.name = "Large text landscape New Session remains closable"; sheet.lifetime = .keepAlways; add(sheet)
        close.tap()
        XCTAssertTrue(addSession.waitForExistence(timeout: 5))
        XCTAssertEqual(sessionRows(app).allElementsBoundByIndex.map { retainedID($0) }, ids)
    }

    func testAccountIDCopiesBase58AndRestoresTruncatedRow() {
        // Copy the full canonical ID through the actual row, then paste into
        // an unsaved native field. Feedback must restore after its two-second deadline.
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-account-settings"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Account"].waitForExistence(timeout: 30))
        let row = app.cells["account-id"]
        guard let fullID = row.value as? String else { XCTFail("Missing account ID"); return }
        let alphabet = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"
        XCTAssertTrue((16...22).contains(fullID.utf8.count))
        XCTAssertTrue(fullID.allSatisfy { alphabet.contains($0) })
        let shortened = String(fullID.prefix(10)) + "…"
        XCTAssertTrue(row.staticTexts[shortened].exists)
        row.tap()
        XCTAssertTrue(row.staticTexts["✓ Copied"].waitForExistence(timeout: 1))
        let copied = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        copied.name = "Account ID copied feedback"; copied.lifetime = .keepAlways; add(copied)
        XCTAssertTrue(row.staticTexts[shortened].waitForExistence(timeout: 3))
        XCTAssertEqual(row.value as? String, fullID)
        app.cells["account-name"].tap()
        let field = app.textFields["account-display-name"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).tap()
        field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue,
            count: (field.value as? String ?? "").count))
        field.press(forDuration: 1.1)
        let paste = app.buttons["Paste"].exists ? app.buttons["Paste"] : app.menuItems["Paste"]
        XCTAssertTrue(paste.waitForExistence(timeout: 5)); paste.tap()
        XCTAssertEqual(field.value as? String, fullID)
        app.navigationBars["Display name"].buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.cells["account-name"].staticTexts["Robin"].exists)
    }

    func testAccountLargeTextAndLandscapeKeepsSignOutReachable() {
        // Native scrolling must keep recovery and confirmed sign-out reachable
        // at the largest supported accessibility text size and in landscape.
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-account-settings",
            "-UIPreferredContentSizeCategoryName", "UICTContentSizeCategoryAccessibilityXXXL"]
        app.launch()
        XCTAssertTrue(app.navigationBars["Account"].waitForExistence(timeout: 30))
        XCUIDevice.shared.orientation = .landscapeLeft
        defer { XCUIDevice.shared.orientation = .portrait }
        let landscape = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.frame.width > app.frame.height
        }, object: app)
        XCTAssertEqual(XCTWaiter.wait(for: [landscape], timeout: 5), .completed)
        let signOut = app.cells["account-sign-out"]
        for _ in 0..<6 where !signOut.isHittable { app.tables["account-settings"].swipeUp() }
        XCTAssertTrue(signOut.isHittable)
        signOut.tap()
        let alert = app.alerts["Sign out?"]
        XCTAssertTrue(alert.waitForExistence(timeout: 5))
        alert.buttons["Cancel"].tap()
        XCTAssertTrue(app.navigationBars["Account"].exists)
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Large text landscape Account keeps Sign Out reachable"
        capture.lifetime = .keepAlways; add(capture)
    }

    func testAccountSignOutPreservesLocalSessionsAcrossRestart() {
        // The offline fixture owns ordinary local data. Cancel and confirmed
        // sign-out must preserve it, including reopening a previously running app.
        continueAfterFailure = false
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-account-settings"]
        app.launch()
        let account = app.navigationBars["Account"]
        XCTAssertTrue(account.waitForExistence(timeout: 30))
        XCTAssertTrue(app.cells["account-name"].staticTexts["Robin"].exists)
        app.cells["account-name"].tap()
        let field = app.textFields["account-display-name"]
        XCTAssertTrue(field.waitForExistence(timeout: 5))
        field.tap(); field.typeText(" edited")
        app.buttons["account-display-name-save"].tap()
        XCTAssertTrue(account.waitForExistence(timeout: 5))
        XCTAssertTrue(app.cells["account-name"].staticTexts["Robin edited"].waitForExistence(timeout: 5))
        app.cells["account-recovery"].tap()
        // This fixture is offline and unenrolled: recovery must report that
        // restriction while leaving account navigation and Sign Out usable.
        let recoveryError = app.alerts["Couldn’t open recovery"]
        XCTAssertTrue(recoveryError.waitForExistence(timeout: 5))
        recoveryError.buttons["OK"].tap()
        XCTAssertTrue(account.waitForExistence(timeout: 5))
        let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
        capture.name = "Account identity, recovery and Sign Out"
        capture.lifetime = .keepAlways; add(capture)
        let signOut = app.cells["account-sign-out"]
        signOut.tap()
        let alert = app.alerts["Sign out?"]
        XCTAssertTrue(alert.waitForExistence(timeout: 5))
        alert.buttons["Cancel"].tap()
        XCTAssertTrue(account.exists)
        account.buttons.element(boundBy: 0).tap()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 5))
        let session = retainedID(row)
        row.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 30))
        let task = "Retained after sign-out"
        input.tap(); input.typeText(task + "\n")
        let saved = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        XCTAssertTrue(saved.waitForExistence(timeout: 5))
        app.navigationBars.buttons["Sessions"].tap()
        // Reenter Account using the same durable fixture, without reseeding data.
        app.terminate(); app.launch()
        XCTAssertTrue(account.waitForExistence(timeout: 30))
        // The synthetic profile is unenrolled; the qualification entry opens
        // Account directly. Runtime tests separately cover enrolled credentials.
        signOut.tap()
        XCTAssertTrue(alert.waitForExistence(timeout: 5))
        alert.buttons["Sign Out"].tap()
        XCTAssertTrue(app.navigationBars["Sign in"].waitForExistence(timeout: 10))
        app.navigationBars["Sign in"].buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.cells["session-row:" + session].waitForExistence(timeout: 5))
        app.cells["session-row:" + session].tap()
        XCTAssertTrue(saved.waitForExistence(timeout: 30))
        app.terminate()
        app.launchArguments = []
        app.launch()
        // Ordinary iOS launch may open the sign-in form; it must remain
        // dismissible so retained local sessions can still be used offline.
        if app.navigationBars["Sign in"].waitForExistence(timeout: 5) {
            app.navigationBars["Sign in"].buttons.element(boundBy: 0).tap()
        }
        XCTAssertTrue(app.cells["session-row:" + session].waitForExistence(timeout: 30))
        app.cells["session-row:" + session].tap()
        XCTAssertTrue(saved.waitForExistence(timeout: 30))
    }

    func testDisplayNameSavesOfflineAndBackDiscardsDraft() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-account-profile"]
        app.launch()
        let field = app.textFields["account-display-name"]
        let save = app.buttons["account-display-name-save"]
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Robin"), object: field)], timeout: 5), .completed)
        field.tap()
        field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: 5))
        XCTAssertFalse(save.isEnabled)
        field.typeText("   ")
        XCTAssertFalse(save.isEnabled)
        XCTAssertTrue(app.staticTexts["Enter a display name using one line of text."].exists)
        field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: 3) + "Robin edited")
        XCTAssertTrue(save.isEnabled)
        let editing = XCTAttachment(screenshot: app.screenshot())
        editing.name = "Native display name editing"; editing.lifetime = .keepAlways; add(editing)
        save.tap()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 5))
        app.terminate(); app.launch()
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Robin edited"), object: field)], timeout: 5), .completed)
        field.tap(); field.typeText(" unsaved")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 5))
        app.terminate(); app.launch()
        XCTAssertTrue(field.waitForExistence(timeout: 20))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Robin edited"), object: field)], timeout: 5), .completed)
        let retained = XCTAttachment(screenshot: app.screenshot())
        retained.name = "Saved name survives restart and canceled draft"; retained.lifetime = .keepAlways; add(retained)
    }

    // Uses the ordinary empty-profile launch and real AP TLS. No qualification
    // seed, email request, enrollment or copied credentials are involved.
    func testSceneLaunchResumesAndReceivesColdAndWarmURLs() {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 20))
        XCUIApplication(bundleIdentifier: "com.apple.springboard").activate()
        // iOS may suspend the backgrounded process before XCTest observes it.
        let background = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [background], timeout: 10), .completed)
        let state = XCTAttachment(string: "Background application state: \(app.state)")
        state.lifetime = .keepAlways; add(state)
        app.activate()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 10))
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 5))
        // Invalid links exercise URL delivery without an account or AP request.
        // Opening over an existing sheet must dismiss it before reporting the link.
        app.buttons["new-session"].tap()
        app.open(URL(string: "playbit:session:public:invalid!")!)
        let error = app.alerts["Couldn’t open invitation"]
        XCTAssertTrue(error.waitForExistence(timeout: 10))
        error.buttons["OK"].tap()
        app.terminate()
        app.open(URL(string: "playbit:session:public:invalid!")!)
        XCTAssertTrue(error.waitForExistence(timeout: 10))
        error.buttons["OK"].tap()
        XCTAssertTrue(app.buttons["new-session"].exists)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Scene launch and URL recovery"; capture.lifetime = .keepAlways; add(capture)
    }

    func testBackgroundInvitationResumesBeforeRuntimeRequest() {
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 20))
        XCUIApplication(bundleIdentifier: "com.apple.springboard").activate()
        // iOS may suspend the backgrounded process before XCTest observes it.
        let background = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            app.state == .runningBackground || app.state == .runningBackgroundSuspended
        }, object: nil)
        XCTAssertEqual(XCTWaiter.wait(for: [background], timeout: 10), .completed)
        let state = XCTAttachment(string: "Background application state: \(app.state)")
        state.lifetime = .keepAlways; add(state)
        // Deliver the URL while backgrounded, as Safari does. Activating first
        // would hide the ordering between URL delivery and scene foregrounding.
        app.open(URL(string: "playbit:session:public:invalid!")!)
        let error = app.alerts["Couldn’t open invitation"]
        XCTAssertTrue(error.waitForExistence(timeout: 10))
        XCTAssertTrue(error.staticTexts.containing(NSPredicate(format: "label CONTAINS %@",
            "session code must be 22 base58 characters")).firstMatch.exists,
            "The resumed runtime must validate the link instead of rejecting it as paused")
        error.buttons["OK"].tap()
        XCTAssertFalse(app.staticTexts["Playbit is paused. Return to the app to continue."].exists)
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Background URL resumes runtime"; capture.lifetime = .keepAlways; add(capture)
    }

    func testSignInPresentationKeepsLocalNavigationAvailable() {
        let app = XCUIApplication()
        app.launchEnvironment["PLAYBIT_CHANNEL"] = "testing"
        app.launch()
        let email = app.textFields["sign-in-email"]
        if !email.waitForExistence(timeout: 4) {
            let manual = app.navigationBars.buttons["Sign in"]
            XCTAssertTrue(manual.waitForExistence(timeout: 10))
            if manual.isHittable { manual.tap() }
        }
        XCTAssertTrue(email.waitForExistence(timeout: 5))
        let send = app.buttons["sign-in-submit"]
        XCTAssertFalse(send.isEnabled)
        XCTAssertTrue(app.secureTextFields["sign-in-account-key"].exists)
        XCTAssertEqual(send.label, "Continue")
        XCTAssertTrue(app.buttons["User agreement"].exists)
        XCTAssertTrue(app.buttons["Privacy policy"].exists)
        app.buttons["User agreement"].tap()
        let agreement = app.textViews["user-agreement-document"]
        XCTAssertTrue(agreement.waitForExistence(timeout: 5))
        XCTAssertTrue((agreement.value as? String)?.hasSuffix("Terms revision 1") == true)
        XCTAssertFalse(app.buttons["user-agreement-accept"].exists)
        let terms = XCTAttachment(screenshot: app.screenshot())
        terms.name = "Bundled user agreement before sign-in"; terms.lifetime = .keepAlways; add(terms)
        for _ in 0..<4 { agreement.swipeUp(velocity: .fast) }
        let footer = XCTAttachment(screenshot: app.screenshot())
        footer.name = "User agreement revision at end"; footer.lifetime = .keepAlways; add(footer)
        app.navigationBars["User agreement"].buttons.element(boundBy: 0).tap()
        let initial = XCTAttachment(screenshot: app.screenshot())
        initial.name = "Native sign-in presentation"; initial.lifetime = .keepAlways; add(initial)
        email.tap()
        email.typeText("presentation-check@example.invalid")
        XCTAssertTrue(send.isEnabled)
        let key = app.secureTextFields["sign-in-account-key"]
        key.tap(); key.typeText("a123\n")
        let rejected = XCTNSPredicateExpectation(predicate: NSPredicate(format:
            "label CONTAINS %@", "enter the complete account key"), object: app.staticTexts["sign-in-status"])
        XCTAssertEqual(XCTWaiter.wait(for: [rejected], timeout: 5), .completed)
        XCTAssertTrue(email.exists)
        XCTAssertTrue(key.isEnabled)
        XCTAssertTrue(send.isEnabled)
        XCTAssertFalse(app.textFields["sign-in-code"].exists, "An incomplete key must not send an email code")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        let open = app.buttons["new-session"]
        XCTAssertTrue(open.waitForExistence(timeout: 5))
        open.tap()
        app.cells["new-session-join"].tap()
        XCTAssertTrue(app.textFields["invitation-link"].waitForExistence(timeout: 5))
        app.buttons["Cancel"].firstMatch.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "exists == false"), object: app.textFields["invitation-link"])], timeout: 5), .completed)
        XCTAssertFalse(email.exists)
        let local = XCTAttachment(screenshot: app.screenshot())
        local.name = "Back retains local navigation"; local.lifetime = .keepAlways; add(local)
        app.navigationBars.buttons["Sign in"].tap()
        XCTAssertTrue(email.waitForExistence(timeout: 5))
        XCTAssertFalse(send.isEnabled)
    }

    func testInvitationLinkRequiresAgreementAfterBackingOutOfReview() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_AGREEMENT_QUALIFICATION"] == "1" else {
            throw XCTSkip("Requires an isolated enrolled Testing profile with no agreement receipt.")
        }
        let app = XCUIApplication()
        app.launchEnvironment["PLAYBIT_CHANNEL"] = "testing"
        app.launch()
        let accept = app.buttons["user-agreement-accept"]
        XCTAssertTrue(accept.waitForExistence(timeout: 10))
        let link = URL(string: "playbit:session:testing:" + String(repeating: "1", count: 22))!
        app.open(link)
        let action = app.buttons["invitation-action"]
        XCTAssertTrue(action.waitForExistence(timeout: 10))
        XCTAssertEqual(action.label, "Review agreement")
        action.tap()
        XCTAssertTrue(accept.waitForExistence(timeout: 5))
        app.navigationBars["User agreement"].buttons.element(boundBy: 0).tap()
        XCTAssertTrue(action.waitForExistence(timeout: 5))
        XCTAssertEqual(action.label, "Review agreement", "Back cannot authorize a transfer.")
        let gated = XCTAttachment(screenshot: app.screenshot())
        gated.name = "Invitation awaiting agreement"; gated.lifetime = .keepAlways; add(gated)
        action.tap()
        XCTAssertTrue(accept.waitForExistence(timeout: 5))
        accept.tap()
        XCTAssertTrue(action.waitForExistence(timeout: 10))
        let canCheckInvitation = XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "label != %@", "Review agreement"), object: action)
        XCTAssertEqual(XCTWaiter.wait(for: [canCheckInvitation], timeout: 10), .completed)
    }

    func testInvitationPauseSurvivesRestartAndResumeOnlyObserves() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        let sessionID = retainedID(row)
        XCTAssertTrue(Identifier.valid(sessionID, bytes: 32))
        row.tap()
        XCTAssertTrue(app.textFields["guest-input-1"].waitForExistence(timeout: 40))
        app.navigationBars.buttons["App"].tap()
        app.buttons["Copy invitation"].tap()
        XCTAssertTrue(app.staticTexts["Invitation copied"].waitForExistence(timeout: 5))
        app.navigationBars.buttons.element(boundBy: 0).tap()
        openInvitation(app)
        let link = app.textFields["invitation-link"]
        XCTAssertTrue(link.waitForExistence(timeout: 5))
        // A retained-session link opens its guest directly, even after Leave
        // (which hides the row but keeps local data). Use an unregistered seed
        // to exercise the pre-enrollment invitation state instead.
        link.tap()
        link.typeText("https://playbit.app/s#1111111111111111111111")
        app.buttons["invitation-open"].tap()
        let action = app.buttons["invitation-action"]
        XCTAssertTrue(action.waitForExistence(timeout: 10))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "label == %@", "Sign in"), object: action)], timeout: 10), .completed)
        app.navigationBars.buttons["Pause"].tap()
        let invitation = app.cells["invitation-row"].firstMatch
        XCTAssertTrue(invitation.waitForExistence(timeout: 10))
        app.terminate(); app.launch()
        XCTAssertTrue(invitation.waitForExistence(timeout: 40)); invitation.tap()
        XCTAssertTrue(action.waitForExistence(timeout: 10))
        XCTAssertEqual(action.label, "Resume")
        XCTAssertFalse(app.navigationBars.buttons["Pause"].exists)
        let paused = XCTAttachment(screenshot: app.screenshot())
        paused.name = "Invitation pause retained after restart"; paused.lifetime = .keepAlways; add(paused)
        action.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "label == %@", "Sign in"), object: action)], timeout: 10), .completed)
        XCTAssertTrue(app.navigationBars.buttons["Pause"].exists)
        XCTAssertFalse(app.textFields["guest-input-1"].exists)
        let resumed = XCTAttachment(screenshot: app.screenshot())
        resumed.name = "Resume observes without joining"; resumed.lifetime = .keepAlways; add(resumed)
        action.tap()
        let email = app.textFields["sign-in-email"]
        let submit = app.buttons["sign-in-submit"]
        XCTAssertTrue(email.waitForExistence(timeout: 5))
        XCTAssertFalse(submit.isEnabled)
        let empty = XCTAttachment(screenshot: app.screenshot())
        empty.name = "Empty email disables sending"; empty.lifetime = .keepAlways; add(empty)
        email.tap(); email.typeText("alice")
        XCTAssertFalse(submit.isEnabled)
        email.typeText("@")
        XCTAssertFalse(submit.isEnabled)
        email.typeText("example.invalid")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == true"), object: submit)], timeout: 5), .completed)
        let valid = XCTAttachment(screenshot: app.screenshot())
        valid.name = "Complete email enables sending"; valid.lifetime = .keepAlways; add(valid)
        email.typeText(" second")
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "enabled == false"), object: submit)], timeout: 5), .completed)
        // No mail is sent by this signed-out fixture. Leaving the form keeps
        // the original invitation and its authority for a later sign-in.
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(action.waitForExistence(timeout: 5))
        XCTAssertEqual(action.label, "Sign in")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertEqual(retainedID(row), sessionID)
    }



    // Use export_guest_trap_fixture's ordinary session in an isolated profile.
    // Qualification keeps its transport offline; the guest itself really traps.
    func testOrdinaryGuestTrapStopsWithAppSpecificMessage() throws {
        guard let session = ProcessInfo.processInfo.environment["PLAYBIT_W7_ORDINARY_SESSION"] else {
            throw XCTSkip("Requires the isolated ordinary guest-trap fixture")
        }
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = app.cells["session-row:" + session]
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        let trap = app.buttons["Trap"]
        XCTAssertTrue(trap.waitForExistence(timeout: 40)); trap.tap()
        let failure = app.staticTexts.matching(NSPredicate(format:
            "label CONTAINS %@", "Crash Demo stopped")).firstMatch
        XCTAssertTrue(failure.waitForExistence(timeout: 10))
        XCTAssertFalse(app.buttons["Reload app"].exists)
        trap.tap()
        XCTAssertTrue(failure.exists, "Repeated input must preserve the named stopped state")
        let stopped = XCTAttachment(screenshot: app.screenshot())
        stopped.name = "W7 ordinary guest stays stopped with an app-specific message"
        stopped.lifetime = .keepAlways; add(stopped)
    }

    // Run with the isolated trapping starter; ordinary packaging never includes it.
    func testFailedGuestQuitReportsDrainAndAllowsReopen() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_QUIT_FAILURE_FIXTURE"] == "1" else {
            throw XCTSkip("Requires the isolated trapping starter")
        }
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        let trap = app.buttons["Trap"]
        XCTAssertTrue(trap.waitForExistence(timeout: 40)); trap.tap()
        app.navigationBars.buttons["App"].tap(); app.buttons["Quit app"].tap()
        let alert = app.alerts["App closed before finishing its work"]
        XCTAssertTrue(alert.waitForExistence(timeout: 10))
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Failed guest Quit reports incomplete work and retires its owner"
        capture.lifetime = .keepAlways; add(capture)
        alert.buttons["OK"].tap()
        XCTAssertTrue(row.waitForExistence(timeout: 10)); row.tap()
        XCTAssertTrue(trap.waitForExistence(timeout: 10))
        app.navigationBars.buttons["App"].tap(); app.buttons["Quit app"].tap()
        XCTAssertTrue(row.waitForExistence(timeout: 10), "The fresh owner drains normally")
    }

    func testEnginePanicRequiresFreshProcessAndExplicitRestart() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        let title = "W7 retained " + UUID().uuidString.prefix(8)
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        input.tap(); input.typeText(title + "\n")
        let todo = app.buttons.matching(NSPredicate(format: "label == %@", title)).firstMatch
        XCTAssertTrue(todo.waitForExistence(timeout: 10))
        XCTAssertEqual(todo.value as? String, "Not done")
        app.terminate()
        app.launchArguments = ["--phase1-qualify", "--w7-panic-on-input"]
        app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        XCTAssertTrue(todo.waitForExistence(timeout: 40))
        let restart = app.buttons["Restart Playbit"]
        // Physical-device XCTest reports the deliberately aborted app; the
        // simulator may not. Match only one crash report, not any assertion.
        let expectedAbort = XCTExpectedFailure.Options()
        expectedAbort.isStrict = false
        let crashLock = NSLock()
        var crashReportPending = true
        expectedAbort.issueMatcher = { issue in
            crashLock.lock(); defer { crashLock.unlock() }
            guard crashReportPending, issue.compactDescription.contains(" crashed in ") else { return false }
            crashReportPending = false
            return true
        }
        XCTExpectFailure("W7 intentionally aborts the failed engine process", options: expectedAbort)
        todo.tap()
        XCTAssertTrue(app.wait(for: .notRunning, timeout: 15))
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        XCTAssertTrue(restart.waitForExistence(timeout: 15))
        crashLock.lock(); crashReportPending = false; crashLock.unlock()
        XCTAssertFalse(row.exists, "The failed engine must not reopen before explicit restart")
        let stopped = XCTAttachment(screenshot: app.screenshot())
        stopped.name = "W7 engine crash offers explicit fresh-process restart"
        stopped.lifetime = .keepAlways; add(stopped)
        restart.tap()
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        XCTAssertTrue(todo.waitForExistence(timeout: 40))
        XCTAssertEqual(todo.value as? String, "Not done", "The failed action must not replay")
        todo.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == 'Done'"), object: todo)], timeout: 10), .completed)
        let recovered = XCTAttachment(screenshot: app.screenshot())
        recovered.name = "W7 retained TODO accepts new input after restart"
        recovered.lifetime = .keepAlways; add(recovered)
    }

    func testLiveSessionChannelsAndRootData() throws {
        guard let session = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_CHANNEL_SESSION"] else {
            throw XCTSkip("Requires the isolated channel-qualification AP fixture and a Mac participant")
        }
        continueAfterFailure = false
        let app = XCUIApplication()
        if let profile = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_CHANNEL_PROFILE"] {
            app.launchArguments = ["--phase1-profile", profile]
        }
        addUIInterruptionMonitor(withDescription: "Isolated AP local network access") { alert in
            let message = alert.staticTexts.matching(NSPredicate(format: "label CONTAINS[c] %@", "local network")).firstMatch
            guard message.exists && alert.buttons["Allow"].exists else { return false }
            alert.buttons["Allow"].tap(); return true
        }
        func openNote() {
            let row = app.cells["session-row:" + session]
            XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
            XCTAssertTrue(app.buttons["Send live preview"].waitForExistence(timeout: 30))
            let ready = XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: app.buttons["Wave"])
            XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 30), .completed)
        }
        func replaceDraft(_ value: String) {
            let input = app.textFields["guest-input-1"]
            for _ in 0..<4 {
                let previous = input.value as? String ?? ""
                if previous.isEmpty || previous == input.placeholderValue { break }
                input.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).tap()
                input.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: previous.count))
            }
            input.tap(); input.typeText(value)
            XCTAssertEqual(input.value as? String, value)
        }
        app.launch()
        // New iOS permission sheets are not always reported as XCTest alerts.
        let springboard = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        let allow = springboard.buttons["Allow"]
        if allow.waitForExistence(timeout: 5) {
            let purpose = springboard.staticTexts.matching(NSPredicate(format: "label CONTAINS %@", "isolated Playbit qualification")).firstMatch
            XCTAssertTrue(purpose.exists)
            allow.tap(); app.activate()
        }
        openNote()
        XCTAssertTrue(app.staticTexts["2 connected app instances · 0 waves"].waitForExistence(timeout: 60))
        replaceDraft("iPhone channel preview")
        app.buttons["Send live preview"].tap(); app.buttons["Wave"].tap()
        XCTAssertTrue(app.staticTexts["Live preview: Mac channel preview"].waitForExistence(timeout: 180))
        XCTAssertTrue(app.staticTexts["2 connected app instances · 2 waves"].waitForExistence(timeout: 60))
        replaceDraft("iPhone root record")
        app.buttons["Save"].tap()
        XCTAssertTrue(app.staticTexts["iPhone root record"].waitForExistence(timeout: 30))
        // Acknowledge visible readback before the Mac replaces this record.
        app.buttons["Wave"].tap()
        XCTAssertTrue(app.staticTexts["Mac root record"].waitForExistence(timeout: 180))
        let live = XCTAttachment(screenshot: app.screenshot())
        live.name = "Two live channels and root-backed data from the Mac"; live.lifetime = .keepAlways; add(live)
        app.terminate(); app.launch(); openNote()
        XCTAssertTrue(app.staticTexts["Mac root record"].waitForExistence(timeout: 30))
        XCTAssertTrue(app.staticTexts["2 connected app instances · 0 waves"].waitForExistence(timeout: 60))
        XCTAssertFalse(app.staticTexts["Live preview: Mac channel preview"].exists)
        let restarted = XCTAttachment(screenshot: app.screenshot())
        restarted.name = "Reconnect refreshes membership without replay; root data persists"; restarted.lifetime = .keepAlways; add(restarted)
        app.terminate()
    }

    func testSharedNoteFileSaveSurvivesRestart() {
        // Save and replace UTF-8 through the guest, then verify each file after restart.
        continueAfterFailure = false
        let app = XCUIApplication()
        let liveSession = ProcessInfo.processInfo.environment["PLAYBIT_LIVE_FILE_SESSION"]
        if liveSession == nil { app.launchArguments = ["--phase1-qualify"] }
        app.launch()
        let row = liveSession.map { app.cells["session-row:" + $0] } ?? sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        let sessionID = retainedID(row)
        XCTAssertTrue(Identifier.valid(sessionID, bytes: 32))
        row.tap()
        let input = app.textFields["guest-input-1"]
        let save = app.buttons["Save"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        let ready = XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: save)
        XCTAssertEqual(XCTWaiter.wait(for: [ready], timeout: 15), .completed)
        let text = "iPhone File API — café 東京 " + UUID().uuidString.prefix(8)
        func replaceDraft(_ value: String) {
            // A tap in a horizontally scrolled field may place the caret in
            // the middle. Clear the visible prefix, then the remaining suffix.
            for _ in 0..<4 {
                let previous = input.value as? String ?? ""
                if previous.isEmpty || previous == input.placeholderValue { break }
                input.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).tap()
                input.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: previous.count))
            }
            let cleared = input.value as? String ?? ""
            XCTAssertTrue(cleared.isEmpty || cleared == input.placeholderValue)
            input.tap()
            input.typeText(value)
            XCTAssertEqual(input.value as? String, value)
            input.typeText("\n")
        }
        replaceDraft(text)
        XCTAssertTrue(app.staticTexts[text].waitForExistence(timeout: 20))
        XCTAssertTrue(app.staticTexts["\(text.utf8.count) bytes · Shared file"].exists)
        XCTAssertTrue(save.isEnabled)
        let saved = XCTAttachment(screenshot: app.screenshot())
        saved.name = "File saved and read back on iPhone"; saved.lifetime = .keepAlways; add(saved)
        XCUIDevice.shared.press(.home)
        app.activate()
        XCTAssertTrue(app.staticTexts[text].waitForExistence(timeout: 10))
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        XCTAssertEqual(retainedID(row), sessionID)
        row.tap()
        XCTAssertTrue(app.staticTexts[text].waitForExistence(timeout: 30))
        XCTAssertEqual(input.value as? String, text)
        let replacement = "Replaced — 東京"
        replaceDraft(replacement)
        XCTAssertTrue(app.staticTexts[replacement].waitForExistence(timeout: 20))
        XCTAssertTrue(app.staticTexts["\(replacement.utf8.count) bytes · Shared file"].exists)
        XCTAssertTrue(save.isEnabled)
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        XCTAssertTrue(app.staticTexts[replacement].waitForExistence(timeout: 30))
        XCTAssertEqual(input.value as? String, replacement)
        let reopened = XCTAttachment(screenshot: app.screenshot())
        reopened.name = "Replacement file after iPhone process restart"
        reopened.lifetime = .keepAlways; add(reopened)
    }

    func testTodoSurvivesProcessRestart() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        let sessionID = retainedID(row)
        XCTAssertTrue(Identifier.valid(sessionID, bytes: 32))
        row.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        XCTAssertTrue(app.staticTexts["Offline; local data is retained"].exists)
        let title = "iPhone restart " + UUID().uuidString.prefix(8)
        input.tap(); input.typeText(title + "\n")
        let todo = app.buttons.matching(NSPredicate(format: "label == %@", title)).firstMatch
        XCTAssertTrue(todo.waitForExistence(timeout: 15))
        todo.tap()
        let done = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == %@", "Done"), object: todo)
        XCTAssertEqual(XCTWaiter.wait(for: [done], timeout: 10), .completed)
        XCTAssertTrue(app.staticTexts["Offline; local data is retained"].exists)
        let first = XCTAttachment(screenshot: app.screenshot()); first.name = "TODO after edit"; first.lifetime = .keepAlways; add(first)
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        XCTAssertEqual(retainedID(row), sessionID)
        row.tap()
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label == %@", title)).firstMatch.waitForExistence(timeout: 40))
        XCTAssertEqual(app.buttons.matching(NSPredicate(format: "label == %@", title)).firstMatch.value as? String, "Done")
        app.navigationBars.buttons["App"].tap()
        app.buttons["Copy invitation"].tap()
        XCTAssertTrue(app.staticTexts["Invitation copied"].waitForExistence(timeout: 5))
        let restored = XCTAttachment(screenshot: app.screenshot()); restored.name = "TODO after process restart"; restored.lifetime = .keepAlways; add(restored)
    }
    func testTodoSurfaceChangesPreserveFocusedDraftAndData() {
        XCUIDevice.shared.orientation = .portrait
        defer { XCUIDevice.shared.orientation = .portrait }
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"], addButton = app.buttons["guest-control-2"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        func layout(_ compact: Bool) {
            let positioned = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
                guard input.exists, addButton.exists, input.frame.width > 0 else { return false }
                return compact ? addButton.frame.minY >= input.frame.maxY :
                    abs(addButton.frame.midY - input.frame.midY) < 2 && addButton.frame.minX >= input.frame.maxX
            }, object: nil)
            XCTAssertEqual(XCTWaiter.wait(for: [positioned], timeout: 10), .completed)
        }
        layout(true)
        var draft = "Surface " + UUID().uuidString.prefix(8)
        input.tap(); input.typeText(draft)
        for orientation: UIDeviceOrientation in [.landscapeLeft, .portrait, .landscapeRight, .portrait] {
            XCUIDevice.shared.orientation = orientation
            layout(orientation == .portrait)
            XCTAssertEqual(input.value as? String, draft)
            XCTAssertTrue(app.keyboards.firstMatch.exists)
            // typeText requires retained first-responder ownership; do not tap again.
            input.typeText("x"); draft += "x"
            let capture = XCTAttachment(screenshot: XCUIScreen.main.screenshot())
            capture.name = orientation == .portrait ? "Compact surface retains focused draft" : "Wide surface retains focused draft"
            capture.lifetime = .keepAlways; add(capture)
        }
        input.typeText("\n")
        let task = app.buttons.matching(NSPredicate(format: "label == %@", draft)).firstMatch
        XCTAssertTrue(task.waitForExistence(timeout: 10)); task.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Done"), object: task)], timeout: 10), .completed)
        app.terminate(); app.launch()
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        XCTAssertTrue(task.waitForExistence(timeout: 40)); XCTAssertEqual(task.value as? String, "Done")
        layout(true)
    }

    func testTodoDraftAndAddControls() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        let addButton = app.buttons["guest-control-2"]
        XCTAssertTrue(input.waitForExistence(timeout: 40)); XCTAssertTrue(addButton.exists)
        XCTAssertFalse(addButton.isEnabled)
        let title = "Native Add " + UUID().uuidString.prefix(8)
        input.tap(); input.typeText(String(title))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "enabled == true"), object: addButton)], timeout: 10), .completed)
        addButton.tap()
        let todo = app.buttons.matching(NSPredicate(format: "label == %@", title)).firstMatch
        XCTAssertTrue(todo.waitForExistence(timeout: 10))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == nil OR value == '' OR value == placeholderValue"), object: input)], timeout: 10), .completed)
        XCTAssertFalse(addButton.isEnabled)
        input.tap(); input.typeText("Retained draft")
        todo.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == %@", "Done"), object: todo)], timeout: 10), .completed)
        XCTAssertEqual(input.value as? String, "Retained draft")
        XCTAssertTrue(app.keyboards.firstMatch.exists)
        input.typeText(" after toggle")
        XCTAssertEqual(input.value as? String, "Retained draft after toggle")
        // Releasing outside a checkbox cancels without toggling.
        todo.coordinate(withNormalizedOffset: CGVector(dx: 0.1, dy: 0.5)).press(forDuration: 0.15, thenDragTo: app.navigationBars.firstMatch.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: 0.5)))
        XCTAssertEqual(todo.value as? String, "Done")
        input.typeText("\n")
        XCTAssertTrue(app.buttons.matching(NSPredicate(format: "label == %@", "Retained draft after toggle")).firstMatch.waitForExistence(timeout: 10))
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == nil OR value == '' OR value == placeholderValue"), object: input)], timeout: 10), .completed)
        let capture = XCTAttachment(screenshot: app.screenshot()); capture.name = "Native TODO draft retained through checkbox reply"; capture.lifetime = .keepAlways; add(capture)
        input.tap(); input.typeText("Navigation retains this instance")
        app.navigationBars.buttons["Sessions"].tap()
        XCTAssertTrue(session.waitForExistence(timeout: 10)); session.tap()
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        XCTAssertEqual(input.value as? String, "Navigation retains this instance")
        app.navigationBars.buttons["App"].tap(); app.buttons["Quit app"].tap()
        XCTAssertTrue(session.waitForExistence(timeout: 10)); session.tap()
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        XCTAssertEqual(input.value as? String, input.placeholderValue)
        XCTAssertEqual(todo.value as? String, "Done", "Quit retains the committed checkbox")
    }

    func testPersonalConsentPreservesGuestDraftAndSavedChoices() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-personal-volumes"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        input.tap(); input.typeText("Consent draft preserved")
        func openAccess() {
            app.navigationBars.buttons["App"].tap()
            app.buttons["Personal volumes"].tap()
            XCTAssertTrue(app.segmentedControls["Travel plans"].waitForExistence(timeout: 5))
        }
        func capture(_ name: String) {
            let image = XCTAttachment(screenshot: app.screenshot())
            image.name = name; image.lifetime = .keepAlways; add(image)
        }
        openAccess()
        let access = app.segmentedControls["Travel plans"]
        XCTAssertTrue(access.buttons["Off"].isSelected)
        capture("Personal access before selection")
        access.buttons["Read"].tap()
        app.navigationBars.buttons["Save"].tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        XCTAssertEqual(input.value as? String, "Consent draft preserved")
        openAccess()
        XCTAssertTrue(access.buttons["Read"].isSelected)
        access.buttons["Read/write"].tap()
        capture("Unsaved write access")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        openAccess()
        XCTAssertTrue(access.buttons["Read"].isSelected, "Back must discard the unsaved write escalation")
        access.buttons["Off"].tap()
        app.navigationBars.buttons["Save"].tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        XCTAssertEqual(input.value as? String, "Consent draft preserved")
        openAccess()
        XCTAssertTrue(access.buttons["Off"].isSelected)
        capture("Saved personal access revoked")
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        input.tap(); input.typeText("\n")
        XCTAssertTrue(app.buttons["Consent draft preserved"].waitForExistence(timeout: 10))
        capture("Retained draft remains usable after revocation")
    }

    func testSessionDetailsShowsSavedPersonalAccessAndRevocation() throws {
        let environment = ProcessInfo.processInfo.environment
        let liveSession = environment["PLAYBIT_LIVE_SESSION"]
        let volumeName: String
        let app = XCUIApplication()
        if liveSession != nil {
            volumeName = try XCTUnwrap(environment["PLAYBIT_LIVE_VOLUME_NAME"],
                "The live session requires an isolated personal volume created through ordinary tools")
        } else {
            volumeName = "Travel plans"
            app.launchArguments = ["--phase1-qualify", "--phase1-personal-volumes"]
        }
        app.launch()
        let row = liveSession.map { app.cells["session-row:" + $0] } ?? sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40))
        let session = retainedID(row)
        func openDetails() {
            app.navigationBars["Sessions"].buttons["Edit sessions"].tap()
            app.cells["session-row:" + session].tap()
            XCTAssertTrue(app.cells["session-detail-1-0"].waitForExistence(timeout: 5))
        }
        func openAccess() {
            app.cells["session-detail-1-0"].tap()
            XCTAssertTrue(app.segmentedControls[volumeName].waitForExistence(timeout: 5))
        }
        func save(_ choice: String, _ expected: String) {
            app.segmentedControls[volumeName].buttons[choice].tap()
            app.navigationBars.buttons["Save"].tap()
            XCTAssertTrue(app.navigationBars["Editing Session"].waitForExistence(timeout: 5))
            XCTAssertTrue(app.staticTexts[expected].waitForExistence(timeout: 5))
        }
        openDetails()
        XCTAssertTrue(app.staticTexts["Choose personal volumes…"].exists)
        openAccess()
        XCTAssertTrue(app.segmentedControls[volumeName].buttons["Off"].isSelected)
        save("Read", "Read only")
        XCTAssertTrue(app.staticTexts[volumeName].exists)
        let readOnly = XCTAttachment(screenshot: app.screenshot())
        readOnly.name = "Session details show saved read-only personal access"
        readOnly.lifetime = .keepAlways; add(readOnly)
        try app.performAccessibilityAudit()
        openAccess()
        app.segmentedControls[volumeName].buttons["Read/write"].tap()
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(app.staticTexts["Read only"].waitForExistence(timeout: 5), "Back must not grant write access")
        openAccess()
        XCTAssertTrue(app.segmentedControls[volumeName].buttons["Read"].isSelected)
        save("Read/write", "Read and write")
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 30)); openDetails()
        XCTAssertTrue(app.staticTexts["Read and write"].waitForExistence(timeout: 5))
        openAccess()
        try app.performAccessibilityAudit()
        save("Off", "Choose personal volumes…")
        XCTAssertFalse(app.staticTexts[volumeName].exists)
        app.terminate(); app.launch()
        XCTAssertTrue(row.waitForExistence(timeout: 30)); openDetails()
        XCTAssertTrue(app.staticTexts["Choose personal volumes…"].exists)
        XCTAssertEqual(app.cells["session-detail-0-0"].value as? String, session)
        openAccess()
        XCTAssertTrue(app.segmentedControls[volumeName].buttons["Off"].isSelected)
    }

    // Normal app and real account. The operator edits source only after the
    // session has a saved grant; updates and reopening retain that mount.
    func testLiveSourceUpdateRetainsSessionMounts() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let session = environment["PLAYBIT_LIVE_SESSION"],
              let volume = environment["PLAYBIT_LIVE_VOLUME_NAME"],
              let task = environment["PLAYBIT_LIVE_RETAINED_TASK"],
              let initial = environment["PLAYBIT_LIVE_INITIAL_HEADING"],
              let updated = environment["PLAYBIT_LIVE_UPDATED_HEADING"] else {
            throw XCTSkip("Requires a live session, completed task, personal volume and coordinated source update")
        }
        let marker = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("playbit-live-consent-ready")
        try? FileManager.default.removeItem(at: marker)
        defer { try? FileManager.default.removeItem(at: marker) }
        let app = XCUIApplication()
        app.launch()
        func capture(_ name: String) {
            let image = XCTAttachment(screenshot: app.screenshot())
            image.name = name; image.lifetime = .keepAlways; add(image)
        }
        let row = app.buttons.matching(NSPredicate(format: "label == %@", task)).firstMatch
        let sessionRow = app.cells["session-row:" + session]
        XCTAssertTrue(sessionRow.waitForExistence(timeout: 30)); sessionRow.tap()
        XCTAssertTrue(app.staticTexts[initial].waitForExistence(timeout: 60))
        XCTAssertTrue(row.waitForExistence(timeout: 10))
        let prepared = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == %@", "Done"), object: row)
        guard XCTWaiter.wait(for: [prepared], timeout: 30) == .completed else {
            XCTFail("The peer's completed-task fixture did not reach this device"); return
        }
        app.navigationBars.buttons["App"].tap()
        app.buttons["Personal volumes"].tap()
        let access = app.segmentedControls[volume]
        XCTAssertTrue(access.waitForExistence(timeout: 30))
        access.buttons["Read"].tap()
        app.navigationBars.buttons["Save"].tap()
        XCTAssertTrue(app.staticTexts[initial].waitForExistence(timeout: 10))
        try Data("session mounts saved".utf8).write(to: marker)
        XCTAssertTrue(app.staticTexts[updated].waitForExistence(timeout: 180))
        XCTAssertEqual(row.value as? String, "Done")
        row.tap()
        let edited = XCTNSPredicateExpectation(predicate: NSPredicate(format: "value == %@", "Not done"), object: row)
        XCTAssertEqual(XCTWaiter.wait(for: [edited], timeout: 10), .completed)
        capture("Updated app retains data and remains editable")
        app.navigationBars.buttons["App"].tap()
        app.buttons["Personal volumes"].tap()
        XCTAssertTrue(access.waitForExistence(timeout: 10))
        XCTAssertTrue(access.buttons["Read"].isSelected)
        capture("Session mount retained after update")
        app.terminate(); app.launch()
        XCTAssertTrue(sessionRow.waitForExistence(timeout: 30)); sessionRow.tap()
        XCTAssertTrue(app.staticTexts[updated].waitForExistence(timeout: 60))
        XCTAssertEqual(row.value as? String, "Not done")
        app.navigationBars.buttons["App"].tap()
        app.buttons["Personal volumes"].tap()
        XCTAssertTrue(access.waitForExistence(timeout: 10))
        XCTAssertTrue(access.buttons["Read"].isSelected)
    }

    func testPendingPersonalConsentDisablesEditsAndCannotDismissAnotherChooser() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-personal-volumes", "--phase1-delay-consent-response"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        input.tap(); input.typeText("Draft during pending save")
        app.navigationBars.buttons["App"].tap(); app.buttons["Personal volumes"].tap()
        let access = app.segmentedControls["Travel plans"]
        XCTAssertTrue(access.waitForExistence(timeout: 5))
        access.buttons["Read"].tap(); app.navigationBars.buttons["Save"].tap()
        let saving = app.navigationBars.buttons["Saving…"]
        XCTAssertTrue(saving.exists); XCTAssertFalse(saving.isEnabled)
        XCTAssertFalse(access.buttons["Read/write"].isEnabled)
        let busy = XCTAttachment(screenshot: app.screenshot())
        busy.name = "Pending consent disables editing and repeated save"; busy.lifetime = .keepAlways; add(busy)
        // Back is navigation, not a claim to cancel the dispatched durable write.
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        XCTAssertEqual(input.value as? String, "Draft during pending save")
        app.navigationBars.buttons["App"].tap(); app.buttons["Personal volumes"].tap()
        XCTAssertTrue(access.waitForExistence(timeout: 10))
        XCTAssertTrue(access.buttons["Read"].isSelected)
        XCTAssertTrue(app.navigationBars.buttons["Save"].isEnabled)
        let retained = XCTAttachment(screenshot: app.screenshot())
        retained.name = "Late result leaves the reopened chooser intact"; retained.lifetime = .keepAlways; add(retained)
    }

    func testTodoLogsShowActualRepeatedOutputAndKeepDraft() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        app.navigationBars.buttons["App"].tap(); app.buttons["Logs"].tap()
        XCTAssertTrue(app.staticTexts["No logs yet"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.navigationBars.buttons["Copy"].isEnabled)
        app.navigationBars.buttons.element(boundBy: 0).tap()
        input.tap(); input.typeText("First logged task\nSecond logged task\n")
        let first = app.buttons["First logged task"]
        XCTAssertTrue(first.waitForExistence(timeout: 10)); first.tap()
        input.tap(); input.typeText("Draft beside logs")
        app.navigationBars.buttons["App"].tap(); app.buttons["Logs"].tap()
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Actual TODO log output"; capture.lifetime = .keepAlways; add(capture)
        XCTAssertTrue(app.staticTexts["TODO added"].firstMatch.waitForExistence(timeout: 5))
        XCTAssertEqual(app.staticTexts.matching(identifier: "TODO added").count, 2)
        XCTAssertTrue(app.staticTexts["TODO changed"].exists)
        app.navigationBars.buttons["Copy"].tap()
        XCTAssertTrue(app.staticTexts["Copied 3 lines"].waitForExistence(timeout: 5))
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(input.waitForExistence(timeout: 5))
        XCTAssertEqual(input.value as? String, "Draft beside logs")
        input.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).press(forDuration: 1.1)
        let selectAll = app.buttons["Select All"].exists ? app.buttons["Select All"] : app.menuItems["Select All"]
        XCTAssertTrue(selectAll.waitForExistence(timeout: 5)); selectAll.tap()
        let paste = app.buttons["Paste"].exists ? app.buttons["Paste"] : app.menuItems["Paste"]
        XCTAssertTrue(paste.waitForExistence(timeout: 5)); paste.tap()
        let copied = input.value as? String ?? ""
        XCTAssertEqual(copied.components(separatedBy: "TODO added").count, 3)
        XCTAssertTrue(copied.contains("TODO changed"))
    }

    func testLogFailureKeepsSnapshotAndCanReopen() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-stop-logged-session"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        input.tap()
        input.typeText((0..<32).map { "Log \($0)\n" }.joined())
        XCTAssertTrue(app.buttons["Log 31"].waitForExistence(timeout: 10))
        app.navigationBars.buttons["App"].tap(); app.buttons["Logs"].tap()
        let failure = app.staticTexts["session is not running"]
        XCTAssertTrue(failure.waitForExistence(timeout: 10))
        // UIKit paints the pinned footer inside the viewport but XCTest reports
        // its static text as non-hittable. Verify visibility in the attachment;
        // neither hit testing nor existence alone proves visual conformance.
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Visible read failure with retained log snapshot"; capture.lifetime = .keepAlways; add(capture)
        XCTAssertTrue(app.staticTexts["TODO added"].firstMatch.exists)
        app.navigationBars.buttons["Retry"].tap()
        XCTAssertTrue(failure.waitForExistence(timeout: 5))
        XCTAssertTrue(app.staticTexts["TODO added"].firstMatch.exists)
        // Back to the guest, then Sessions. Reopening starts the retained guest
        // normally; the qualification hook stops it only once.
        app.navigationBars.buttons.element(boundBy: 0).tap()
        app.navigationBars.buttons.element(boundBy: 0).tap()
        XCTAssertTrue(session.waitForExistence(timeout: 5)); session.tap()
        XCTAssertTrue(input.waitForExistence(timeout: 10))
        XCTAssertTrue(app.buttons["Log 31"].exists)
        app.navigationBars.buttons["App"].tap(); app.buttons["Logs"].tap()
        XCTAssertTrue(app.staticTexts["No logs yet"].waitForExistence(timeout: 5))
        XCTAssertFalse(app.navigationBars.buttons["Copy"].isEnabled)
        XCTAssertFalse(app.navigationBars.buttons["Retry"].exists)
        XCTAssertFalse(failure.exists)
    }

    func testTodoNativeSelectionAndClipboard() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-1"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        let draft = "Clipboard draft"
        input.tap(); input.typeText(draft)
        func editCommand(_ title: String) -> XCUIElement {
            let button = app.buttons[title].firstMatch
            return button.exists ? button : app.menuItems[title].firstMatch
        }
        func selectAll() {
            // Press trailing whitespace once. Tap-then-press in the text can
            // select a word on iOS18, whose selection menu has no Select All.
            input.coordinate(withNormalizedOffset: CGVector(dx: 0.95, dy: 0.5)).press(forDuration: 1.1)
            let command = editCommand("Select All")
            XCTAssertTrue(command.waitForExistence(timeout: 5)); command.tap()
        }
        selectAll()
        let selected = XCTAttachment(screenshot: app.screenshot()); selected.name = "Native selected text and edit menu"; selected.lifetime = .keepAlways; add(selected)
        let copy = editCommand("Copy")
        XCTAssertTrue(copy.waitForExistence(timeout: 5)); copy.tap()
        // Copy retains the selection; typing must replace that exact range.
        input.typeText("Replace copied text")
        XCTAssertEqual(input.value as? String, "Replace copied text")
        selectAll()
        let pasteCopied = editCommand("Paste")
        XCTAssertTrue(pasteCopied.waitForExistence(timeout: 5)); pasteCopied.tap()
        XCTAssertEqual(input.value as? String, draft)
        selectAll()
        let cut = editCommand("Cut")
        XCTAssertTrue(cut.waitForExistence(timeout: 5)); cut.tap()
        XCTAssertTrue((input.value as? String).map { $0.isEmpty || $0 == input.placeholderValue } ?? true)
        input.press(forDuration: 1.1)
        let paste = editCommand("Paste")
        XCTAssertTrue(paste.waitForExistence(timeout: 5)); paste.tap()
        XCTAssertEqual(input.value as? String, draft)
        selectAll()
        // The first outside tap dismisses UIKit's edit menu without activating
        // the control underneath. Preserve selection, then perform the toggle.
        app.staticTexts["TODO"].tap()
        XCTAssertFalse(editCommand("Copy").exists)
        let todo = app.buttons.matching(NSPredicate(format: "label == %@", "Try Playbit offline")).firstMatch
        let before = todo.value as? String
        todo.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate: NSPredicate(format: "value != %@", before ?? ""), object: todo)], timeout: 10), .completed)
        XCTAssertEqual(input.value as? String, draft)
        XCTAssertTrue(app.keyboards.firstMatch.exists)
        // This must replace the retained selection, rather than append at a
        // reset caret after the unrelated checkbox scene reply.
        input.typeText("Replacement draft")
        XCTAssertEqual(input.value as? String, "Replacement draft")
        let retained = XCTAttachment(screenshot: app.screenshot()); retained.name = "Selection survives checkbox update"; retained.lifetime = .keepAlways; add(retained)
    }

    func testBasicControlsGallery() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let session = sessionRows(app).firstMatch
        XCTAssertTrue(session.waitForExistence(timeout: 40)); session.tap()
        let input = app.textFields["guest-input-30"]
        let empty = app.textFields["guest-input-31"]
        XCTAssertTrue(input.waitForExistence(timeout: 40))
        XCTAssertEqual(input.value as? String, "Text value")
        XCTAssertFalse(app.textFields["guest-input-32"].isEnabled)
        input.tap(); input.typeText(" retained")
        empty.tap(); empty.typeText("Second draft")
        app.scrollViews.firstMatch.swipeUp()
        let check = app.buttons["guest-control-10"]
        XCTAssertTrue(check.waitForExistence(timeout: 5)); check.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Done"), object: check)], timeout: 10), .completed)
        XCTAssertEqual(input.value as? String, "Text value retained")
        XCTAssertEqual(empty.value as? String, "Second draft")
        XCTAssertFalse(app.buttons["guest-control-12"].isEnabled)
        XCTAssertFalse(app.buttons["guest-control-14"].isEnabled)
        for id in [4, 5, 6] { XCTAssertFalse(app.buttons["guest-control-\(id)"].isEnabled) }
        app.buttons["guest-control-2"].tap()
        XCTAssertTrue(app.staticTexts["Primary button activated · 2 actions"].waitForExistence(timeout: 10))
        check.tap()
        XCTAssertEqual(XCTWaiter.wait(for: [XCTNSPredicateExpectation(predicate:
            NSPredicate(format: "value == %@", "Not done"), object: check)], timeout: 10), .completed)
        XCTAssertEqual(input.value as? String, "Text value retained")
        let capture = XCTAttachment(screenshot: app.screenshot())
        capture.name = "Basic gallery native controls and retained drafts"
        capture.lifetime = .keepAlways; add(capture)
    }

    func testMalformedCanvasRetainsPriorSceneAndCanReopen() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify", "--phase1-malformed-scene"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        let canvas = app.otherElements["guest-canvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 40))
        XCTAssertTrue(app.staticTexts["The app scene is invalid. The previous view is retained."].waitForExistence(timeout: 10))
        XCTAssertTrue(canvas.exists)
        XCTAssertTrue(app.staticTexts["Speed 0 · Distance 0"].exists)
        let retained = XCTAttachment(screenshot: app.screenshot())
        retained.name = "Malformed scene error retains actual Micro Kart canvas"
        retained.lifetime = .keepAlways; add(retained)
        app.terminate()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let restored = sessionRows(app).firstMatch
        XCTAssertTrue(restored.waitForExistence(timeout: 40)); restored.tap()
        XCTAssertTrue(canvas.waitForExistence(timeout: 40))
        XCTAssertFalse(app.staticTexts["The app scene is invalid. The previous view is retained."].exists)
        let start = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.8))
        let end = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.8))
        start.press(forDuration: 1.5, thenDragTo: end)
        let changed = NSPredicate(format: "label BEGINSWITH 'Speed ' AND NOT label ENDSWITH 'Distance 0'")
        XCTAssertTrue(app.staticTexts.matching(changed).firstMatch.waitForExistence(timeout: 10))
        let recovered = XCTAttachment(screenshot: app.screenshot())
        recovered.name = "Actual Canvas input works after reopening"
        recovered.lifetime = .keepAlways; add(recovered)
    }

    func testMicroKartCanvasRespondsToTouchAndFrames() {
        let app = XCUIApplication()
        app.launchArguments = ["--phase1-qualify"]
        app.launch()
        let row = sessionRows(app).firstMatch
        XCTAssertTrue(row.waitForExistence(timeout: 40)); row.tap()
        let canvas = app.otherElements["guest-canvas"]
        XCTAssertTrue(canvas.waitForExistence(timeout: 40))
        XCTAssertTrue(app.staticTexts["Speed 0 · Distance 0"].exists)
        let initial = XCTAttachment(screenshot: app.screenshot()); initial.name = "Micro Kart before input"; initial.lifetime = .keepAlways; add(initial)
        let start = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.25, dy: 0.8))
        let end = canvas.coordinate(withNormalizedOffset: CGVector(dx: 0.7, dy: 0.8))
        start.press(forDuration: 1.5, thenDragTo: end)
        let changed = NSPredicate(format: "label BEGINSWITH 'Speed ' AND NOT label ENDSWITH 'Distance 0'")
        XCTAssertTrue(app.staticTexts.matching(changed).firstMatch.waitForExistence(timeout: 10))
        let result = XCTAttachment(screenshot: app.screenshot()); result.name = "Micro Kart after native touch and frames"; result.lifetime = .keepAlways; add(result)
    }

}
