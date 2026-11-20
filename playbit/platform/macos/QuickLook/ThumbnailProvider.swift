// Finder owns scheduling and caching. One serial background lane reads artwork;
// credits remain held until Quick Look releases each reply's immutable pixels.
import CoreGraphics
import Foundation
import QuickLookThumbnailing
import os

final class ThumbnailAdmission {
    static let shared = ThumbnailAdmission()
    private let lock = NSLock()
    private var count = 0
    static let limit = 16

    func acquire() -> ThumbnailCredit? {
        lock.lock()
        defer { lock.unlock() }
        guard count < Self.limit else { return nil }
        count += 1
        return ThumbnailCredit(self)
    }

    fileprivate func release() {
        lock.lock()
        defer { lock.unlock() }
        precondition(count > 0)
        count -= 1
    }
}

final class ThumbnailCredit {
    private let admission: ThumbnailAdmission
    init(_ admission: ThumbnailAdmission) { self.admission = admission }
    deinit { admission.release() }
}

struct ThumbnailGeometry {
    let size: CGSize
    let side: UInt32

    init?(maximum: CGSize, minimum: CGSize, scale: CGFloat) {
        guard scale.isFinite, scale > 0,
              maximum.width.isFinite, maximum.height.isFinite,
              minimum.width.isFinite, minimum.height.isFinite,
              maximum.width > 0, maximum.height > 0,
              minimum.width >= 0, minimum.height >= 0 else { return nil }
        let points = min(maximum.width, maximum.height, 512 / scale)
        guard points >= max(minimum.width, minimum.height) else { return nil }
        size = CGSize(width: points, height: points)
        let physical = points * scale
        side = physical <= 128 ? 128 : physical <= 256 ? 256 : 512
    }
}

@objc(PlaybitThumbnailProvider)
final class ThumbnailProvider: QLThumbnailProvider {
    private static let lane = DispatchQueue(label: "app.playbit.thumbnail", qos: .userInitiated)
    private static let logger = Logger(subsystem: "app.playbit.thumbnail", category: "render")

    override func provideThumbnail(for request: QLFileThumbnailRequest,
        _ handler: @escaping (QLThumbnailReply?, Error?) -> Void) {
        guard let credit = ThumbnailAdmission.shared.acquire() else {
            handler(nil, Self.failure(1))
            return
        }
        guard let geometry = ThumbnailGeometry(maximum: request.maximumSize,
            minimum: request.minimumSize, scale: request.scale) else {
            handler(nil, Self.failure(2))
            return
        }
        // Snapshot the system preference per request. Finder controls when its
        // cached thumbnails are regenerated; QLFileThumbnailRequest has no theme.
        let dark = UserDefaults.standard.string(forKey: "AppleInterfaceStyle") == "Dark"
        Self.lane.async {
            autoreleasepool {
                do {
                    let image = try Self.image(url: request.fileURL, side: geometry.side, dark: dark)
                    let reply = QLThumbnailReply(contextSize: geometry.size) { context in
                        withExtendedLifetime(credit) {
                            Self.draw(image, in: context)
                        }
                        return true
                    }
                    handler(reply, nil)
                } catch {
                    // Error codes contain neither app artwork nor private file paths.
                    Self.logger.debug("Thumbnail unavailable: \((error as NSError).code)")
                    handler(nil, error)
                }
            }
        }
    }

    static func draw(_ image: CGImage, in context: CGContext) {
        // Quick Look's bitmap is scaled for Retina, but its Core Graphics CTM
        // need not use points. Cover the complete device canvas in either case.
        let pixels = CGRect(x: 0, y: 0, width: context.width, height: context.height)
        context.draw(image, in: context.convertToUserSpace(pixels))
    }

    static func image(url: URL, side: UInt32, dark: Bool) throws -> CGImage {
        let descriptor = url.withUnsafeFileSystemRepresentation { path in
            path.map { open($0, O_RDONLY | O_CLOEXEC | O_NONBLOCK | O_NOFOLLOW) } ?? -1
        }
        guard descriptor >= 0 else { throw failure(3) }
        defer { close(descriptor) }
        var bytes = Data(count: Int(side * side * 4))
        let result = bytes.withUnsafeMutableBytes { buffer in
            pb_thumbnail_render(descriptor, side, dark ? 1 : 0,
                buffer.baseAddress?.assumingMemoryBound(to: UInt8.self), buffer.count)
        }
        guard result == 0 else { throw failure(result == 1 ? 4 : 5) }
        guard let provider = CGDataProvider(data: bytes as CFData),
              let image = CGImage(width: Int(side), height: Int(side), bitsPerComponent: 8,
                bitsPerPixel: 32, bytesPerRow: Int(side) * 4,
                space: CGColorSpace(name: CGColorSpace.sRGB)!,
                bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { throw failure(6) }
        return image
    }

    private static func failure(_ code: Int) -> NSError {
        NSError(domain: "app.playbit.thumbnail", code: code)
    }
}
