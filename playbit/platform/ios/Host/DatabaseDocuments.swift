import Foundation
import UIKit
import Darwin

enum DatabaseDocuments {
    private static let reader = DispatchQueue(label: "app.playbit.database-document")
    private static var reading = false // Main-thread admission: at most one coordinated read.
    private static var generation: UInt64 = 0

    static func cancelPendingOpen() { generation &+= 1 }

    static func open(_ url: URL, navigation: UINavigationController) {
        cancelPendingOpen()
        let intent = generation
        if navigation.viewControllers.contains(where: { ($0 as? DatabaseDraft)?.hasDatabaseDraft == true }) {
            let alert = UIAlertController(title: "Unsaved database changes", message: "Save or cancel your changes before opening another database.", preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "OK", style: .default))
            navigation.present(alert, animated: true)
            return
        }
        guard !reading else {
            let alert = UIAlertController(title: "Opening database", message: "Wait for the current reference to finish opening, then open this file again.", preferredStyle: .alert)
            alert.addAction(UIAlertAction(title: "OK", style: .default))
            navigation.present(alert, animated: true)
            return
        }
        reading = true
        EngineHost.shared.resume()
        reader.async { [weak navigation] in
            let result: Result<String, Error> = Result {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                var coordinationError: NSError?
                var result: Result<String, Error>?
                NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { coordinated in
                    result = Result {
                        let descriptor = coordinated.withUnsafeFileSystemRepresentation { path in
                            path.map { Darwin.open($0, O_RDONLY | O_CLOEXEC | O_NONBLOCK) } ?? -1
                        }
                        guard descriptor >= 0 else { throw NSError(domain: NSPOSIXErrorDomain, code: Int(errno)) }
                        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
                        defer { try? handle.close() }
                        let count = "playbit:database:".utf8.count + 22 + 1 + 21 + 1
                        var metadata = stat()
                        guard fstat(descriptor, &metadata) == 0,
                              metadata.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG), metadata.st_size >= 0, metadata.st_size <= off_t(count) else {
                            throw CocoaError(.fileReadCorruptFile)
                        }
                        let data = try handle.read(upToCount: count + 1) ?? Data()
                        guard data.count == Int(metadata.st_size), let text = String(data: data, encoding: .utf8) else {
                            throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "This is not a valid Playbit database reference."])
                        }
                        return text
                    }
                }
                if let coordinationError { throw coordinationError }
                guard let result else { throw CocoaError(.fileReadUnknown) }
                return try result.get()
            }
            DispatchQueue.main.async {
                reading = false
                guard generation == intent, let navigation, navigation.viewIfLoaded?.window != nil else { return }
                // Downloading may outlive navigation to an editor. Recheck the
                // draft boundary before the completion changes the visible stack.
                guard !navigation.viewControllers.contains(where: { ($0 as? DatabaseDraft)?.hasDatabaseDraft == true }) else {
                    let alert = UIAlertController(title: "Unsaved database changes", message: "Save or cancel your changes, then open this file again.", preferredStyle: .alert)
                    alert.addAction(UIAlertAction(title: "OK", style: .default))
                    navigation.present(alert, animated: true)
                    return
                }
                switch result {
                case .success(let reference):
                    let database = DatabaseController(reference: reference)
                    database.opened = { [weak navigation, weak database] volume, name, entry, parent in
                        guard let navigation, let database else { return }
                        let browser = VolumeBrowserController(volume: volume, parent: parent, name: name)
                        browser.selectedDatabase = entry
                        browser.seek(to: entry)
                        let sessions = navigation.viewControllers.first(where: { $0 is SessionsController }) ?? SessionsController()
                        navigation.setViewControllers([sessions, browser, database], animated: false)
                    }
                    if navigation.topViewController is DatabaseController {
                        navigation.popViewController(animated: false)
                    }
                    navigation.pushViewController(database, animated: false)
                case .failure(let error):
                    let alert = UIAlertController(title: "Couldn’t open database", message: error.localizedDescription, preferredStyle: .alert)
                    alert.addAction(UIAlertAction(title: "OK", style: .default))
                    navigation.present(alert, animated: true)
                }
            }
        }
    }
}
