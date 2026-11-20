import Foundation
import FileProvider
import UniformTypeIdentifiers

final class ProviderItem: NSObject, NSFileProviderItem {
    let value: [String: Any]
    init(_ value: [String: Any]) { self.value = value }
    var itemIdentifier: NSFileProviderItemIdentifier {
        NSFileProviderItemIdentifier(value["id"] as? String ?? "")
    }
    var parentItemIdentifier: NSFileProviderItemIdentifier {
        (value["parent"] as? String).map { NSFileProviderItemIdentifier($0) } ?? .rootContainer
    }
    var filename: String { value["filename"] as? String ?? "Playbit" }
    var directory: Bool { value["kind"] as? String == "directory" }
    var database: Bool {
        let kind = value["kind"] as? String
        return kind == "kv_db" || kind == "object_db"
    }
    var contentType: UTType { directory ? .folder : database ? UTType(importedAs: "app.playbit.database") :
        UTType(filenameExtension: (filename as NSString).pathExtension) ?? .data }
    var documentSize: NSNumber? { value["len_bytes"] as? NSNumber }
    var isUploaded: Bool { directory && itemIdentifier == .rootContainer || value["uploaded"] as? Bool == true }
    var itemVersion: NSFileProviderItemVersion {
        NSFileProviderItemVersion(contentVersion: Data(value["content_version"] as? [UInt8] ?? []),
            metadataVersion: Data(value["metadata_version"] as? [UInt8] ?? []))
    }
    var capabilities: NSFileProviderItemCapabilities {
        if value["unavailable"] as? Bool == true { return [.allowsContentEnumerating, .allowsReading] }
        if value["role"] as? String == "root" { return [.allowsContentEnumerating, .allowsReading, .allowsAddingSubItems] }
        if value["role"] as? String == "volume" {
            var result: NSFileProviderItemCapabilities = [.allowsContentEnumerating, .allowsReading]
            if value["writable"] as? Bool == true { result.formUnion([.allowsRenaming, .allowsAddingSubItems]) }
            return result
        }
        var result: NSFileProviderItemCapabilities = directory ? [.allowsContentEnumerating, .allowsReading] : [.allowsReading]
        if value["writable"] as? Bool == true {
            result.formUnion([.allowsRenaming, .allowsReparenting, .allowsDeleting])
            if directory { result.insert(.allowsAddingSubItems) }
            else if !database { result.insert(.allowsWriting) }
        }
        return result
    }
}

/// Apple item identities distinguish the account root, volume folders and entries.
/// Only decoded identities reach the existing per-volume operation boundary.
enum ProviderNode: Equatable {
    case root
    case volume(String)
    case entry(volume: String, entry: String)

    init(_ identifier: NSFileProviderItemIdentifier) throws {
        if identifier == .rootContainer { self = .root; return }
        let text = identifier.rawValue
        if Identifier.valid(text) { self = .volume(text); return }
        guard let bytes = Identifier.decode(text, bytes: 31) else { throw NSFileProviderError(.noSuchItem) }
        self = .entry(volume: Identifier.encode(Array(bytes.prefix(16))), entry: Identifier.encode(Array(bytes.suffix(15))))
    }
    var volume: String? {
        switch self {
        case .root: return nil
        case .volume(let volume), .entry(let volume, _): return volume
        }
    }
    func itemRequest() -> [String: Any] {
        switch self {
        case .root: return ["op": "root"]
        case .volume(let volume): return ["op": "volume_item", "volume": volume]
        case .entry(let volume, let entry): return ["op": "item", "volume": volume, "entry": entry]
        }
    }
    func entryRequest(_ op: String) throws -> [String: Any] {
        guard case .entry(let volume, let entry) = self else { throw CocoaError(.fileWriteNoPermission) }
        return ["op": op, "volume": volume, "entry": entry]
    }
    func parentFields() throws -> [String: Any] {
        switch self {
        case .root: throw CocoaError(.fileWriteNoPermission)
        case .volume(let volume): return ["volume": volume, "parent": NSNull()]
        case .entry(let volume, let entry): return ["volume": volume, "parent": entry]
        }
    }
}
