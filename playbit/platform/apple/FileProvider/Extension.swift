import Foundation
import FileProvider
import CryptoKit

@objc(FileProviderExtension)
final class FileProviderExtension: NSObject, NSFileProviderReplicatedExtension {
    let domain: NSFileProviderDomain
    let client: ProviderClient
    required init(domain: NSFileProviderDomain) {
        self.domain = domain
        #if os(macOS)
        self.client = ProviderClient(account: domain.identifier.rawValue,
            profile: domain.userInfo?["profile"] as? String ?? "")
        #else
        self.client = ProviderClient(account: domain.identifier.rawValue)
        #endif
        super.init()
        #if os(iOS)
        ProviderPush.start()
        #endif
    }
    func invalidate() {
        client.invalidate()
    }
    private func upload(_ value: [String: Any]) -> ((Progress) -> Void)? {
        guard let item = value["item"] as? [String: Any], item["uploaded"] as? Bool != true,
              let entry = item["id"] as? String else { return nil }
        return { progress in
            self.client.uploads.track(item, progress: progress) { verify in
                guard let fields = try? ProviderNode(NSFileProviderItemIdentifier(entry)).itemRequest() else {
                    verify(nil); return
                }
                self.client.call(fields) { result in
                    if case .success(let value) = result { verify(value["item"] as? [String: Any]) }
                    else { verify(nil) }
                }
            }
        }
    }
    func enumerator(for containerItemIdentifier: NSFileProviderItemIdentifier,
        request: NSFileProviderRequest) throws -> NSFileProviderEnumerator {
        ProviderEnumerator(client: client, domain: domain, container: containerItemIdentifier)
    }
    @discardableResult
    func item(for identifier: NSFileProviderItemIdentifier, request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, Error?) -> Void) -> Progress {
        let operation = ProviderOperation<(NSFileProviderItem?, Error?)>(cancelled: (nil, CocoaError(.userCancelled))) {
            completionHandler($0.0, $0.1)
        }
        do {
            let node = try ProviderNode(identifier)
            if case .volume(let volume) = node {
                let enumerator = ProviderEnumerator(client: client, domain: domain, container: .rootContainer)
                enumerator.catalog { result in
                    do {
                        guard let bytes = try result.get()[volume] else { throw NSFileProviderError(.noSuchItem) }
                        operation.finish((ProviderItem(try enumerator.folder(bytes)), nil))
                    } catch { operation.finish((nil, error)) }
                }
                return operation.progress
            }
            client.call(node.itemRequest()) { result in
                switch result {
                case .success(let value): operation.finish(((value["item"] as? [String: Any]).map(ProviderItem.init), nil))
                case .failure(let error): operation.finish((nil, error))
                }
            }
        } catch { operation.finish((nil, error)) }
        return operation.progress
    }
    @discardableResult
    func fetchContents(for itemIdentifier: NSFileProviderItemIdentifier, version requestedVersion: NSFileProviderItemVersion?,
        request: NSFileProviderRequest,
        completionHandler: @escaping (URL?, NSFileProviderItem?, Error?) -> Void) -> Progress {
        let operation = ProviderOperation<(URL?, NSFileProviderItem?, Error?)>(cancelled: (nil, nil, CocoaError(.userCancelled))) {
            completionHandler($0.0, $0.1, $0.2)
        }
        item(for: itemIdentifier, request: request) { item, error in
            guard !operation.progress.isCancelled else {
                operation.finish((nil, nil, CocoaError(.userCancelled)))
                return
            }
            guard let item else { operation.finish((nil, nil, error)); return }
            do {
                let directory = try NSFileProviderManager(for: self.domain)!.temporaryDirectoryURL()
                let path = directory.appendingPathComponent(UUID().uuidString)
                let version = requestedVersion ?? item.itemVersion!
                var fields = try ProviderNode(itemIdentifier).entryRequest("fetch")
                fields["version"] = Array(version.contentVersion)
                fields["path"] = path.path
                self.client.call(fields) { result in
                    switch result {
                    case .success(let value):
                        if !operation.finish((path, (value["item"] as? [String: Any]).map(ProviderItem.init), nil)) {
                            try? FileManager.default.removeItem(at: path)
                        }
                    case .failure(let error):
                        try? FileManager.default.removeItem(at: path)
                        operation.finish((nil, nil, error))
                    }
                }
            } catch { operation.finish((nil, nil, error)) }
        }
        return operation.progress
    }
    @discardableResult
    func createItem(basedOn itemTemplate: NSFileProviderItem, fields: NSFileProviderItemFields,
        contents url: URL?, options: NSFileProviderCreateItemOptions, request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let operation = ProviderOperation<(NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?)>(
            cancelled: (nil, [], false, CocoaError(.userCancelled))) {
            completionHandler($0.0, $0.1, $0.2, $0.3)
        }
        // A dataless reimport is not a new empty file. Without a verified match,
        // Apple's create contract requires refusing the placeholder with nil.
        if options.contains(.mayAlreadyExist) && url == nil && itemTemplate.contentType != .folder {
            operation.finish((nil, [], false, nil))
            return operation.progress
        }
        let fields: [String: Any]
        do {
            let parent = try ProviderNode(itemTemplate.parentItemIdentifier)
            let identity = domain.identifier.rawValue + ":" + (parent.volume ?? "") + ":" + itemTemplate.itemIdentifier.rawValue
            let digest = Array(SHA256.hash(data: Data(identity.utf8)))
            if parent == .root {
                guard itemTemplate.contentType == .folder, url == nil else {
                    throw NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.cannotSynchronize.rawValue,
                        userInfo: [NSLocalizedDescriptionKey: "Choose or create a volume folder inside Playbit to save files."])
                }
                let volume = Identifier.encode(Array(digest.prefix(16)))
                try ProviderEnumerator(client: client, domain: domain, container: .rootContainer).reserveVolume(volume, name: itemTemplate.filename)
                fields = ["op": "create_volume", "id": volume, "name": itemTemplate.filename]
            } else {
                var create = try parent.parentFields()
                create["op"] = "create"
                create["id"] = Identifier.encode(Array(digest.prefix(15)))
                create["name"] = itemTemplate.filename
                create["directory"] = itemTemplate.contentType == .folder
                create["path"] = url?.path
                fields = create
            }
        } catch { operation.finish((nil, [], false, error)); return operation.progress }
        client.call(fields) { result in
            switch result {
            case .success(let value):
                let item = (value["item"] as? [String: Any]).map(ProviderItem.init)
                // Copying a reference creates a new database identity. Fetch its
                // new reference bytes before the system considers this copy current.
                operation.finish((item, [], item?.database == true, nil), upload: self.upload(value))
            case .failure(let error): operation.finish((nil, [], false, error))
            }
        }
        return operation.progress
    }
    @discardableResult
    func modifyItem(_ item: NSFileProviderItem, baseVersion version: NSFileProviderItemVersion,
        changedFields: NSFileProviderItemFields, contents newContents: URL?,
        options: NSFileProviderModifyItemOptions, request: NSFileProviderRequest,
        completionHandler: @escaping (NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?) -> Void) -> Progress {
        let operation = ProviderOperation<(NSFileProviderItem?, NSFileProviderItemFields, Bool, Error?)>(
            cancelled: (nil, [], false, CocoaError(.userCancelled))) {
            completionHandler($0.0, $0.1, $0.2, $0.3)
        }
        let fields: [String: Any]
        do {
            let node = try ProviderNode(item.itemIdentifier)
            let parent = try ProviderNode(item.parentItemIdentifier)
            if case .volume(let volume) = node {
                guard parent == .root, newContents == nil else { throw NSFileProviderError(.cannotSynchronize) }
                fields = ["op": "rename_volume", "volume": volume, "name": item.filename,
                    "metadata": Array(version.metadataVersion)]
            } else {
                guard node.volume == parent.volume, node.volume != nil else {
                    throw NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.cannotSynchronize.rawValue,
                        userInfo: [NSLocalizedDescriptionKey: "Moving between Playbit volumes is not supported. Copy the item instead."])
                }
                var modify = try node.entryRequest("modify")
                modify.merge(try parent.parentFields()) { _, new in new }
                modify["name"] = item.filename
                modify["version"] = Array(version.contentVersion)
                modify["metadata"] = Array(version.metadataVersion)
                modify["path"] = newContents?.path
                fields = modify
            }
        } catch { operation.finish((nil, [], false, error)); return operation.progress }
        client.call(fields) { result in
            switch result {
            case .success(let value): operation.finish(((value["item"] as? [String: Any]).map(ProviderItem.init), [], false, nil), upload: self.upload(value))
            case .failure(let error): operation.finish((nil, [], false, error))
            }
        }
        return operation.progress
    }
    @discardableResult
    func deleteItem(identifier: NSFileProviderItemIdentifier, baseVersion version: NSFileProviderItemVersion,
        options: NSFileProviderDeleteItemOptions, request: NSFileProviderRequest,
        completionHandler: @escaping (Error?) -> Void) -> Progress {
        let operation = ProviderOperation<Error?>(cancelled: CocoaError(.userCancelled), completion: completionHandler)
        let fields: [String: Any]
        do {
            var remove = try ProviderNode(identifier).entryRequest("delete")
            remove["version"] = Array(version.contentVersion)
            remove["metadata"] = Array(version.metadataVersion)
            remove["recursive"] = options.contains(.recursive)
            fields = remove
        } catch {
            operation.finish(NSError(domain: NSFileProviderErrorDomain, code: NSFileProviderError.deletionRejected.rawValue,
                userInfo: [NSLocalizedDescriptionKey: "Delete whole volumes in Playbit."]))
            return operation.progress
        }
        client.call(fields) { result in
            switch result {
            case .success: operation.finish(nil)
            case .failure(let error):
                let failure = error as NSError
                if failure.domain == NSCocoaErrorDomain && [CocoaError.fileWriteFileExists.rawValue,
                    CocoaError.fileWriteNoPermission.rawValue, CocoaError.fileReadNoPermission.rawValue].contains(failure.code) {
                    self.item(for: identifier, request: request) { current, _ in
                        guard let current else { operation.finish(NSFileProviderError(.deletionRejected)); return }
                        operation.finish(NSError.fileProviderErrorForRejectedDeletion(of: current))
                    }
                } else if failure.domain == NSFileProviderErrorDomain && failure.code == NSFileProviderError.noSuchItem.rawValue {
                    operation.finish(nil)
                } else { operation.finish(error) }
            }
        }
        return operation.progress
    }
}
