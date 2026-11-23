import Foundation

struct GuestScene: Equatable {
    enum Kind: UInt32 { case column = 1, row, text, button, textField, checkbox, canvas }
    struct Rect: Equatable {
        var x: UInt32, y: UInt32, width: UInt32, height: UInt32, color: UInt32
    }
    enum CanvasItem: Equatable {
        case rect(Rect)
        case text(Rect, fontSize: UInt32, bold: Bool, text: String)
        case pixels(Rect, width: UInt32, height: UInt32, rgba: Data)
    }
    var kind: Kind
    var id: UInt32
    var style: UInt32
    var state: UInt32
    var disabled: Bool
    var multiline: Bool
    var readOnly: Bool
    var text: String
    var placeholder: String
    var children: [GuestScene]
    var width: UInt32
    var height: UInt32
    var frames: UInt32
    var clearColor: UInt32?
    var items: [CanvasItem]

    // All borrowed spans are copied before EngineHost releases the reply.
    static func read(_ pointer: OpaquePointer) throws -> GuestScene {
        let node = pb_ios_scene_node(pointer)
        guard let kind = Kind(rawValue: node.kind) else {
            throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native guest scene"])
        }
        let children = try (0..<node.children).map { index in
            guard let child = pb_ios_scene_child(pointer, index) else {
                throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Missing native guest control"])
            }
            return try read(child)
        }
        let items = try (0..<node.items).map { index -> CanvasItem in
            let item = pb_ios_scene_canvas_item(pointer, index)
            let bounds = Rect(x: item.x, y: item.y, width: item.width, height: item.height, color: item.color)
            switch item.kind {
            case 0: return .rect(bounds)
            case 1: return .text(bounds, fontSize: item.font_size, bold: item.bold != 0,
                text: String(decoding: UnsafeBufferPointer(start: item.text, count: item.text_len), as: UTF8.self))
            case 2:
                guard item.source_width > 0, item.source_height > 0,
                      item.source_width <= 4096, item.source_height <= 4096,
                      item.pixels_len <= 32 * 1024 * 1024,
                      UInt64(item.source_width) * UInt64(item.source_height) * 4 == item.pixels_len,
                      let pixels = item.pixels else {
                    throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native packed pixels"])
                }
                return .pixels(bounds, width: item.source_width, height: item.source_height,
                    rgba: Data(bytes: pixels, count: item.pixels_len))
            default: throw NSError(domain: "Playbit", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid native canvas item"])
            }
        }
        return GuestScene(kind: kind, id: node.id, style: node.style, state: node.state,
            disabled: node.disabled != 0, multiline: node.multiline != 0, readOnly: node.read_only != 0,
            text: String(decoding: UnsafeBufferPointer(start: node.text, count: node.text_len), as: UTF8.self),
            placeholder: String(decoding: UnsafeBufferPointer(start: node.placeholder, count: node.placeholder_len), as: UTF8.self),
            children: children, width: node.width, height: node.height, frames: node.frames,
            clearColor: node.has_clear_color != 0 ? node.clear_color : nil, items: items)
    }
}
