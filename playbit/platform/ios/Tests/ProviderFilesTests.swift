import XCTest
import UIKit
import UniformTypeIdentifiers

/// Opt-in packaged qualification through ordinary sign-in and Apple's Files UI.
/// Credentials are read from the runner's protected container, never arguments.
final class ProviderFilesTests: XCTestCase {
    override func setUp() {
        super.setUp(); continueAfterFailure = false
        XCUIDevice.shared.press(.home)
    }
    private func capture(_ app: XCUIApplication, _ name: String) {
        let attachment = XCTAttachment(screenshot: app.screenshot())
        attachment.name = name; attachment.lifetime = .keepAlways; add(attachment)
    }
    private func location(_ app: XCUIApplication, named name: String) -> XCUIElement {
        let browse = app.tabBars.buttons["Browse"]
        if browse.exists && browse.isHittable { browse.tap() }
        for _ in 0..<8 {
            let locations = app.staticTexts["Locations"].firstMatch
            if locations.exists && locations.isHittable { break }
            let history = app.buttons["DOC.navBarButton.backInHistory"]
            let back = history.exists ? history : app.navigationBars.buttons.element(boundBy: 0)
            guard back.exists, back.isEnabled, back.isHittable,
                  back.label != "Cancel", back.label != "Done" else { break }
            back.tap()
        }
        XCTAssertTrue(app.staticTexts["Locations"].firstMatch.waitForExistence(timeout: 15), "Reach Files locations before selecting an account")
        return app.staticTexts[name].firstMatch
    }
    private func openPersonalVolume(_ app: XCUIApplication, named name: String) {
        let account = location(app, named: "Playbit")
        XCTAssertTrue(account.waitForExistence(timeout: 30), "The account has one Playbit location")
        account.tap()
        let enable = app.alerts.buttons["Turn On"]
        if enable.waitForExistence(timeout: 3) { enable.tap() }
        capture(app, "Playbit account volume folders")
        let folder = app.cells.containing(.staticText, identifier: name).firstMatch
        XCTAssertTrue(folder.waitForExistence(timeout: 30), "The personal volume is a folder inside Playbit")
        folder.tap()
    }

    // Opt-in physical-device measurement of the ordinary installed package.
    // XCTest's launch metric excludes the driver's UI-query and idle waits.
    func testProviderStartupTiming() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_STARTUP_TIMING"] == "1" else {
            throw XCTSkip("Requires an initialized reference-device provider account")
        }
        let app = XCUIApplication()
        app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        app.terminate()
        let options = XCTMeasureOptions()
        options.iterationCount = 10
        measure(metrics: [XCTApplicationLaunchMetric(waitUntilResponsive: true)], options: options) {
            app.launch()
        }
        app.terminate()
    }
    @MainActor func testFreshAccountEnrollmentRegistersAccountDrive() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let email = environment["PLAYBIT_PROVIDER_EMAIL"],
              let drive = environment["PLAYBIT_PROVIDER_DRIVE"] else {
            throw XCTSkip("Requires an isolated live provider account and personal volume")
        }
        let keyFile = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("playbit-live-recovery-key.txt")
        let app = XCUIApplication()
        app.launch()
        if app.textFields["sign-in-email"].waitForExistence(timeout: 3) {
            let bytes = try Data(contentsOf: keyFile)
            try FileManager.default.removeItem(at: keyFile)
            guard bytes.count == 23, let key = String(data: bytes, encoding: .utf8), key.first == "a" else {
                XCTFail("Expected an isolated account key"); return
            }
            let field = app.textFields["sign-in-email"]
            XCTAssertTrue(field.waitForExistence(timeout: 30))
            field.tap()
            field.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: (field.value as? String ?? "").count))
            field.typeText(email)
            let secret = app.secureTextFields["sign-in-account-key"]
            XCTAssertTrue(secret.exists)
            // XCTest logs a typeText argument even for a secure field. Paste
            // through the native menu so credentials never enter its action log.
            let runner = XCUIApplication(bundleIdentifier: Bundle.main.bundleIdentifier!)
            runner.activate()
            XCTAssertTrue(runner.wait(for: .runningForeground, timeout: 10))
            UIPasteboard.general.setItems([[UTType.utf8PlainText.identifier: key]],
                options: [.localOnly: true, .expirationDate: Date().addingTimeInterval(60)])
            defer {
                runner.activate()
                UIPasteboard.general.items = []
                app.activate()
            }
            app.activate()
            XCTAssertTrue(app.wait(for: .runningForeground, timeout: 10))
            secret.tap()
            secret.typeText(String(repeating: XCUIKeyboardKey.delete.rawValue, count: (secret.value as? String ?? "").count))
            secret.press(forDuration: 1)
            let paste = app.buttons["Paste"].exists ? app.buttons["Paste"] : app.menuItems["Paste"]
            XCTAssertTrue(paste.waitForExistence(timeout: 5)); paste.tap()
            app.buttons["sign-in-submit"].tap()
        }
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 180), "Fresh enrollment with provider services available")
        XCTAssertTrue(XCTWaiter.wait(for: [XCTNSPredicateExpectation(
            predicate: NSPredicate(format: "exists == false"), object: app.textFields["sign-in-email"])], timeout: 180) == .completed)
        capture(app, "Fresh enrollment completed")
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        files.activate()
        if files.alerts.buttons["Turn On"].exists { files.alerts.buttons["Turn On"].tap() }
        capture(files, "Files locations after enrollment")
        openPersonalVolume(files, named: drive)
        capture(files, "Personal volume contents inside Playbit")
        if let filename = environment["PLAYBIT_PROVIDER_FILE"] {
            if !files.staticTexts[filename].firstMatch.waitForExistence(timeout: 3) {
                let search = files.searchFields.firstMatch
                if search.exists { search.tap(); search.typeText(filename + "\n") }
            }
            XCTAssertTrue(files.staticTexts[filename].firstMatch.waitForExistence(timeout: 60))
            capture(files, "Located personal file")
        }
    }
    func testInspectFiles() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_INSPECT"] == "1" else {
            throw XCTSkip("Operator-driven Files inspection")
        }
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        files.activate()
        capture(files, "Files current view")
        let text = XCTAttachment(string: files.debugDescription)
        text.name = "Files accessibility"; text.lifetime = .keepAlways; add(text)
    }
    @MainActor func testSignOutRetainsUnavailableAccountDrive() throws {
        guard ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_DRIVE"] != nil else {
            throw XCTSkip("Requires an enrolled isolated account and personal volume")
        }
        let app = XCUIApplication(); app.launch()
        let account = app.buttons["Account"]
        XCTAssertTrue(account.waitForExistence(timeout: 30)); account.tap()
        let identity = app.cells["account-id"]
        XCTAssertTrue(identity.waitForExistence(timeout: 10))
        let accountID = try XCTUnwrap(identity.value as? String)
        XCTAssertTrue(Identifier.valid(accountID))
        let signOut = app.cells["account-sign-out"]
        XCTAssertTrue(signOut.waitForExistence(timeout: 10)); signOut.tap()
        app.alerts["Sign out?"].buttons["Sign Out"].tap()
        XCTAssertTrue(app.textFields["sign-in-email"].waitForExistence(timeout: 30))
        capture(app, "Signed out with provider services available")
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        files.activate()
        XCTAssertTrue(files.wait(for: .runningForeground, timeout: 10))
        let unavailable = location(files, named: "Playbit (Unavailable, " + accountID + ")")
        XCTAssertTrue(unavailable.waitForExistence(timeout: 30), "Sign-out retains the exact account location and its local edits")
        capture(files, "Signed-out account location retained unavailable")
    }
    func testProviderDatabaseReferenceColdAndWarmBeyondFirstPage() throws {
        guard let drive = ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_DRIVE"] else {
            throw XCTSkip("Requires the live 132-database provider fixture")
        }
        let app = XCUIApplication()
        app.launch()
        let files = XCUIApplication(bundleIdentifier: "com.apple.DocumentsApp")
        files.activate()
        XCTAssertTrue(files.wait(for: .runningForeground, timeout: 10))
        if files.alerts.buttons["Turn On"].exists { files.alerts.buttons["Turn On"].tap() }
        else if files.buttons["Cancel"].exists { files.buttons["Cancel"].tap() }
        openPersonalVolume(files, named: drive)
        let reference = files.cells.containing(.staticText, identifier: "Beyond first page.pbdb").firstMatch
        XCTAssertTrue(reference.waitForExistence(timeout: 30))
        for cold in [true, false] {
            if cold { app.terminate() }
            else { files.activate() }
            XCTAssertTrue(files.wait(for: .runningForeground, timeout: 10))
            // Requery the visible hit target after switching applications.
            let reference = files.cells.containing(.staticText, identifier: "Beyond first page.pbdb").firstMatch
            XCTAssertTrue(reference.waitForExistence(timeout: 30))
            XCTAssertTrue(XCTWaiter.wait(for: [XCTNSPredicateExpectation(
                predicate: NSPredicate(format: "hittable == true"), object: reference)], timeout: 10) == .completed)
            reference.tap()
            XCTAssertTrue(app.wait(for: .runningForeground, timeout: 10), "Files must activate the registered database editor")
            // A recovered app can still be initializing this fixture's 132
            // databases. This network wait does not measure cached readiness.
            XCTAssertTrue(app.navigationBars["Beyond first page"].waitForExistence(timeout: cold ? 180 : 30))
            XCTAssertTrue(app.cells.containing(.staticText, identifier: "greeting").firstMatch.waitForExistence(timeout: 10))
            app.navigationBars.buttons.element(boundBy: 0).tap()
            let selected = app.cells.containing(.staticText, identifier: "Beyond first page.pbdb").firstMatch
            XCTAssertTrue(selected.waitForExistence(timeout: 10))
            XCTAssertTrue(selected.isSelected, "The exact database beyond the first browser page is selected")
            capture(app, cold ? "Cold provider reference open beyond first page" : "Warm provider reference open beyond first page")
        }
    }
    @MainActor func testDocumentPickerHydratesAndReplacesExactBytes() throws {
        guard let drive = ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_DRIVE"] else {
            throw XCTSkip("Requires an isolated live provider volume")
        }
        let large = ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_LARGE_FILE"] == "1"
        let window: UIWindow
        if let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene {
            window = UIWindow(windowScene: scene)
        } else { window = UIWindow(frame: UIScreen.main.bounds) }
        window.rootViewController = ProviderPickerHarness()
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        let picker = XCUIApplication(bundleIdentifier: Bundle.main.bundleIdentifier!)
        picker.activate()
        XCTAssertTrue(picker.buttons["Read from Files"].waitForExistence(timeout: 10))
        picker.buttons["Read from Files"].tap()
        let browse = picker.buttons["Browse"].firstMatch
        if browse.waitForExistence(timeout: 5) { browse.tap() }
        openPersonalVolume(picker, named: drive)
        let file = picker.cells.matching(NSPredicate(format: "label BEGINSWITH %@", large ? "large" : "exact")).firstMatch
        // The 132-database fixture places these blob names below the first
        // screen. Files creates its visible collection cells lazily.
        for _ in 0..<32 {
            if file.exists && file.isHittable { break }
            picker.swipeUp()
        }
        XCTAssertTrue(file.waitForExistence(timeout: 180)); file.tap()
        if picker.buttons["Open"].exists { picker.buttons["Open"].tap() }
        let length = large ? 64 * 1024 * 1024 : 1048581
        let hydrated = picker.staticTexts["Exact bytes: \(length)"]
        let finished = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in
            hydrated.exists || picker.alerts.firstMatch.exists
        }, object: nil)
        _ = XCTWaiter.wait(for: [finished], timeout: large ? 180 : 60)
        capture(picker, "Document picker hydration result")
        XCTAssertTrue(hydrated.exists, picker.alerts.firstMatch.debugDescription)
        capture(picker, "Document picker exact-byte hydration")
        picker.buttons["Replace selected file"].tap()
        XCTAssertTrue(picker.staticTexts["Replacement saved"].waitForExistence(timeout: 30))
        capture(picker, "Document picker replacement completed")
    }
    @MainActor func testDocumentPickerOrdinaryFileLifecycle() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let phase = environment["PLAYBIT_PROVIDER_LIFECYCLE"],
              let drive = environment["PLAYBIT_PROVIDER_DRIVE"] else {
            throw XCTSkip("Requires an isolated personal drive and lifecycle phase")
        }
        let app = XCUIApplication(); app.launch()
        XCTAssertTrue(app.buttons["new-session"].waitForExistence(timeout: 30))
        let window = UIWindow(windowScene: try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene))
        window.rootViewController = ProviderPickerHarness(lifecycle: phase)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        let picker = XCUIApplication(bundleIdentifier: Bundle.main.bundleIdentifier!)
        picker.activate(); window.makeKeyAndVisible()
        let read = picker.buttons["Read from Files"]
        XCTAssertTrue(read.waitForExistence(timeout: 10))
        capture(picker, "Folder picker client before selection")
        read.tap()
        if picker.buttons["Browse"].firstMatch.waitForExistence(timeout: 3) { picker.buttons["Browse"].firstMatch.tap() }
        openPersonalVolume(picker, named: drive)
        let action = picker.buttons["DOCPicker.actionButton"]
        let choose = action.exists ? action : picker.buttons["Open"].firstMatch
        XCTAssertTrue(choose.waitForExistence(timeout: 15)); choose.tap()
        let completed = picker.staticTexts["Lifecycle \(phase) completed"].waitForExistence(timeout: 60)
        if !completed {
            capture(picker, "Document lifecycle \(phase) failure")
            let detail = XCTAttachment(string: picker.debugDescription)
            detail.name = "Document lifecycle client state"; detail.lifetime = .keepAlways; add(detail)
        }
        XCTAssertTrue(completed)
        capture(picker, "Coordinated document lifecycle \(phase)")
    }
    @MainActor func testDocumentPickerCopiesDatabaseToAnotherVolume() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let reference = environment["PLAYBIT_PROVIDER_REFERENCE"],
              let volume = environment["PLAYBIT_PROVIDER_DESTINATION"],
              let drive = environment["PLAYBIT_PROVIDER_DRIVE"] else {
            throw XCTSkip("Requires two isolated live personal volumes and a database reference")
        }
        let window = UIWindow(windowScene: try XCTUnwrap(UIApplication.shared.connectedScenes.first as? UIWindowScene))
        window.rootViewController = ProviderPickerHarness(reference: reference, destination: volume)
        window.makeKeyAndVisible()
        defer { window.isHidden = true }
        let picker = XCUIApplication(bundleIdentifier: Bundle.main.bundleIdentifier!)
        picker.activate()
        picker.buttons["Copy database to Files"].tap()
        let browse = picker.buttons["Browse"].firstMatch
        if browse.waitForExistence(timeout: 5) { browse.tap() }
        openPersonalVolume(picker, named: drive)
        let save = picker.navigationBars.buttons["Save"]
        XCTAssertTrue(save.waitForExistence(timeout: 15)); save.tap()
        if picker.alerts.buttons["Replace"].waitForExistence(timeout: 2) { picker.alerts.buttons["Replace"].tap() }
        let verified = picker.staticTexts["Database copy reference verified"].waitForExistence(timeout: 60)
        if !verified {
            capture(picker, "Database copy result")
            let detail = XCTAttachment(string: picker.debugDescription)
            detail.name = "Database copy client state"; detail.lifetime = .keepAlways; add(detail)
        }
        XCTAssertTrue(verified)
        capture(picker, "Cross-volume canonical database copy")
    }
}

/// Runs only in the opt-in UI test runner. This is a normal coordinated document
/// client; it never opens Playbit's profile or accesses its App Group.
private final class ProviderPickerHarness: UIViewController, UIDocumentPickerDelegate, NSFilePresenter {
    private let result = UILabel()
    private var selected: URL?
    private let reference: String?
    private let destination: String?
    private let lifecycle: String?
    private var exporting = false
    private(set) var presentedItemURL: URL?
    let presentedItemOperationQueue: OperationQueue = {
        let queue = OperationQueue(); queue.maxConcurrentOperationCount = 1
        return queue
    }()
    deinit { NSFileCoordinator.removeFilePresenter(self) }
    func presentedItemDidChange() {
        if let url = presentedItemURL { verifyCopy(url) }
    }
    init(reference: String? = nil, destination: String? = nil, lifecycle: String? = nil) {
        self.reference = reference; self.destination = destination; self.lifecycle = lifecycle
        super.init(nibName: nil, bundle: nil)
    }
    required init?(coder: NSCoder) { fatalError("Use the qualification initializer") }
    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground
        let stack = UIStackView(); stack.axis = .vertical; stack.spacing = 24
        stack.translatesAutoresizingMaskIntoConstraints = false
        result.numberOfLines = 0; result.text = "Choose exact.bin inside the personal volume in Playbit."
        let read = UIButton(type: .system); read.setTitle("Read from Files", for: .normal)
        read.addTarget(self, action: #selector(open), for: .touchUpInside)
        let replace = UIButton(type: .system); replace.setTitle("Replace selected file", for: .normal)
        replace.addTarget(self, action: #selector(write), for: .touchUpInside)
        let copy = UIButton(type: .system); copy.setTitle("Copy database to Files", for: .normal)
        copy.addTarget(self, action: #selector(copyDatabase), for: .touchUpInside)
        for control in [read, replace, copy, result] { stack.addArrangedSubview(control) }
        view.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.leadingAnchor, constant: 24),
            stack.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -24),
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 40),
        ])
    }
    @objc private func open() {
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: lifecycle == nil ? [.data] : [.folder], asCopy: false)
        picker.delegate = self; present(picker, animated: true)
    }
    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        guard let url = urls.first else { return }
        if let lifecycle { performLifecycle(url, phase: lifecycle); return }
        if exporting {
            presentedItemURL = url
            NSFileCoordinator.addFilePresenter(self)
            verifyCopy(url); return
        }
        selected = url
        perform(url, writing: false)
    }
    @objc private func copyDatabase() {
        guard let reference, reference.utf8.count <= 62 else { result.text = "Missing database fixture"; return }
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("Copied database \(Int(Date().timeIntervalSince1970)).pbdb")
        do { try Data(reference.utf8).write(to: url, options: .atomic) }
        catch { result.text = "Cannot stage reference"; return }
        exporting = true
        let picker = UIDocumentPickerViewController(forExporting: [url], asCopy: true)
        picker.delegate = self; present(picker, animated: true)
    }
    private func performLifecycle(_ root: URL, phase: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            let scoped = root.startAccessingSecurityScopedResource()
            defer { if scoped { root.stopAccessingSecurityScopedResource() } }
            var coordinationError: NSError?
            var failure: Error?
            NSFileCoordinator().coordinate(writingItemAt: root, options: .forMerging, error: &coordinationError) { coordinated in
                let directory = coordinated.appendingPathComponent("Provider lifecycle", isDirectory: true)
                let original = directory.appendingPathComponent("ordinary.bin")
                let renamed = directory.appendingPathComponent("renamed.bin")
                let copied = directory.appendingPathComponent("copy.bin")
                let moved = coordinated.appendingPathComponent("moved.bin")
                do {
                    let files = FileManager.default
                    switch phase {
                    case "create":
                        try files.createDirectory(at: directory, withIntermediateDirectories: false)
                        try Data("ordinary file café 東京\0".utf8).write(to: original, options: .atomic)
                    case "edit":
                        let bytes = Data((0..<8192).map { UInt8(truncatingIfNeeded: $0) })
                        try bytes.write(to: original, options: .atomic)
                        guard try Data(contentsOf: original) == bytes else { throw CocoaError(.fileReadCorruptFile) }
                    case "rename": try files.moveItem(at: original, to: renamed)
                    case "copy":
                        try files.copyItem(at: renamed, to: copied)
                        guard try Data(contentsOf: copied) == Data(contentsOf: renamed) else { throw CocoaError(.fileReadCorruptFile) }
                    case "move": try files.moveItem(at: copied, to: moved)
                    case "verify":
                        let bytes = Data((0..<8192).map { UInt8(truncatingIfNeeded: $0) })
                        guard try Data(contentsOf: renamed) == bytes,
                              try Data(contentsOf: moved) == bytes else { throw CocoaError(.fileReadCorruptFile) }
                    case "delete":
                        try files.removeItem(at: directory)
                        try files.removeItem(at: moved)
                    default: throw CocoaError(.featureUnsupported)
                    }
                } catch { failure = error }
            }
            let error = coordinationError ?? failure as NSError?
            DispatchQueue.main.async { self.result.text = error.map { "Lifecycle failed: \($0.localizedDescription)" } ?? "Lifecycle \(phase) completed" }
        }
    }
    private func verifyCopy(_ url: URL) {
        DispatchQueue.global(qos: .userInitiated).async {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            var error: NSError?
            var message = "Copied reference unavailable"
            NSFileCoordinator().coordinate(readingItemAt: url, options: .withoutChanges, error: &error) { source in
                do {
                    let bytes = try Data(contentsOf: source)
                    let text = String(data: bytes, encoding: .utf8) ?? ""
                    let parts = text.dropLast().split(separator: ":", omittingEmptySubsequences: false)
                    if bytes.count <= 62, text.hasSuffix("\n"), text != self.reference,
                       parts.count == 4, parts[0] == "playbit", parts[1] == "database",
                       String(parts[2]) == self.destination, Identifier.valid(String(parts[3]), bytes: 15) {
                        message = "Database copy reference verified"
                    } else {
                        message = "Copied reference identity mismatch"
                        let report: [String: Any] = ["bytes": Array(bytes), "destination": self.destination ?? "", "sameAsSource": text == self.reference]
                        if let data = try? JSONSerialization.data(withJSONObject: report),
                           let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first {
                            try? data.write(to: documents.appendingPathComponent("provider-copy-readback.json"), options: .atomic)
                        }
                    }
                } catch { message = "Copied reference read failed: \(error.localizedDescription)" }
            }
            if let error { message = error.localizedDescription }
            DispatchQueue.main.async { self.result.text = message }
        }
    }
    @objc private func write() {
        guard let selected else { return }
        perform(selected, writing: true)
    }
    private func perform(_ url: URL, writing: Bool) {
        result.text = writing ? "Saving replacement…" : "Hydrating…"
        DispatchQueue.global(qos: .userInitiated).async {
            let scoped = url.startAccessingSecurityScopedResource()
            defer { if scoped { url.stopAccessingSecurityScopedResource() } }
            let coordinator = NSFileCoordinator()
            var coordinationError: NSError?
            var message = "Coordination failed"
            if writing {
                coordinator.coordinate(writingItemAt: url, options: .forReplacing, error: &coordinationError) { destination in
                    do {
                        var bytes = Data((0..<65536).map { UInt8(truncatingIfNeeded: $0) })
                        bytes.append(Data("Device replacement — café 東京\0".utf8))
                        try bytes.write(to: destination, options: .atomic)
                        message = "Replacement saved"
                    } catch { message = "Write failed: \(error.localizedDescription)" }
                }
            } else {
                coordinator.coordinate(readingItemAt: url, options: .withoutChanges, error: &coordinationError) { source in
                    do {
                        let large = ProcessInfo.processInfo.environment["PLAYBIT_PROVIDER_LARGE_FILE"] == "1"
                        let length = large ? 64 * 1024 * 1024 : 1048581
                        let pattern = Data((0..<(2 * 65536)).map { UInt8(truncatingIfNeeded: $0) })
                        let tail = Data([0, 101, 110, 100, 255])
                        let handle = try FileHandle(forReadingFrom: source)
                        defer { try? handle.close() }
                        var count = 0
                        var matches = true
                        while count < length {
                            let boundary = !large && count < 1048576 ? 1048576 : length
                            let bytes = try handle.read(upToCount: min(65536, boundary - count)) ?? Data()
                            if bytes.isEmpty { matches = false; break }
                            let start = !large && count >= 1048576 ? count - 1048576 : count % 65536
                            let expected = (!large && count >= 1048576 ? tail : pattern)
                                .subdata(in: start..<(start + bytes.count))
                            count += bytes.count
                            if bytes != expected { matches = false; break }
                        }
                        if let extra = try handle.read(upToCount: 1), !extra.isEmpty { matches = false }
                        message = matches && count == length ? "Exact bytes: \(count)" : "Byte mismatch after \(count) bytes"
                    } catch { message = "Read failed: \(error.localizedDescription)" }
                }
            }
            if let coordinationError { message = coordinationError.localizedDescription }
            DispatchQueue.main.async { self.result.text = message }
        }
    }
}
