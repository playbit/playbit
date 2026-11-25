# Input Lab

Input Lab shows the window input that Playbit delivers to a guest. Open it with
a Playbit build that provides `ui.input.v1`. From this directory, run

```sh
pb init .
pb run
```

`pb init .` prepares the local project on first use. `pb run` opens the guest
and watches the source. Use `pb build --release` to produce a package.

All five families start subscribed. Select a tab to inspect it, or use the
pop-up menu at the top of a narrow window. Each family
keeps its latest 16 events, including events received while another tab was
selected. **Freeze readout** takes a snapshot across all five tabs. **Older**
and **Newer** step through the snapshot. **Resume live** shows the events that
arrived meanwhile. Input is held in memory for the lifetime of the guest.

| Tab | Try | Look for |
| --- | --- | --- |
| Pointer | Hover, press two mouse buttons, drag off the pad, or use multiple fingers or a pen. | Contact ID, enter/leave/down/move/up/cancel, changed button and held mask. Window coordinates use the content origin. Canvas coordinates use the pad origin and can extend past its edge during a drag. Pen samples add pressure, tilt, twist, contact size and orientation. |
| Keyboard | Hold one key while pressing another, hold for repeat, then release. On an Apple keyboard, tap Fn, then hold it while pressing another key. Try a different layout or a non-ASCII character. | Logical and physical key codes, pressed/released/repeated transitions, modifiers and Unicode text. Look for **Fn down**, **Fn up** and the Fn modifier on the other key. The strip shows recent presses as tall marks, releases as short marks and repeats in amber. |
| Scroll | Scroll horizontally and vertically with a wheel or trackpad, then lift your fingers. | Three delta axes, units, precision, inversion, position and phase, including momentum when supplied by the host. |
| Gesture | Pan, pinch and rotate using a supported trackpad or touchscreen. | Begin, change and end events, center, translation, relative scale and clockwise rotation in radians. |
| Gamepad | Connect a controller, move both sticks and the D-pad, squeeze a trigger, press buttons, then disconnect. | Connection transitions, named controls, scalar values and paired axes. The crosshair shows the axis pair with positive Y up. |

Each sample identifies its originating window, target, client and device and
includes the monotonic timestamp in nanoseconds. Target 1 is the pad. Target 0
is window-scoped. Hardware and host capabilities determine which input details
are available.

To test the subscription boundary, hold a key and use the pointer to click
**Unsubscribe** on the Keyboard tab. Inspect the release that finishes the held
sequence. New key presses stop while the other tabs continue receiving input.
Click **Subscribe** to enable the family again.

## Automated checks

From the repository root, run

```sh
cargo test --release --locked -p input-lab --test window_input
```

The tests feed encoded events from all five families through the registered
native guest and inspect its submitted scenes, source metadata, subscription
mask and history controls. For an interactive CLI check, select Pointer and
use `pb input SESSION pointer 1 down 80 40` followed by
`pb input SESSION pointer 1 up 80 40`.
