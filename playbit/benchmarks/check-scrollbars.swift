// PID-targeted packaged scrollbar checks; never activates the app or sends global input.
// swiftc -O check-scrollbars.swift -o /tmp/check-scrollbars
// /tmp/check-scrollbars PID [sample-count]
import ApplicationServices
import CoreGraphics
import Foundation
setbuf(stdout, nil)
let pid = pid_t(CommandLine.arguments[1])!
let app = AXUIElementCreateApplication(pid)
func value(_ e: AXUIElement, _ name: String) -> CFTypeRef? {
    var v: CFTypeRef?
    guard AXUIElementCopyAttributeValue(e, name as CFString, &v) == .success else { return nil }
    return v
}
func children(_ e: AXUIElement) -> [AXUIElement] { value(e, kAXChildrenAttribute) as? [AXUIElement] ?? [] }
func all(_ e: AXUIElement, _ role: String) -> [AXUIElement] {
    (value(e, kAXRoleAttribute) as? String == role ? [e] : []) + children(e).flatMap { all($0, role) }
}
func eventually(_ label: String, _ condition: () -> Bool) {
    for _ in 0..<200 { if condition() { return }; Thread.sleep(forTimeInterval: 0.01) }
    print("AX ranges:",all(app,kAXScrollBarRole).map { value($0,kAXValueAttribute) ?? "nil" as CFTypeRef }); fatalError(label)
}
let windows = value(app, kAXWindowsAttribute) as? [AXUIElement] ?? []
precondition(!windows.isEmpty)
let bars = all(windows[0], kAXScrollBarRole)
print("scrollbars:", bars.count)
for bar in bars { print(value(bar, kAXTitleAttribute) ?? "" as CFTypeRef, value(bar, kAXValueAttribute) ?? "" as CFTypeRef) }
let vertical = bars.last { (value($0,kAXOrientationAttribute) as? String) == kAXVerticalOrientationValue }!
if bars.count >= 3 {
    let outerBefore = (value(vertical,kAXValueAttribute) as? NSNumber)?.doubleValue ?? -1
    let nested = bars.filter { !CFEqual($0, vertical) }
    for bar in nested {
        precondition(AXUIElementSetAttributeValue(bar,kAXValueAttribute as CFString,NSNumber(value:0.5)) == .success)
        eventually("nested photo scrollbar responds independently") { abs(((value(bar,kAXValueAttribute) as? NSNumber)?.doubleValue ?? -1) - 0.5) < 0.02 }
    }
    precondition(abs(((value(vertical,kAXValueAttribute) as? NSNumber)?.doubleValue ?? -1) - outerBefore) < 0.02, "nested scrolling must not move the outer viewport")
    print("PASS: nested photo scroll axes preserve the outer offset")
}
let count = CommandLine.arguments.count > 2 ? Int(CommandLine.arguments[2])! : 4
precondition(count > 0 && count <= 10000)
for i in 0..<count {
    let target = i % 2 == 0 ? 0.2 : 0.8
    precondition(AXUIElementSetAttributeValue(vertical,kAXValueAttribute as CFString,NSNumber(value:target)) == .success)
    eventually("scroll offset is reflected in the AX range") { abs(((value(vertical,kAXValueAttribute) as? NSNumber)?.doubleValue ?? -1) - target) < 0.02 }
    Thread.sleep(forTimeInterval: 0.02)
}
print("PASS: \(count) alternating packaged scrollbar selections")
let info = CGWindowListCopyWindowInfo(.optionAll,kCGNullWindowID) as? [[String:Any]] ?? []
for item in info where item[kCGWindowOwnerPID as String] as? Int32 == pid && item[kCGWindowLayer as String] as? Int == 0 {
 print("WINDOW",item[kCGWindowNumber as String] ?? "",item[kCGWindowBounds as String] ?? "")
}
