import Foundation

@main enum IdentifierTests {
    static func main() throws {
        // All shells share the same expected text, including every leading zero count.
        let path = CommandLine.arguments.dropFirst().first ?? "playbit/tests/fixtures/identifiers.json"
        let vectors = try JSONSerialization.jsonObject(with: Data(contentsOf: URL(fileURLWithPath: path))) as! [[String: Any]]
        for vector in vectors {
            let bytes = (vector["bytes"] as! [NSNumber]).map { $0.uint8Value }
            let text = vector["text"] as! String
            precondition(Identifier.encode(bytes) == text)
            precondition(Identifier.decode(text, bytes: bytes.count) == bytes)
            precondition(Identifier.decode("1" + text, bytes: bytes.count) == nil)
        }
        for text in ["", "0", "O", "I", "l", "é", String(repeating: "1", count: 17), String(repeating: "z", count: 22)] {
            precondition(!Identifier.valid(text))
        }
        print("IdentifierTests: shared canonical base58 vectors and invalid widths passed")
    }
}
