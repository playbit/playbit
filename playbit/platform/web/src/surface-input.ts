// The key order is the dense PBKey enum in playbit/sys/abi.h.
const keyNames = "None Space Quote Comma Minus Period Slash 0 1 2 3 4 5 6 7 8 9 Semicolon Equal A B C D E F G H I J K L M N O P Q R S T U V W X Y Z LeftBracket Backslash RightBracket Grave Escape Enter Tab Backspace Insert Delete Left Right Down Up PageUp PageDown Home End CapsLock LeftShift LeftCtrl LeftAlt LeftSuper MediaNext MediaPrev MediaPlay MediaStop World1 World2 ScrollLock NumLock PrintScreen Pause F1 F2 F3 F4 F5 F6 F7 F8 F9 F10 F11 F12 F13 F14 F15 F16 F17 F18 F19 F20 F21 F22 F23 F24 Numpad0 Numpad1 Numpad2 Numpad3 Numpad4 Numpad5 Numpad6 Numpad7 Numpad8 Numpad9 NumpadDot NumpadDivide NumpadMultiply NumpadSubtract NumpadAdd NumpadEnter NumpadEquals NumpadClear RightShift RightCtrl RightAlt RightSuper Menu VolumeUp VolumeDown Mute Fn".split(" ");
const keys = new Map(keyNames.map((name, index) => [name, index]));
const legacyKeys = new Map(["Left", "Right", "Up", "Down", "Space"].map((key, i) => [keys.get(key)!, i + 1]));

// Released named-canvas guests expect the original 16-byte pointer and 8-byte key payloads.
export function legacyCanvasEvent(event: { kind: number; target: number; objectId?: number; data: Uint8Array },
                                  size?: { width: number; height: number }) {
    const view = new DataView(event.data.buffer, event.data.byteOffset, event.data.byteLength);
    if (event.kind === 23 && event.target && !view.getUint8(25)) {
        const key = legacyKeys.get(view.getUint16(22, true) || view.getUint16(20, true));
        if (!key) return;
        const data = new Uint8Array(8), payload = new DataView(data.buffer);
        payload.setUint32(0, key, true);
        payload.setUint32(4, view.getUint8(24), true);
        return { ...event, kind: 12, data };
    }
    if (event.kind === 22 && size && (view.getUint16(28, true) & 1)) {
        const phase = view.getUint8(24), button = view.getUint8(26), primary = view.getUint16(30, true) & 1;
        if (phase > 3 || (phase === 0 && !primary) || ((phase === 1 || phase === 2) && button !== 0)) return;
        const data = new Uint8Array(16), payload = new DataView(data.buffer);
        payload.setUint32(0, Math.round(Math.max(0, Math.min(size.width, view.getFloat32(48, true)))), true);
        payload.setUint32(4, Math.round(Math.max(0, Math.min(size.height, view.getFloat32(52, true)))), true);
        payload.setUint32(8, phase, true);
        payload.setUint32(12, primary, true);
        return { ...event, kind: 11, data };
    }
}

type Send = (kind: number, target: number, data: Uint8Array, motion?: boolean, reservation?: number, legacy?: boolean) => boolean;
type Point = { x: number; y: number };
type Contact = Point & { id: number; target: number; kind: number; flags: number; held: boolean; legacy: boolean; capture?: HTMLCanvasElement; canvasX: number; canvasY: number };

export function combineSurfaceMotion(kind: number, previous: Uint8Array, next: Uint8Array): void {
    const before = new DataView(previous.buffer, previous.byteOffset, previous.byteLength);
    const after = new DataView(next.buffer, next.byteOffset, next.byteLength);
    const start = kind === 22 ? 40 : 32;
    const count = kind === 22 ? 2 : kind === 24 ? 3 : 4;
    if (kind !== 22 && kind !== 24 && kind !== 25) return;
    for (let i = 0; i < count; i++) {
        const offset = start + i * 4;
        const a = before.getFloat32(offset, true), b = after.getFloat32(offset, true);
        after.setFloat32(offset, kind === 25 && i === 2 ? a * b : a + b, true);
    }
    if (kind === 22) next[28] |= 16;
}

export function encodeKey(code: string, logical: string): [number, number] {
    const physicalName = ({
        ArrowLeft: "Left", ArrowRight: "Right", ArrowUp: "Up", ArrowDown: "Down",
        Backquote: "Grave", BracketLeft: "LeftBracket", BracketRight: "RightBracket",
        ShiftLeft: "LeftShift", ShiftRight: "RightShift", ControlLeft: "LeftCtrl", ControlRight: "RightCtrl",
        AltLeft: "LeftAlt", AltRight: "RightAlt", MetaLeft: "LeftSuper", MetaRight: "RightSuper",
        NumpadDecimal: "NumpadDot", NumpadEqual: "NumpadEquals", ContextMenu: "Menu",
        IntlBackslash: "World1", IntlRo: "World2", IntlYen: "World2",
        MediaTrackNext: "MediaNext", MediaTrackPrevious: "MediaPrev", MediaPlayPause: "MediaPlay",
        AudioVolumeUp: "VolumeUp", AudioVolumeDown: "VolumeDown", AudioVolumeMute: "Mute",
    } as Record<string, string>)[code] ?? code.replace(/^(Key|Digit)(?=[A-Z0-9]$)/, "");
    const physical = keys.get(physicalName) ?? 0;
    const logicalName = ({
        " ": "Space", "'": "Quote", ",": "Comma", "-": "Minus", ".": "Period", "/": "Slash",
        ";": "Semicolon", "=": "Equal", "[": "LeftBracket", "]": "RightBracket",
        "\\": "Backslash", "`": "Grave", ArrowLeft: "Left", ArrowRight: "Right", ArrowUp: "Up", ArrowDown: "Down",
    } as Record<string, string>)[logical] ?? (/^[a-z]$/.test(logical) ? logical.toUpperCase() : logical);
    return [keys.get(logicalName) ?? (Array.from(logical).length === 1 ? 0 : physical), physical];
}

function modifiers(event: MouseEvent | KeyboardEvent): number {
    return Number(event.shiftKey) | (Number(event.ctrlKey) << 1) | (Number(event.altKey) << 2)
        | (Number(event.metaKey) << 3) | (Number(event.getModifierState("CapsLock")) << 4)
        | (Number(event.getModifierState("Fn")) << 5);
}
function packet(length: number, device: number, timestamp: number, flags: number) {
    const bytes = new Uint8Array(length), view = new DataView(bytes.buffer);
    view.setBigUint64(0, BigInt(Math.round(Math.max(0, timestamp) * 1e6)), true);
    view.setUint32(12, device, true);
    view.setUint16(16, flags, true);
    const floats = (offset: number, ...values: number[]) => values.forEach((v, i) => view.setFloat32(offset + 4 * i, v, true));
    return { bytes, view, floats };
}

export function observeSurface(
    root: HTMLElement, send: Send, logicalScale: () => number,
    canvasSize?: (canvas: HTMLCanvasElement) => { width: number; height: number } | undefined,
) {
    let mask = 0;
    let legacyKeyboard = false;
    let nextPointer = 1;
    const contacts = new Map<number, Contact>();
    const heldKeys = new Map<string, { key: [number, number]; target: number }>();
    const controllers = new Map<number, { id: number; buttons: boolean[]; axes: number[] }>();
    let nextController = 16, frame = 0;
    const point = (event: MouseEvent): Point => {
        const bounds = root.getBoundingClientRect(), scale = logicalScale();
        return { x: (event.clientX - bounds.left) / scale, y: (event.clientY - bounds.top) / scale };
    };
    const belongs = (event: Event) => event.target instanceof Node && root.contains(event.target);
    const gesturePoints = new Map<number, Point>();
    let activeGestures = 0;
    function gestures(before: Point[], after: Point[], timestamp: number, flags: number) {
        if (before.length < 2 && after.length < 2) return;
        const phase = before.length < 2 ? 1 : after.length < 2 ? 2 : 0;
        const [a, b] = phase === 2 ? before : after, [c, d] = phase === 0 ? before : [a, b];
        const x = (a.x + b.x) / 2, y = (a.y + b.y) / 2;
        const dx = x - (c.x + d.x) / 2, dy = y - (c.y + d.y) / 2;
        const distance = Math.hypot(d.x - c.x, d.y - c.y);
        const scale = distance ? Math.hypot(b.x - a.x, b.y - a.y) / distance : 1;
        const angle = Math.atan2(b.y - a.y, b.x - a.x) - Math.atan2(d.y - c.y, d.x - c.x);
        const rotation = Math.atan2(Math.sin(angle), Math.cos(angle));
        for (let kind = 0; kind < 3; kind++) {
            const bit = 1 << kind;
            if (phase !== 1 && !(activeGestures & bit)) continue;
            const item = packet(48, 2, timestamp, flags);
            item.view.setUint8(20, kind); item.view.setUint8(21, phase);
            item.floats(24, x, y, kind === 0 ? dx : 0, kind === 0 ? dy : 0, kind === 1 && scale > 0 ? scale : 1, kind === 2 ? rotation : 0);
            if (send(25, 0, item.bytes, phase === 0, phase === 1 ? 1 : phase === 2 ? -1 : 0)) {
                if (phase === 1) activeGestures |= bit;
                if (phase === 2) activeGestures &= ~bit;
            }
        }
    }
    function pointerPacket(contact: Contact, phase: number, time: number, mods: number, event?: PointerEvent, previous?: Contact) {
        const item = packet(contact.kind === 4 ? 92 : 56, contact.kind, time, mods);
        const { view } = item;
        view.setUint32(20, contact.id, true);
        view.setUint8(24, phase); view.setUint8(25, contact.kind);
        // A legacy drag ends when the primary button lifts, even during a chord.
        const button = contact.legacy && phase === 2 ? 0 : event?.button ?? -1;
        const changed = button === 1 ? 2 : button === 2 ? 1 : button;
        view.setUint8(26, (phase === 1 || phase === 2) && changed >= 0 && changed < 16 ? changed : 255);
        view.setUint8(27, Math.min(event?.detail ?? 0, 255));
        view.setUint16(28, contact.flags, true);
        const buttons = contact.held ? event?.buttons ?? 0 : 0;
        view.setUint16(30, buttons & 0xffff, true);
        item.floats(32, contact.x, contact.y, previous ? contact.x - previous.x : 0,
            previous ? contact.y - previous.y : 0, contact.canvasX, contact.canvasY);
        if (contact.kind === 4) {
            const azimuth = event?.azimuthAngle ?? 0;
            item.floats(56, event?.pressure ?? 0, event?.tangentialPressure ?? 0,
                (event?.tiltX ?? 0) / 90, (event?.tiltY ?? 0) / 90, event?.twist ?? 0,
                event?.width ?? 0, event?.height ?? 0, Math.min(event?.altitudeAngle ?? 0, Math.PI / 2),
                azimuth >= Math.PI * 2 - 0.000001 ? 0 : azimuth);
        }
        const reservation = Number(contact.held) - Number(previous?.held ?? false);
        return send(22, contact.target, item.bytes, phase === 0, reservation, contact.legacy);
    }
    function pointer(event: PointerEvent, phase: number) {
        if (!(mask & 9)) return;
        const old = contacts.get(event.pointerId);
        const observesPointer = belongs(event) || old?.held;
        if (!observesPointer && !gesturePoints.has(event.pointerId)) return;
        if ((phase === 2 || phase === 3) && !old?.held && !gesturePoints.has(event.pointerId)) return;
        if (phase === 5 && old?.held) return;
        if ((phase === 4 || phase === 5) && event.target !== root) return;
        const kind = event.pointerType === "touch" ? 2 : event.pointerType === "pen" ? 4 : 1;
        const p = point(event), mods = modifiers(event);
        if ((mask & 8) && kind === 2 && (phase === 1 || gesturePoints.has(event.pointerId))) {
            const before = Array.from(gesturePoints.values()).slice(0, 2);
            if (phase === 2 || phase === 3) gesturePoints.delete(event.pointerId);
            else if (gesturePoints.size < 16 || gesturePoints.has(event.pointerId)) gesturePoints.set(event.pointerId, p);
            gestures(before, Array.from(gesturePoints.values()).slice(0, 2), event.timeStamp, mods);
        }
        if (!(mask & 1) || !observesPointer) return;
        if (old?.held && old.legacy && phase === 0 && !(event.buttons & 1)) phase = 2;
        if (!old && contacts.size >= 16) return;
        const node = event.target instanceof Element ? event.target.closest<HTMLCanvasElement>("canvas[data-control]") : null;
        const canvas = node ?? (old?.target ? root.querySelector<HTMLCanvasElement>(`canvas[data-control="${old.target}"]`) : null);
        const bounds = canvas?.getBoundingClientRect(), scale = logicalScale();
        const size = canvas && canvasSize?.(canvas);
        const held = phase === 1 || (old?.held === true && ![2, 3, 5].includes(phase));
        const contact = { ...p, id: old?.id ?? nextPointer++, kind, target: old?.held ? old.target : Number(canvas?.dataset.control ?? 0), held,
            legacy: old?.held ? old.legacy : legacyKeyboard,
            capture: held ? (old?.held ? old.capture : canvas ?? undefined) : undefined,
            flags: Number(event.isPrimary) | ((kind !== 1 && held && event.buttons !== 0) ? 2 : 0)
                | (kind === 4 && (event.button === 5 || event.buttons & 32) ? 4 : 0)
                | ((event.getCoalescedEvents?.()?.length ?? 0) > 1 ? 16 : 0),
            canvasX: bounds ? (event.clientX - bounds.left) * (size && bounds.width ? size.width / bounds.width : 1 / scale) : old?.canvasX ?? 0,
            canvasY: bounds ? (event.clientY - bounds.top) * (size && bounds.height ? size.height / bounds.height : 1 / scale) : old?.canvasY ?? 0 };
        if (pointerPacket(contact, phase, event.timeStamp, mods, event, old)) {
            if (phase === 1 && canvas) {
                try { canvas.setPointerCapture(event.pointerId); } catch { /* The page still observes uncaptured input. */ }
                canvas.focus({ preventScroll: true });
            }
            if (phase === 2 || phase === 3 || phase === 5) {
                contacts.delete(event.pointerId);
                if (old?.legacy && old.capture?.hasPointerCapture(event.pointerId)) {
                    old.capture.releasePointerCapture(event.pointerId);
                }
            } else contacts.set(event.pointerId, contact);
        }
    }
    function keyboardPacket(key: [number, number], target: number, pressed: boolean, time: number, mods: number, text = "", repeat = false) {
        const item = packet(60, 8, time, mods);
        item.view.setUint16(20, key[0], true); item.view.setUint16(22, key[1], true);
        item.view.setUint8(24, Number(pressed)); item.view.setUint8(25, Number(repeat));
        const chars = Array.from(text).slice(0, 8);
        item.view.setUint8(26, chars.length);
        chars.forEach((ch, i) => item.view.setUint32(28 + i * 4, ch.codePointAt(0)!, true));
        return send(23, target, item.bytes, repeat, repeat ? 0 : pressed ? 1 : -1, target !== 0);
    }
    const canvasTarget = (owner: EventTarget | null) => owner instanceof HTMLCanvasElement ? Number(owner.dataset.control ?? 0) : 0;
    function keyboard(event: KeyboardEvent, pressed: boolean) {
        if (!(mask & 2)) return;
        // A browser can name Fn while reporting no physical code for either key in a chord.
        const unknownCode = !event.code || event.code === "Unidentified";
        const keyId = unknownCode && event.key === "Fn" ? "Fn" : event.code;
        const old = heldKeys.get(keyId);
        const target = old?.target ?? (legacyKeyboard ? canvasTarget(event.target) : 0);
        const shortcut = event.altKey || event.ctrlKey || event.metaKey;
        if (pressed && ((!belongs(event) && event.target !== document.body) || event.isComposing)) return;
        if (pressed && legacyKeyboard && (!target || shortcut)) return;
        if (!pressed && !old) return;
        const key = old?.key ?? encodeKey(event.code, event.key);
        const text = pressed && Array.from(event.key).length === 1 ? event.key : "";
        if (pressed && !key[0] && !key[1] && !text) return;
        if (keyboardPacket(key, target, pressed, event.timeStamp, modifiers(event), text, pressed && !!old)) {
            if ((!legacyKeyboard || !shortcut) && event.target instanceof HTMLCanvasElement
                && (event.code.startsWith("Arrow") || event.code === "Space")) event.preventDefault();
            if (pressed && heldKeys.size < keyNames.length) heldKeys.set(keyId, { key, target });
            if (!pressed) heldKeys.delete(keyId);
        }
    }
    function finishKeys(time: number, target?: number) {
        for (const [code, held] of heldKeys) {
            if (target !== undefined && held.target !== target) continue;
            keyboardPacket(held.key, held.target, false, time, 0);
            heldKeys.delete(code);
        }
    }
    function releaseCaptures() {
        const held = Array.from(contacts);
        contacts.clear();
        for (const [id, contact] of held) {
            if (gesturePoints.has(id)) continue;
            if (contact.capture?.hasPointerCapture?.(id)) contact.capture.releasePointerCapture(id);
        }
    }
    function finish(families = mask) {
        const now = performance.now();
        if (families & 8) {
            gestures(Array.from(gesturePoints.values()).slice(0, 2), [], now, 0);
            gesturePoints.clear();
        }
        if (families & 1) {
            for (const contact of contacts.values()) pointerPacket({ ...contact, held: false, flags: contact.flags & ~2 },
                contact.held ? 3 : 5, now, 0, undefined, contact);
            releaseCaptures();
        }
        if (families & 2) finishKeys(now);
    }
    function controllerEvent(id: number, phase: number, control: number, value: number, x = 0, y = 0, time = performance.now()) {
        const event = packet(36, id, time, 0);
        event.view.setUint8(20, phase); event.view.setUint16(22, control, true);
        event.floats(24, value, x, y);
        return send(26, 0, event.bytes, phase === 4, phase === 0 ? 1 : phase === 1 ? -1 : 0);
    }
    function sampleGamepads(time: number) {
        frame = 0;
        if (!(mask & 16) || document.hidden || !document.hasFocus()) return;
        const devices = (navigator.getGamepads?.() ?? []).slice(0, 8);
        for (const [index, gamepad] of controllers) {
            if (!devices.some(device => device?.index === index) && controllerEvent(gamepad.id, 1, 0, 0)) controllers.delete(index);
        }
        for (const device of devices) {
            if (device?.mapping !== "standard") continue;
            let state = controllers.get(device.index);
            if (!state) {
                if (!controllerEvent(nextController, 0, 0, 0)) continue;
                controllers.set(device.index, state = { id: nextController++, buttons: [], axes: [] });
            }
            for (const [control, index] of [0, 1, 2, 3, 4, 5, 6, 7, 10, 11, 9, 8, 16].entries()) {
                const button = device.buttons[index], pressed = !!button?.pressed;
                if (pressed !== !!state.buttons[control] && controllerEvent(state.id, pressed ? 2 : 3, control, button?.value ?? 0)) state.buttons[control] = pressed;
            }
            const axes = [device.axes[0] ?? 0, -(device.axes[1] ?? 0), device.axes[2] ?? 0, -(device.axes[3] ?? 0),
                Number(!!device.buttons[15]?.pressed) - Number(!!device.buttons[14]?.pressed),
                Number(!!device.buttons[12]?.pressed) - Number(!!device.buttons[13]?.pressed),
                device.buttons[6]?.value ?? 0, device.buttons[7]?.value ?? 0];
            for (const [control, value] of axes.entries()) {
                const pair = control & ~1;
                if (value !== (state.axes[control] ?? 0) && controllerEvent(state.id, 4, control, value,
                    control < 6 ? axes[pair] : 0, control < 6 ? axes[pair + 1] : 0, time)) state.axes[control] = value;
            }
        }
        if (devices.some(device => device?.mapping === "standard")) frame = requestAnimationFrame(sampleGamepads);
    }
    function startGamepads() {
        if (frame || !(mask & 16) || document.hidden || !document.hasFocus()) return;
        if ((navigator.getGamepads?.() ?? []).some(device => device?.mapping === "standard")) frame = requestAnimationFrame(sampleGamepads);
    }
    function stopGamepads() {
        if (frame) cancelAnimationFrame(frame);
        frame = 0;
        for (const gamepad of controllers.values()) controllerEvent(gamepad.id, 1, 0, 0);
        controllers.clear();
    }
    for (const [event, phase] of [["pointermove", 0], ["pointerdown", 1], ["pointerup", 2],
        ["pointercancel", 3], ["pointerenter", 4], ["pointerleave", 5], ["lostpointercapture", 3]] as const) {
        window.addEventListener(event, e => pointer(e as PointerEvent, phase), true);
    }
    window.addEventListener("keydown", event => keyboard(event, true), true);
    window.addEventListener("keyup", event => keyboard(event, false), true);
    window.addEventListener("focusout", event => {
        const target = legacyKeyboard && canvasTarget(event.target);
        if (target) finishKeys(event.timeStamp, target);
    }, true);
    window.addEventListener("wheel", event => {
        if (!(mask & 4) || !belongs(event)) return;
        const item = packet(44, 1, event.timeStamp, modifiers(event)), p = point(event);
        item.view.setUint8(20, 1);
        item.view.setUint16(22, event.deltaMode === 1 ? 4 : event.deltaMode === 2 ? 8 : 0, true);
        const scale = event.deltaMode === 0 ? logicalScale() : 1;
        item.floats(24, p.x, p.y, event.deltaX / scale, event.deltaY / scale, event.deltaZ / scale);
        send(24, 0, item.bytes, true);
    }, { passive: true, capture: true });
    window.addEventListener("blur", () => { finish(); stopGamepads(); });
    window.addEventListener("focus", startGamepads);
    window.addEventListener("gamepadconnected", startGamepads);
    window.addEventListener("gamepaddisconnected", () => { if (!frame) sampleGamepads(performance.now()); });
    document.addEventListener("visibilitychange", () => {
        if (document.hidden) { finish(); stopGamepads(); } else startGamepads();
    });
    return {
        configure(next: number, legacy = false) {
            let removed = mask & ~next;
            if (legacyKeyboard !== legacy) removed |= mask & 3;
            if (removed & 11) finish(removed);
            if (mask & ~next & 16) stopGamepads();
            mask = next;
            legacyKeyboard = legacy;
            startGamepads();
        },
        reset() {
            if (frame) cancelAnimationFrame(frame);
            frame = mask = activeGestures = 0;
            legacyKeyboard = false;
            gesturePoints.clear(); releaseCaptures(); heldKeys.clear(); controllers.clear();
        },
    };
}
