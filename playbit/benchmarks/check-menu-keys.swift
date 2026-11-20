// Process-targeted regression; never activates the app or sends global input.
// Compile with swiftc -O, then pass the PID of a running packaged UI Gallery.
import ApplicationServices
import CoreGraphics
import Foundation

let pid = pid_t(CommandLine.arguments[1])!
let app = AXUIElementCreateApplication(pid)
func value(_ element: AXUIElement, _ key: String) -> CFTypeRef? {
    var result: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, key as CFString, &result) == .success else { return nil }
    return result
}
func children(_ element: AXUIElement) -> [AXUIElement] {
    value(element, kAXChildrenAttribute) as? [AXUIElement] ?? []
}
func find(_ element: AXUIElement, _ role: String, _ title: String) -> AXUIElement? {
    if value(element,kAXRoleAttribute) as? String == role && value(element,kAXTitleAttribute) as? String == title { return element }
    for child in children(element) { if let found = find(child,role,title) { return found } }
    return nil
}
func eventually(_ message: String, _ condition: () -> Bool) {
    for _ in 0..<100 {
        if condition() { return }
        Thread.sleep(forTimeInterval: 0.03)
    }
    fatalError(message)
}
func windowCount() -> Int {
    (CGWindowListCopyWindowInfo(.optionOnScreenOnly,kCGNullWindowID) as? [[String:Any]] ?? [])
        .filter { $0[kCGWindowOwnerPID as String] as? Int == Int(pid) }.count
}
func set(_ element: AXUIElement, _ attribute: String) {
    precondition(AXUIElementSetAttributeValue(element,attribute as CFString,kCFBooleanTrue) == .success)
}
func press(_ element: AXUIElement) {
    precondition(AXUIElementPerformAction(element,kAXPressAction as CFString) == .success)
}
// Keyboard events are posted only to this PID; the app is never activated.
func key(_ code: CGKeyCode) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!
        event.flags = (123...126).contains(code) ? [.maskSecondaryFn, .maskNumericPad] : []
        event.postToPid(pid)
    }
}
func selected(_ title: String) -> Bool {
    guard let item = find(app,kAXMenuItemRole,title) else { return false }
    return (value(item,kAXSelectedAttribute) as? NSNumber)?.boolValue == true
}
let originalWindows = windowCount()
press(find(app,kAXButtonRole,"Open context menu")!)
eventually("menu not open") { find(app,kAXMenuItemRole,"Inspect") != nil }
key(125)
eventually("Down must select Inspect") { selected("Inspect") }
key(125)
eventually("Down must select Copy") { selected("Copy") }
key(126)
eventually("Up must select Inspect") { selected("Inspect") }
key(125)
eventually("Down must select Copy again") { selected("Copy") }
key(125)
eventually("Down must select More") { selected("More") }
key(124)
eventually("Right must select Nested action") { selected("Nested action") }
key(123)
eventually("Left must restore parent and close child") {
    selected("More") && find(app,kAXMenuItemRole,"Nested action") == nil
}
key(53)
eventually("Escape must dismiss") { windowCount() == originalWindows }
print("Packaged PID-targeted Up/Down/Right/Left/Escape passed without activating app")
