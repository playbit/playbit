import Foundation

/// Main-thread-owned admission/cache. Completion is acknowledged before the
/// next decode starts, so a blocked main thread cannot accumulate pixel buffers.
struct SessionArtworkState<Pixel> {
    enum Outcome { case ready(Pixel?), failed }
    private struct Entry { var outcome: Outcome?; let source: UInt64 }
    private var entries: [String: Entry] = [:]
    private var order: [String] = []
    private(set) var active: String?
    private var activeWanted = false
    var count: Int { entries.count }
    mutating func reconcile(_ builds: [String], source: UInt64 = 0) {
        var seen = Set<String>()
        order = Array(builds.prefix(256)).filter { seen.insert($0).inserted }
        if let active, !seen.contains(active) { activeWanted = false }
        entries = entries.filter { seen.contains($0.key) }
        for build in order where entries[build] == nil { entries[build] = Entry(source: source) }
    }
    mutating func next() -> String? {
        guard active == nil, let build = order.first(where: { entries[$0]?.outcome == nil }) else { return nil }
        active = build
        activeWanted = true
        return build
    }
    /// Returns the status-application stamp only if this result is still wanted.
    mutating func complete(_ build: String, _ outcome: Outcome) -> UInt64? {
        precondition(active == build)
        active = nil
        guard activeWanted, let entry = entries[build] else { return nil }
        entries[build]?.outcome = outcome
        return entry.source
    }
    func image(_ build: String?) -> Pixel? {
        guard let build, case .ready(let image) = entries[build]?.outcome else { return nil }
        return image
    }
    func failed(_ build: String?) -> Bool {
        guard let build, case .failed = entries[build]?.outcome else { return false }
        return true
    }
}

#if canImport(UIKit)
import UIKit

final class SessionArtwork {
    private var state = SessionArtworkState<UIImage>()
    private let worker = DispatchQueue(label: "app.playbit.artwork", qos: .utility)
    private let profile: URL
    private let published: Bool
    var changed: ((String) -> Void)?
    init(profile: URL, published: Bool = false) { self.profile = profile; self.published = published }
    func reconcile(_ rows: [SessionListSnapshot.Row]) {
        reconcile(builds: rows.compactMap(\.build))
    }
    func reconcile(builds: [String]) {
        dispatchPrecondition(condition: .onQueue(.main))
        state.reconcile(builds, source: pb_ios_trace_start())
        startNext()
    }
    func image(_ build: String?) -> UIImage? { state.image(build) }
    func failed(_ build: String?) -> Bool { state.failed(build) }
    private func startNext() {
        guard let build = state.next() else { return }
        let profile = self.profile
        let published = self.published
        worker.async { [weak self] in
            let result: SessionArtworkState<UIImage>.Outcome = autoreleasepool {
                do { return .ready(try Self.load(profile, build, published: published)) }
                catch {
                    NSLog("App icon %@: %@", build, String(error.localizedDescription.prefix(256)))
                    return .failed
                }
            }
            DispatchQueue.main.async { [weak self] in
                guard let self else { return }
                let start = pb_ios_trace_start()
                if let source = self.state.complete(build, result) {
                    self.changed?(build)
                    // Render readiness after updating visible cells, not presentation.
                    pb_ios_trace_end(PB_IOS_TRACE_ARTWORK_READY, source)
                }
                self.startNext()
                pb_ios_trace_end(PB_IOS_TRACE_ARTWORK_APPLY, start)
            }
        }
    }
    private static func load(_ profile: URL, _ build: String, published: Bool) throws -> UIImage? {
        func invalid(_ text: String) -> NSError {
            NSError(domain: "PlaybitArtwork", code: 1, userInfo: [NSLocalizedDescriptionKey: text])
        }
        var pixels = Data(count: 120 * 120 * 4)
        var present: UInt8 = 0
        let pointer = pixels.withUnsafeMutableBytes { output in
            profile.path.withCString { profile in build.withCString { build in
                pb_ios_icon_read(profile, build, published ? 1 : 0, output.baseAddress?.assumingMemoryBound(to: UInt8.self), output.count, &present)
            } }
        }
        if let pointer {
            defer { pb_ios_string_free(pointer) }
            throw invalid(String(cString: pointer))
        }
        guard present <= 1 else { throw invalid("Invalid artwork result") }
        guard present == 1 else { return nil }
        guard let provider = CGDataProvider(data: pixels as CFData),
              let color = CGColorSpace(name: CGColorSpace.sRGB),
              let image = CGImage(width: 120, height: 120, bitsPerComponent: 8, bitsPerPixel: 32,
                bytesPerRow: 480, space: color, bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.premultipliedLast.rawValue),
                provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { throw invalid("Couldn’t construct app artwork") }
        return UIImage(cgImage: image, scale: 3, orientation: .up)
    }
}
#endif
