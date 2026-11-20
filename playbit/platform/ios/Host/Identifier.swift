import Foundation

/// Opaque identifiers use canonical Bitcoin base58 at every text boundary.
/// Native bridge arrays carry bytes, independent of the displayed text length.
enum Identifier {
    private static let alphabet = Array("123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz".utf8)
    private static let digits: [Int16] = {
        var values = [Int16](repeating: -1, count: 128)
        for (index, letter) in alphabet.enumerated() { values[Int(letter)] = Int16(index) }
        return values
    }()
    static func encode(_ bytes: [UInt8]) -> String {
        precondition(!bytes.isEmpty && bytes.count <= 32)
        var digits: [Int] = []
        for byte in bytes {
            var carry = Int(byte)
            for index in digits.indices {
                carry += digits[index] * 256
                digits[index] = carry % 58; carry /= 58
            }
            while carry != 0 { digits.append(carry % 58); carry /= 58 }
        }
        let zeros = bytes.prefix { $0 == 0 }.count
        return String(decoding: [UInt8](repeating: 49, count: zeros) + digits.reversed().map { alphabet[$0] }, as: UTF8.self)
    }
    static func decode(_ text: String, bytes width: Int) -> [UInt8]? {
        guard [15, 16, 31, 32].contains(width), !text.isEmpty, text.utf8.count <= 44 else { return nil }
        let leadingZeros = text.utf8.prefix { $0 == 49 }.count
        guard leadingZeros <= width else { return nil }
        var bytes = [UInt8](repeating: 0, count: width)
        // Leading zero digits cannot affect the accumulator.
        for letter in text.utf8.dropFirst(leadingZeros) {
            guard letter < 128, digits[Int(letter)] >= 0 else { return nil }
            var carry = Int(digits[Int(letter)])
            for index in bytes.indices.reversed() {
                carry += Int(bytes[index]) * 58
                bytes[index] = UInt8(carry & 255); carry >>= 8
            }
            guard carry == 0 else { return nil }
        }
        let zeros = bytes.prefix { $0 == 0 }.count
        return leadingZeros == zeros ? bytes : nil
    }
    static func valid(_ text: String, bytes: Int = 16) -> Bool {
        decode(text, bytes: bytes) != nil
    }
}
