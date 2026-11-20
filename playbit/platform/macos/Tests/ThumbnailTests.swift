// Geometry and ownership checks run against the real extension classes.
import Foundation
import CoreGraphics

@main
struct ThumbnailTests {
    static func main() throws {
        for (points, scale, side) in [(16.0, 2.0, 128), (128.0, 2.0, 256), (512.0, 2.0, 512)] {
            let geometry = ThumbnailGeometry(maximum: CGSize(width: points, height: points),
                minimum: .zero, scale: scale)!
            precondition(geometry.side == side)
            precondition(geometry.size.width * scale <= 512)
        }
        precondition(ThumbnailGeometry(maximum: .zero, minimum: .zero, scale: 1) == nil)
        precondition(ThumbnailGeometry(maximum: CGSize(width: 512, height: 512),
            minimum: CGSize(width: 300, height: 300), scale: 2) == nil)
        precondition(ThumbnailGeometry(maximum: CGSize(width: CGFloat.infinity, height: 16),
            minimum: .zero, scale: 1) == nil)
        let admission = ThumbnailAdmission()
        var credits = (0..<ThumbnailAdmission.limit).map { _ in admission.acquire()! }
        precondition(admission.acquire() == nil)
        credits.removeLast()
        var retained: (() -> Void)?
        do {
            let credit = admission.acquire()!
            retained = { withExtendedLifetime(credit) {} }
        }
        precondition(admission.acquire() == nil, "reply must retain admission until released")
        retained?()
        retained = nil
        precondition(admission.acquire() != nil)
        credits.removeAll()
        // Quick Look may supply physical pixels with either an identity or
        // scaled CTM. Both must fill every corner of the thumbnail canvas.
        let provider = CGDataProvider(data: Data([255, 0, 0, 255]) as CFData)!
        let color = CGColorSpace(name: CGColorSpace.sRGB)!
        let image = CGImage(width: 1, height: 1, bitsPerComponent: 8, bitsPerPixel: 32,
            bytesPerRow: 4, space: color,
            bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
            provider: provider, decode: nil, shouldInterpolate: false, intent: .defaultIntent)!
        for scale in [1.0, 2.0] {
            let context = CGContext(data: nil, width: 256, height: 256, bitsPerComponent: 8,
                bytesPerRow: 1024, space: color,
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
            context.scaleBy(x: scale, y: scale)
            ThumbnailProvider.draw(image, in: context)
            let bytes = context.data!.assumingMemoryBound(to: UInt8.self)
            for offset in [0, 255 * 4, 255 * 1024, 256 * 256 * 4 - 4] {
                precondition(bytes[offset] == 255 && bytes[offset + 3] == 255)
            }
        }
        print("Thumbnail geometry and admission passed")
    }
}
