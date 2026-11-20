// Packaged, process-targeted segmented-control regression and trace workload.
// swiftc -O check-segmented.swift -o /tmp/check-segmented
// /tmp/check-segmented PID [sample-count]; never activates an app or sends global input.
import ApplicationServices
import CoreGraphics
import Foundation
let pid = pid_t(CommandLine.arguments[1])!
let application = AXUIElementCreateApplication(pid)
let app = (value(application, kAXWindowsAttribute) as? [AXUIElement])!.first!
func value(_ element: AXUIElement, _ key: String) -> CFTypeRef? {
    var result: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, key as CFString, &result) == .success else { return nil }
    return result
}
func children(_ element: AXUIElement) -> [AXUIElement] { value(element, kAXChildrenAttribute) as? [AXUIElement] ?? [] }
func all(_ element: AXUIElement, _ role: String) -> [AXUIElement] {
    (value(element, kAXRoleAttribute) as? String == role ? [element] : []) + children(element).flatMap { all($0, role) }
}
func label(_ element: AXUIElement) -> String { value(element, kAXTitleAttribute) as? String ?? "" }
func selected(_ element: AXUIElement) -> Bool { (value(element, kAXValueAttribute) as? NSNumber)?.boolValue == true }
func eventually(_ message: String, _ condition: () -> Bool) {
    for _ in 0..<100 { if condition() { return }; Thread.sleep(forTimeInterval: 0.02) }
    fatalError(message)
}
func press(_ element: AXUIElement) { precondition(AXUIElementPerformAction(element, kAXPressAction as CFString) == .success) }
func key(_ code: CGKeyCode) {
    for down in [true, false] {
        let event = CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: down)!
        event.flags = (123...126).contains(code) ? [.maskSecondaryFn, .maskNumericPad] : []
        event.postToPid(pid)
    }
}
let groups = all(app, kAXRadioGroupRole)
precondition(groups.count == 3, "expected three segmented groups, got \(groups.count)")
let speed = children(groups[0]), animals = children(groups[1]), disabled = children(groups[2])
precondition(speed.map(label) == ["Slow", "Medium", "Fast"])
precondition(animals.count == 5 && disabled.count == 3)
precondition((value(animals[1], kAXEnabledAttribute) as? NSNumber)?.boolValue == false)
precondition(disabled.allSatisfy { (value($0, kAXEnabledAttribute) as? NSNumber)?.boolValue == false })
if CommandLine.arguments.count > 2 {
    let count = Int(CommandLine.arguments[2])!
    precondition(count > 0 && count <= 10000)
    let first = selected(speed[0]) ? 2 : 0
    func actionCount() -> Int? {
        for e in all(app, kAXStaticTextRole) {
            if let text = value(e, kAXValueAttribute) as? String, text.hasSuffix(" actions"),
               let count = text.split(separator: "·").last?.split(separator: " ").first.flatMap({ Int($0) }) { return count }
        }
        return nil
    }
    let before = actionCount()!
    for i in 0..<count {
        let target = speed[i % 2 == 0 ? first : 2 - first]
        press(target)
        eventually("selection did not change") { selected(target) }
        Thread.sleep(forTimeInterval: 0.02)
    }
    eventually("guest must acknowledge every selection") { actionCount() == before + count }
    print("Completed \(count) guest-acknowledged alternating packaged segment selections; analyze app trace for CPU readiness")
} else {
    press(speed[1]); eventually("select Medium before navigation") { selected(speed[1]) }
    precondition(AXUIElementSetAttributeValue(speed[1], kAXFocusedAttribute as CFString, kCFBooleanTrue) == .success)
    key(124); eventually("Right selects Fast") { selected(speed[2]) }
    key(124); Thread.sleep(forTimeInterval: 0.04); precondition(selected(speed[2]))
    key(123); eventually("Left selects Medium") { selected(speed[1]) }
    press(speed[0]); eventually("AX press selects Slow") { selected(speed[0]) }
    precondition(AXUIElementSetAttributeValue(animals[2], kAXFocusedAttribute as CFString, kCFBooleanTrue) == .success)
    key(123); eventually("Left skips disabled Slow") { selected(animals[0]) }
    key(124); eventually("Right skips disabled Slow") { selected(animals[2]) }
    print("Packaged segment AX selection, disabled state, Left/Right, and disabled skipping passed")
}
