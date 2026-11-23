import Foundation
@main struct GuestSceneTests {
    static func main() throws {
        let scene: GuestScene
        do {
            let root = SceneFixture(kind: 1)
            let field = SceneFixture(kind: 5, text: "A\0é draft", placeholder: "Type here")
            field.fields.id = 7; field.fields.read_only = 1; field.fields.multiline = 1
            let canvas = SceneFixture(kind: 7)
            canvas.fields.id = 9; canvas.fields.width = 320; canvas.fields.height = 200
            canvas.fields.frames = 1
            canvas.fields.has_clear_color = 1; canvas.fields.clear_color = 0x22334480
            canvas.items = [
                CanvasItemFixture(SceneCanvasItem(x: 1, y: 2, width: 30, height: 40, color: 0x11223344)),
                CanvasItemFixture(SceneCanvasItem(kind: 1, x: 4, y: 8, width: 100, height: 20,
                    color: 0x55667788, font_size: 14, bold: 1), text: "Trace é path"),
                CanvasItemFixture(SceneCanvasItem(kind: 2, x: 2, y: 3, width: 8, height: 4,
                    source_width: 2, source_height: 1), pixels: [255, 0, 0, 255, 0, 255, 0, 128])
            ]
            root.children = [field, canvas]
            scene = try GuestScene.read(root.pointer)
        }
        // Native owners are gone; all Swift text and rectangle storage remains valid.
        precondition(scene.kind == .column && scene.children.count == 2)
        precondition(scene.children[0].text == "A\0é draft")
        precondition(scene.children[0].placeholder == "Type here")
        precondition(scene.children[0].readOnly && scene.children[0].multiline)
        precondition(scene.children[1].items[0] == .rect(GuestScene.Rect(x: 1, y: 2, width: 30, height: 40, color: 0x11223344)))
        precondition(scene.children[1].items[1] == .text(GuestScene.Rect(x: 4, y: 8, width: 100, height: 20,
            color: 0x55667788), fontSize: 14, bold: true, text: "Trace é path"))
        precondition(scene.children[1].frames == 1 && scene.children[1].clearColor == 0x22334480)
        precondition(scene.children[1].items[2] == .pixels(GuestScene.Rect(x: 2, y: 3, width: 8, height: 4, color: 0),
            width: 2, height: 1, rgba: Data([255, 0, 0, 255, 0, 255, 0, 128])))
        precondition(scene.children[1].width == 320 && scene.children[1].height == 200)
        precondition(scene == scene)
        let bad = SceneFixture(kind: 99)
        do { _ = try GuestScene.read(bad.pointer); preconditionFailure("unknown scene kind accepted") }
        catch { }
        let invalidPixels = SceneFixture(kind: 7)
        invalidPixels.items = [CanvasItemFixture(SceneCanvasItem(kind: 2, source_width: 2, source_height: 1), pixels: [0, 0, 0, 0])]
        do { _ = try GuestScene.read(invalidPixels.pointer); preconditionFailure("malformed pixel span accepted") }
        catch { }
        let invalidItem = SceneFixture(kind: 7)
        invalidItem.items = [CanvasItemFixture(SceneCanvasItem(kind: 99))]
        do { _ = try GuestScene.read(invalidItem.pointer); preconditionFailure("unknown item kind accepted") }
        catch { }
        let staticCanvas = SceneFixture(kind: 7)
        let copiedStatic = try GuestScene.read(staticCanvas.pointer)
        precondition(copiedStatic.frames == 0 && copiedStatic.clearColor == nil)
        print("GuestSceneTests: typed fields, copied text/pixel lifetimes, embedded NUL, frame policy and malformed spans passed")
    }
}
