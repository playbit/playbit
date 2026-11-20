import Foundation

/// Bundled through local status, never a remote catalog or an automatic join.
struct CuratedExample {
    let id: String
    let name: String
    let description: String
    let invitation: String
    let source: URL

    enum Failure: LocalizedError {
        case invalid
        var errorDescription: String? { "Examples are unavailable. You can still open an invitation or a saved session." }
    }
    static func parse(_ value: Any?) throws -> [CuratedExample] {
        guard let entries = value as? [[String: Any]], entries.count <= 4 else { throw Failure.invalid }
        var seen = Set<String>()
        var total = 0
        return try entries.map { entry in
            guard let id = entry["id"] as? String, !id.isEmpty, id.utf8.count <= 32, seen.insert(id).inserted,
                  let name = entry["name"] as? String, !name.isEmpty, name.utf8.count <= 64,
                  let description = entry["description"] as? String, !description.isEmpty, description.utf8.count <= 512,
                  let invitation = entry["invitation"] as? String, invitation.utf8.count <= 128,
                  invitation.hasPrefix("https://playbit.app/s#"),
                  let sourceText = entry["source"] as? String, sourceText.utf8.count <= 512,
                  let source = URL(string: sourceText), source.scheme == "https", source.host != nil,
                  source.user == nil, source.password == nil else { throw Failure.invalid }
            let code = invitation.dropFirst("https://playbit.app/s#".count)
            guard code.utf8.count == 22,
                  code.utf8.allSatisfy({ "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".utf8.contains($0) })
            else { throw Failure.invalid }
            total += id.utf8.count + name.utf8.count + description.utf8.count + invitation.utf8.count + sourceText.utf8.count
            guard total <= 4096 else { throw Failure.invalid }
            return CuratedExample(id: id, name: name, description: description, invitation: invitation, source: source)
        }
    }
}
