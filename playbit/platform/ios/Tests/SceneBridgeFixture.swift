// C bridge fixture for testing the production Swift scene copy independently of UIKit.
import Foundation
struct SceneNode {
    var kind: UInt32 = 0, id: UInt32 = 0, style: UInt32 = 0, state: UInt32 = 0
    var disabled: UInt8 = 0, multiline: UInt8 = 0, read_only: UInt8 = 0
    var text: UnsafePointer<UInt8>?, text_len = 0
    var placeholder: UnsafePointer<UInt8>?, placeholder_len = 0
    var children = 0
    var width: UInt32 = 0, height: UInt32 = 0, frames: UInt32 = 0
    var items = 0
    var clear_color: UInt32 = 0, has_clear_color: UInt8 = 0
}
struct SceneCanvasItem {
    var kind: UInt32 = 0, x: UInt32 = 0, y: UInt32 = 0, width: UInt32 = 0, height: UInt32 = 0, color: UInt32 = 0, font_size: UInt32 = 0
    var bold: UInt8 = 0
    var text: UnsafePointer<UInt8>?, text_len = 0
    var source_width: UInt32 = 0, source_height: UInt32 = 0
    var pixels: UnsafePointer<UInt8>?, pixels_len = 0
}
final class CanvasItemFixture {
    var fields: SceneCanvasItem
    private var text: UnsafeMutablePointer<UInt8>
    private var pixels: UnsafeMutablePointer<UInt8>
    init(_ fields: SceneCanvasItem, text: String = "", pixels: [UInt8] = []) {
        self.fields = fields
        let bytes = Array(text.utf8)
        self.text = .allocate(capacity: max(1, bytes.count))
        for (i, byte) in bytes.enumerated() { self.text[i] = byte }
        self.fields.text = UnsafePointer(self.text); self.fields.text_len = bytes.count
        self.pixels = .allocate(capacity: max(1, pixels.count))
        for (i, byte) in pixels.enumerated() { self.pixels[i] = byte }
        self.fields.pixels = UnsafePointer(self.pixels); self.fields.pixels_len = pixels.count
    }
    deinit { text.deallocate(); pixels.deallocate() }
}
final class SceneFixture {
    var fields = SceneNode()
    var children: [SceneFixture] = []
    var items: [CanvasItemFixture] = []
    private var text: UnsafeMutablePointer<UInt8>
    private var placeholder: UnsafeMutablePointer<UInt8>
    init(kind: UInt32, text: String = "", placeholder: String = "") {
        fields.kind = kind
        let bytes = Array(text.utf8), hint = Array(placeholder.utf8)
        self.text = .allocate(capacity: max(1, bytes.count))
        self.placeholder = .allocate(capacity: max(1, hint.count))
        for (i, byte) in bytes.enumerated() { self.text[i] = byte }
        for (i, byte) in hint.enumerated() { self.placeholder[i] = byte }
        fields.text = UnsafePointer(self.text); fields.text_len = bytes.count
        fields.placeholder = UnsafePointer(self.placeholder); fields.placeholder_len = hint.count
    }
    deinit { text.deallocate(); placeholder.deallocate() }
    var pointer: OpaquePointer { OpaquePointer(Unmanaged.passUnretained(self).toOpaque()) }
}
func fixture(_ pointer: OpaquePointer) -> SceneFixture {
    Unmanaged<SceneFixture>.fromOpaque(UnsafeRawPointer(pointer)).takeUnretainedValue()
}
func pb_ios_scene_node(_ pointer: OpaquePointer) -> SceneNode {
    let value = fixture(pointer)
    var fields = value.fields
    fields.children = value.children.count; fields.items = value.items.count
    return fields
}
func pb_ios_scene_child(_ pointer: OpaquePointer, _ index: Int) -> OpaquePointer? {
    let value = fixture(pointer)
    return value.children.indices.contains(index) ? value.children[index].pointer : nil
}
func pb_ios_scene_canvas_item(_ pointer: OpaquePointer, _ index: Int) -> SceneCanvasItem {
    fixture(pointer).items[index].fields
}
