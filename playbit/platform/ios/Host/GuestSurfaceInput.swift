import UIKit
import GameController

final class GuestWindow: UIWindow {
    override func sendEvent(_ event: UIEvent) {
        if isKeyWindow, let nav = rootViewController as? UINavigationController,
            let guest = nav.topViewController as? GuestController, guest.presentedViewController == nil {
            guest.observeInput(event)
        }
        super.sendEvent(event)
    }
}

final class GuestSurfaceInput: NSObject, UIGestureRecognizerDelegate {
    struct Contact {
        var id: UInt32
        var point: CGPoint
        var local: CGPoint
        var target: UInt32
        var kind: UInt8
        var primary: Bool
        var held: Bool
        var buttons: UInt16
    }
    struct Controller {
        let device: UInt32
        var buttons = Array(repeating: false, count: 13)
        var axes = Array(repeating: Float(0), count: 8)
    }
    weak var root: UIView?
    weak var content: UIView?
    weak var viewport: UIView?
    let send: (Int, UInt32, Data, Int) -> Bool
    var scrollers: [UIPanGestureRecognizer] = []
    var mask: UInt32 = 0
    var contacts: [ObjectIdentifier: Contact] = [:]
    var heldKeys: [Int: (UInt16, UInt16)] = [:]
    var gestureTouches: [ObjectIdentifier: CGPoint] = [:]
    private var gestures = GuestInputSequences()
    private var wheels = GuestInputSequences()
    var controllers: [ObjectIdentifier: Controller] = [:]
    var observers: [NSObjectProtocol] = []
    var nextDevice: UInt32 = 16
    var nextPointer: UInt32 = 1

    init(root: UIView, viewport: UIView, send: @escaping (Int, UInt32, Data, Int) -> Bool) {
        self.root = root
        self.viewport = viewport
        self.send = send
        super.init()
        for types in [UIScrollTypeMask.continuous, .discrete] {
            let pan = UIPanGestureRecognizer(target: self, action: #selector(wheel(_:)))
            pan.allowedScrollTypesMask = types
            pan.allowedTouchTypes = []
            pan.cancelsTouchesInView = false
            pan.delegate = self
            pan.isEnabled = false
            root.addGestureRecognizer(pan)
            scrollers.append(pan)
        }
    }
    deinit { observers.forEach(NotificationCenter.default.removeObserver) }

    private var contentBounds: CGRect? {
        guard let root, let content, let viewport else { return nil }
        let scene = content.convert(content.bounds, to: root)
        let clip = viewport.convert(viewport.bounds, to: root)
        let top = max(scene.minY, clip.minY)
        return CGRect(x: scene.minX, y: top, width: scene.width, height: max(0, clip.maxY - 16 - top))
    }

    func configure(_ next: UInt32) {
        if mask & 4 != 0 && next & 4 == 0 {
            for (kind, point) in wheels.points { scroll(kind, phase: .ended, point: point, delta: .zero) }
        }
        for recognizer in scrollers { recognizer.isEnabled = next & 4 != 0 }
        if mask & 1 != 0 && next & 1 == 0 {
            let time = ProcessInfo.processInfo.systemUptime
            for contact in contacts.values {
                var terminal = contact
                terminal.held = false
                terminal.buttons = 0
                pointer(terminal, phase: contact.held ? 3 : 5, time: time, mods: 0, previous: contact)
            }
            contacts.removeAll(keepingCapacity: true)
        }
        if mask & 2 != 0 && next & 2 == 0 {
            let time = ProcessInfo.processInfo.systemUptime
            for key in heldKeys.values { keyboard(key, down: false, time: time, mods: 0) }
            heldKeys.removeAll(keepingCapacity: true)
        }
        if mask & 8 != 0 && next & 8 == 0 {
            gesture(Array(gestureTouches.values.prefix(2)), [], time: ProcessInfo.processInfo.systemUptime, mods: 0)
            gestureTouches.removeAll(keepingCapacity: true)
        }
        if mask & 16 != 0 && next & 16 == 0 {
            for state in controllers.values { disconnect(state) }
            controllers.removeAll(keepingCapacity: true)
            for device in GCController.controllers() { device.extendedGamepad?.valueChangedHandler = nil }
            observers.forEach(NotificationCenter.default.removeObserver)
            observers.removeAll(keepingCapacity: true)
        } else if mask & 16 == 0 && next & 16 != 0 {
            let center = NotificationCenter.default
            for name in [Notification.Name.GCControllerDidConnect, Notification.Name.GCControllerDidDisconnect] {
                observers.append(center.addObserver(forName: name, object: nil, queue: .main) { [weak self] note in
                    guard let self, let device = note.object as? GCController else { return }
                    if note.name == .GCControllerDidConnect { self.attach(device) }
                    else if let state = self.controllers.removeValue(forKey: ObjectIdentifier(device)) {
                        self.disconnect(state)
                    }
                })
            }
            for device in GCController.controllers().prefix(8) { attach(device) }
        }
        mask = next
    }

    func receive(_ event: UIEvent) {
        guard let root else { return }
        let mods = Self.modifiers(event.modifierFlags)
        if mask & 2 != 0, let presses = event as? UIPressesEvent {
            for press in presses.allPresses {
                guard let key = press.key else { continue }
                let code = Int(key.keyCode.rawValue)
                let down: Bool
                switch press.phase {
                case .began, .changed: down = true
                case .ended, .cancelled: down = false
                default: continue // A key held stationary during a chord has no transition.
                }
                let logical = Self.logicalKey(key.charactersIgnoringModifiers)
                let physical = Self.physicalKey(code)
                let pair = heldKeys[code] ?? (logical == 0 ? physical : logical, physical)
                if !down && heldKeys[code] == nil { continue }
                if keyboard(pair, down: down, time: press.timestamp, mods: mods,
                            text: down ? key.characters : "", repeatKey: down && heldKeys[code] != nil) {
                    if down && heldKeys.count < 128 { heldKeys[code] = pair }
                    if !down { heldKeys.removeValue(forKey: code) }
                }
            }
        }
        guard mask & 9 != 0, let touches = event.allTouches, let bounds = contentBounds else { return }
        for touch in touches {
            let identity = ObjectIdentifier(touch)
            let location = touch.location(in: root)
            let old = contacts[identity]
            guard old != nil || gestureTouches[identity] != nil || bounds.contains(location) else { continue }
            let phase: UInt8
            switch touch.phase {
            case .began: phase = 1
            case .moved, .regionMoved: phase = 0
            case .ended: phase = 2
            case .cancelled: phase = 3
            case .regionEntered: phase = 4
            case .regionExited: phase = 5
            default: continue
            }
            let point = CGPoint(x: location.x - bounds.minX, y: location.y - bounds.minY)
            if (phase == 2 || phase == 3) && old?.held != true && gestureTouches[identity] == nil { continue }
            if mask & 8 != 0 && touch.type == .direct {
                let before = Array(gestureTouches.values.prefix(2))
                if phase == 2 || phase == 3 { gestureTouches.removeValue(forKey: identity) }
                else if gestureTouches.count < 16 || gestureTouches[identity] != nil { gestureTouches[identity] = point }
                gesture(before, Array(gestureTouches.values.prefix(2)), time: touch.timestamp, mods: mods)
            }
            guard mask & 1 != 0, contacts.count < 16 || contacts[identity] != nil else { continue }
            var view = touch.view
            while view != nil && !(view is CanvasView) && view !== root { view = view?.superview }
            let canvas = view as? CanvasView
            let kind: UInt8 = touch.type == .pencil ? 4 : touch.type == .indirectPointer ? 1 : 2
            var local = CGPoint.zero
            if let canvas, let scene = canvas.scene, canvas.bounds.width > 0, canvas.bounds.height > 0 {
                let location = touch.location(in: canvas)
                local = CGPoint(x: location.x * CGFloat(scene.width) / canvas.bounds.width,
                                y: location.y * CGFloat(scene.height) / canvas.bounds.height)
            }
            let held = phase == 1 || (old?.held == true && phase == 0)
            let mouseButtons = UInt16(truncatingIfNeeded: event.buttonMask.rawValue)
            let buttons: UInt16 = held ? (kind == 1 && mouseButtons != 0 ? mouseButtons : 1) : 0
            let contact = Contact(id: old?.id ?? nextPointer, point: point, local: local,
                target: UInt32(canvas?.tag ?? 0), kind: kind, primary: old?.primary ?? contacts.values.allSatisfy { $0.kind != kind },
                held: held, buttons: buttons)
            if pointer(contact, phase: phase, time: touch.timestamp, mods: mods, touch: touch, previous: old) {
                if phase == 2 || phase == 3 || phase == 5 { contacts.removeValue(forKey: identity) }
                else { contacts[identity] = contact; if old == nil { nextPointer &+= 1 } }
            }
        }
    }

    @discardableResult func pointer(_ c: Contact, phase: UInt8, time: Double, mods: UInt16,
                                   touch: UITouch? = nil, previous: Contact? = nil) -> Bool {
        let down = c.held
        let flags = (c.primary ? UInt16(1) : 0) | (down && c.kind != 1 ? UInt16(2) : 0)
        var data = Self.packet(time, UInt32(c.kind), mods)
        Self.write(c.id, to: &data)
        let changed = c.buttons ^ (previous?.buttons ?? 0)
        let button = (phase == 1 || phase == 2) && changed != 0 ? UInt8(changed.trailingZeroBitCount) : 255
        data.append(contentsOf: [phase, c.kind, button, UInt8(clamping: touch?.tapCount ?? 0)])
        Self.write(flags, to: &data)
        Self.write(c.buttons, to: &data)
        Self.writeFloats([c.point.x, c.point.y, c.point.x - (previous?.point.x ?? c.point.x),
                          c.point.y - (previous?.point.y ?? c.point.y), c.local.x, c.local.y], to: &data)
        if c.kind == 4 {
            let pressure = (touch?.maximumPossibleForce ?? 0) > 0 ? touch!.force / touch!.maximumPossibleForce : 0
            let altitude = touch?.altitudeAngle ?? 0
            let azimuth = root.map { touch?.azimuthAngle(in: $0) ?? 0 } ?? 0
            let tilt = 1 - altitude / (.pi / 2)
            let radius = touch?.majorRadius ?? 0
            Self.writeFloats([min(1, max(0, pressure)), 0, tilt * cos(azimuth), tilt * sin(azimuth),
                              0, radius * 2, radius * 2, altitude, azimuth], to: &data)
        }
        return send(22, c.target, data, (c.held ? 1 : 0) - (previous?.held == true ? 1 : 0))
    }

    @discardableResult func keyboard(_ key: (UInt16, UInt16), down: Bool, time: Double, mods: UInt16,
                                    text: String = "", repeatKey: Bool = false) -> Bool {
        var data = Self.packet(time, 8, mods)
        Self.write(key.0, to: &data)
        Self.write(key.1, to: &data)
        let characters = text.unicodeScalars.filter { !CharacterSet.controlCharacters.contains($0) }.prefix(8)
        data.append(contentsOf: [down ? 1 : 0, repeatKey ? 1 : 0, UInt8(characters.count), 0])
        for scalar in characters { Self.write(scalar.value, to: &data) }
        data.append(contentsOf: repeatElement(0, count: 4 * (8 - characters.count)))
        return send(23, 0, data, repeatKey ? 0 : down ? 1 : -1)
    }

    func gesture(_ before: [CGPoint], _ after: [CGPoint], time: Double, mods: UInt16) {
        guard before.count == 2 || after.count == 2 else { return }
        let phase: GuestInputSequences.Phase = before.count < 2 ? .began : after.count < 2 ? .ended : .changed
        let p = phase == .ended ? before : after
        let q = phase == .changed ? before : p
        let x = (p[0].x + p[1].x) / 2, y = (p[0].y + p[1].y) / 2
        let dx = x - (q[0].x + q[1].x) / 2, dy = y - (q[0].y + q[1].y) / 2
        let distance = hypot(q[1].x - q[0].x, q[1].y - q[0].y)
        let scale = distance == 0 ? 1 : hypot(p[1].x - p[0].x, p[1].y - p[0].y) / distance
        let angle = atan2(p[1].y - p[0].y, p[1].x - p[0].x) - atan2(q[1].y - q[0].y, q[1].x - q[0].x)
        for kind in UInt8(0)...UInt8(2) {
            gestures.deliver(phase, kind: kind, point: CGPoint(x: x, y: y)) { reservation in
                var data = Self.packet(time, 2, mods)
                data.append(contentsOf: [kind, phase.rawValue, 0, 0])
                Self.writeFloats([x, y, kind == 0 ? dx : 0, kind == 0 ? dy : 0,
                                  kind == 1 && scale > 0 ? scale : 1, kind == 2 ? atan2(sin(angle), cos(angle)) : 0], to: &data)
                return send(25, 0, data, reservation)
            }
        }
    }

    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith otherGestureRecognizer: UIGestureRecognizer) -> Bool { true }

    @objc private func wheel(_ pan: UIPanGestureRecognizer) {
        guard mask & 4 != 0, let root, let bounds = contentBounds else { return }
        let p = pan.location(in: root)
        let phase: GuestInputSequences.Phase
        switch pan.state {
        case .began:
            guard bounds.contains(p) else { return }
            phase = .began
        case .changed: phase = .changed
        case .ended, .cancelled, .failed: phase = .ended
        default: return
        }
        let delta = pan.translation(in: root)
        pan.setTranslation(.zero, in: root)
        let kind: UInt8 = pan.allowedScrollTypesMask == .discrete ? 1 : 3
        scroll(kind, phase: phase, point: CGPoint(x: p.x - bounds.minX, y: p.y - bounds.minY), delta: delta)
    }
    private func scroll(_ kind: UInt8, phase: GuestInputSequences.Phase, point: CGPoint, delta: CGPoint) {
        wheels.deliver(phase, kind: kind, point: point) { reservation in
            var data = Self.packet(ProcessInfo.processInfo.systemUptime, UInt32(kind), 0)
            data.append(contentsOf: [kind, phase.rawValue])
            Self.write(UInt16(kind == 1 ? 0 : 1), to: &data)
            Self.writeFloats([point.x, point.y, -delta.x, -delta.y, 0], to: &data)
            return send(24, 0, data, reservation)
        }
    }

    func attach(_ device: GCController) {
        let key = ObjectIdentifier(device)
        guard controllers.count < 8, controllers[key] == nil, let profile = device.extendedGamepad,
            controller(nextDevice, phase: 0, reservation: 1) else { return }
        controllers[key] = Controller(device: nextDevice)
        nextDevice &+= 1
        device.handlerQueue = .main
        profile.valueChangedHandler = { [weak self] profile, _ in self?.sample(profile, key: key) }
        sample(profile, key: key)
    }
    func disconnect(_ state: Controller) {
        let reserved = 1 + state.buttons.filter { $0 }.count
        controller(state.device, phase: 1, reservation: -reserved)
    }
    func sample(_ p: GCExtendedGamepad, key: ObjectIdentifier) {
        guard var state = controllers[key] else { return }
        let buttons: [GCControllerButtonInput?] = [p.buttonA, p.buttonB, p.buttonX, p.buttonY, p.leftShoulder, p.rightShoulder,
            p.leftTrigger, p.rightTrigger, p.leftThumbstickButton, p.rightThumbstickButton, p.buttonMenu, p.buttonOptions, p.buttonHome]
        for (i, button) in buttons.enumerated() {
            let pressed = button?.isPressed ?? false
            if pressed != state.buttons[i] && controller(state.device, phase: pressed ? 2 : 3, control: UInt16(i),
                value: button?.value ?? 0, reservation: pressed ? 1 : -1) {
                state.buttons[i] = pressed
            }
        }
        let axes = [p.leftThumbstick.xAxis.value, p.leftThumbstick.yAxis.value, p.rightThumbstick.xAxis.value, p.rightThumbstick.yAxis.value,
                    p.dpad.xAxis.value, p.dpad.yAxis.value, p.leftTrigger.value, p.rightTrigger.value]
        for (i, value) in axes.enumerated() where state.axes[i] != value {
            let pair = i & ~1
            if controller(state.device, phase: 4, control: UInt16(i), value: value, x: i < 6 ? axes[pair] : 0, y: i < 6 ? axes[pair + 1] : 0) {
                state.axes[i] = value
            }
        }
        controllers[key] = state
    }
    @discardableResult func controller(_ device: UInt32, phase: UInt8, control: UInt16 = 0, value: Float = 0,
                                       x: Float = 0, y: Float = 0, reservation: Int = 0) -> Bool {
        var data = Self.packet(ProcessInfo.processInfo.systemUptime, device, 0)
        data.append(contentsOf: [phase, 0])
        Self.write(control, to: &data)
        for number in [value, x, y] { Self.write(number.bitPattern, to: &data) }
        return send(26, 0, data, reservation)
    }

    static func packet(_ time: Double, _ device: UInt32, _ mods: UInt16) -> Data {
        var data = Data(capacity: 92)
        write(UInt64(time * 1e9), to: &data)
        write(UInt32(0), to: &data)
        write(device, to: &data)
        write(mods, to: &data)
        write(UInt16(0), to: &data)
        return data
    }
    static func write<T: FixedWidthInteger>(_ value: T, to data: inout Data) {
        var little = value.littleEndian
        withUnsafeBytes(of: &little) { data.append(contentsOf: $0) }
    }
    static func writeFloats(_ values: [CGFloat], to data: inout Data) {
        for number in values { write(Float(number).bitPattern, to: &data) }
    }
    static func modifiers(_ flags: UIKeyModifierFlags) -> UInt16 {
        (flags.contains(.shift) ? 1 : 0) | (flags.contains(.control) ? 2 : 0) |
        (flags.contains(.alternate) ? 4 : 0) | (flags.contains(.command) ? 8 : 0) |
        (flags.contains(.alphaShift) ? 16 : 0)
    }
    static func physicalKey(_ code: Int) -> UInt16 {
        switch code {
        case 4: return 19 // A
        case 5: return 20 // B
        case 6: return 21 // C
        case 7: return 22 // D
        case 8: return 23 // E
        case 9: return 24 // F
        case 10: return 25 // G
        case 11: return 26 // H
        case 12: return 27 // I
        case 13: return 28 // J
        case 14: return 29 // K
        case 15: return 30 // L
        case 16: return 31 // M
        case 17: return 32 // N
        case 18: return 33 // O
        case 19: return 34 // P
        case 20: return 35 // Q
        case 21: return 36 // R
        case 22: return 37 // S
        case 23: return 38 // T
        case 24: return 39 // U
        case 25: return 40 // V
        case 26: return 41 // W
        case 27: return 42 // X
        case 28: return 43 // Y
        case 29: return 44 // Z
        case 30: return 8 // Digit1
        case 31: return 9 // Digit2
        case 32: return 10 // Digit3
        case 33: return 11 // Digit4
        case 34: return 12 // Digit5
        case 35: return 13 // Digit6
        case 36: return 14 // Digit7
        case 37: return 15 // Digit8
        case 38: return 16 // Digit9
        case 39: return 7 // Digit0
        case 40: return 50 // Enter
        case 41: return 49 // Escape
        case 42: return 52 // Backspace
        case 43: return 51 // Tab
        case 44: return 1 // Space
        case 45: return 4 // Minus
        case 46: return 18 // Equal
        case 47: return 45 // LeftBracket
        case 48: return 47 // RightBracket
        case 49: return 46 // Backslash
        case 50: return 72 // World1
        case 51: return 17 // Semicolon
        case 52: return 2 // Quote
        case 53: return 48 // Grave
        case 54: return 3 // Comma
        case 55: return 5 // Period
        case 56: return 6 // Slash
        case 57: return 63 // CapsLock
        case 58: return 78 // F1
        case 59: return 79 // F2
        case 60: return 80 // F3
        case 61: return 81 // F4
        case 62: return 82 // F5
        case 63: return 83 // F6
        case 64: return 84 // F7
        case 65: return 85 // F8
        case 66: return 86 // F9
        case 67: return 87 // F10
        case 68: return 88 // F11
        case 69: return 89 // F12
        case 70: return 76 // PrintScreen
        case 71: return 74 // ScrollLock
        case 72: return 77 // Pause
        case 73: return 53 // Insert
        case 74: return 61 // Home
        case 75: return 59 // PageUp
        case 76: return 54 // Delete
        case 77: return 62 // End
        case 78: return 60 // PageDown
        case 79: return 56 // Right
        case 80: return 55 // Left
        case 81: return 57 // Down
        case 82: return 58 // Up
        case 83: return 75 // NumLock
        case 84: return 113 // NumpadDivide
        case 85: return 114 // NumpadMultiply
        case 86: return 115 // NumpadSubtract
        case 87: return 116 // NumpadAdd
        case 88: return 117 // NumpadEnter
        case 89: return 103 // Numpad1
        case 90: return 104 // Numpad2
        case 91: return 105 // Numpad3
        case 92: return 106 // Numpad4
        case 93: return 107 // Numpad5
        case 94: return 108 // Numpad6
        case 95: return 109 // Numpad7
        case 96: return 110 // Numpad8
        case 97: return 111 // Numpad9
        case 98: return 102 // Numpad0
        case 99: return 112 // NumpadDot
        case 100: return 73 // World2
        case 101: return 124 // Menu
        case 103: return 118 // NumpadEquals
        case 104: return 90 // F13
        case 105: return 91 // F14
        case 106: return 92 // F15
        case 107: return 93 // F16
        case 108: return 94 // F17
        case 109: return 95 // F18
        case 110: return 96 // F19
        case 111: return 97 // F20
        case 112: return 98 // F21
        case 113: return 99 // F22
        case 114: return 100 // F23
        case 115: return 101 // F24
        case 127: return 127 // Mute
        case 128: return 125 // VolumeUp
        case 129: return 126 // VolumeDown
        case 156: return 119 // NumpadClear
        case 224: return 65 // LeftCtrl
        case 225: return 64 // LeftShift
        case 226: return 66 // LeftAlt
        case 227: return 67 // LeftSuper
        case 228: return 121 // RightCtrl
        case 229: return 120 // RightShift
        case 230: return 122 // RightAlt
        case 231: return 123 // RightSuper
        default: return 0
        }
    }
    static func logicalKey(_ name: String) -> UInt16 {
        guard let scalar = name.uppercased().unicodeScalars.first else { return 0 }
        switch scalar.value {
        case 65...90: return UInt16(scalar.value - 65 + 19)
        case 48...57: return UInt16(scalar.value - 48 + 7)
        case 32: return 1 // Space
        case 39: return 2 // Quote
        case 44: return 3 // Comma
        case 45: return 4 // Minus
        case 46: return 5 // Period
        case 47: return 6 // Slash
        case 59: return 17 // Semicolon
        case 61: return 18 // Equal
        case 91: return 45 // LeftBracket
        case 93: return 47 // RightBracket
        case 92: return 46 // Backslash
        case 96: return 48 // Grave
        default: return 0
        }
    }
}
