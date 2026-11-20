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
let originalWindows = windowCount()
let opener = find(app,kAXButtonRole,"Open context menu")!
press(opener)
eventually("context menu did not open") { find(app,kAXMenuItemRole,"More") != nil }
let more = find(app,kAXMenuItemRole,"More")!
let submenu = children(more).first!
precondition(value(submenu,kAXRoleAttribute) as? String == kAXMenuRole)
print("AXMenuItem More → AXMenu exists before expansion")
set(more,kAXFocusedAttribute)
eventually("submenu is not nested under More") { find(more,kAXMenuItemRole,"Nested action") != nil }
let nested = find(more,kAXMenuItemRole,"Nested action")!
set(nested,kAXSelectedAttribute)
eventually("AXSelected did not highlight nested item") { (value(nested,kAXSelectedAttribute) as? NSNumber)?.boolValue == true }
precondition(windowCount() == originalWindows+1,"selection must not activate or close the menu")
set(more,kAXSelectedAttribute)
Thread.sleep(forTimeInterval: 0.1)
precondition(windowCount() == originalWindows+1,"returning to parent must not activate")
print("AXFocused opens child; AXSelected highlights child/parent without activation")
press(nested)
eventually("AXPress did not close the menu") { windowCount() == originalWindows }
print("AXPress on nested item activates and closes the menu")
