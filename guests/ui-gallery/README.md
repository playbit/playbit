# UI gallery

The packaged gallery exercises the shared guest semantics through the default
`ui::desktop` GPUI renderer. Review 1 covers text fields, labels, regular and
prominent and destructive buttons, and off/on/mixed checkboxes. Review 2 adds
pop-up and right-click context menus; Review 3 adds segmented controls.
Review 4 adds overlay scrollbars to the guest viewport.
Every enabled control responds to
input; disabled examples remain inert.

From the repository root, build the guest and macOS host:

```sh
cargo build --locked --release -p ui-gallery --target wasm32-unknown-unknown
sh playbit/platform/macos/package.sh release
cargo test --locked --release -p ui-gallery --test controls
```

Run `pb` from this package with a **dedicated** `PLAYBIT_PROFILE` directory and
volume. `pb run --release --volume ID` uses the gallery's source manifest and
keeps watching it; leave the watcher running while reviewing the window. The
manifest's `volume` key must contain the same isolated volume ID during that
source run. Restore it afterward. To update an existing isolated gallery
session without changing its link, use `pb run --release --session ID`.
Do not use a shared default Playbit profile.

At 2× display scale, compare the gallery with the Figma UI Library frame.
The View menu provides Zoom In (⌘+ or ⌘=), Zoom Out (⌘−), and Actual Size
(⌘0). Zoom changes the Playbit UI scale for every open window. Control edges,
text starts, and baselines snap to physical pixels at each scale.

Try typing into an editable field, selecting and copying the read-only value,
then attempting to type or paste into it. The disabled field should not take
focus. Buttons respond to a pointer release inside and to Space or Return;
dragging out or pressing Escape cancels activation. The nine enabled checkboxes
toggle between off and on; an initially mixed checkbox changes to on on its
first activation. Only guest state can set mixed. Four squares below the
checkboxes use Display P3 red, green, yellow, and magenta at full intensity to
expose accidental sRGB clamping. The action line reports button,
checkbox and submitted-field actions. Tab and Shift-Tab navigate enabled
controls. Test clipboard, undo/redo and an IME in the editable fields.

For Review 2, press the enabled pop-up to open its menu. A quick release keeps
it open; a held press can drag to an item and release to choose it. Pointer
down and up on the same enabled row selects it. Escape or an outside pointer
down dismisses the menu. The disabled pop-up stays inert. Right-click the
context target to open a separate menu, then use More to try a nested action.
Unavailable entries cannot dispatch. While either menu is open, controls and
shortcuts in a second Playbit window should not respond. The action line and
selected pop-up label show the result.

For Review 3, switch between Slow, Medium, and Fast. Tab treats a segmented
control as one stop; Left/Right changes its selected option and stops at either
end. The five-choice group skips its disabled Slow option. The final group is
fully disabled. Pointer down followed by release outside the same segment
cancels; pointer selection preserves the existing keyboard focus. The action
line acknowledges each changed selection. Accessibility exposes a radio group
with one selected child. The selected highlight stretches toward the new option
over 240ms; try rapidly reversing direction. Resting selected and pressed corners
are 4dp. macOS Reduce Motion skips the decorative transition.

After Review 3 feedback, scroll bars come next, followed by Visual bell. The
scroll-bar design is on Figma v3 at node `4454:114688`; Visual bell is
`4474:8314`. Production `--ui-script` support remains deferred until the UI
library is done.

Each editable field accepts at most 4,096 UTF-8 bytes. The gallery stores only
temporary guest state. The ignored runtime diagnostic measures Engine input
through CPU presentation for 1,000 alternating field/checkbox events with
32-byte and 4-KiB text; it excludes host IPC, layout and displayed pixels:

```sh
cargo test --locked --release -p ui-gallery --test controls benchmark_basic_controls -- --ignored --nocapture
```

The iOS host uses the same guest semantics with UIKit rendering. The simulator
check remains available with `SIMULATOR_ID=<UDID> sh playbit/platform/ios/qualify.sh simulator ui-gallery`.

## Scrollbar review

Scroll or resize the window: any change in scroll position or extent reveals
the overlay scrollbars for two seconds, followed by a 200ms fade to hidden.
Scrolling or hovering during the fade restores full opacity. Dragging and macOS
“Always” keep the bars visible. While visible, hover either rail to expand both
thumbs and reveal the hovered track; drag a thumb (including outside the viewport) or click a track to jump.
The rails disappear when content fits and leave room for one another when both
axes overflow. Scrolling keeps keyboard focus in the current text field.
Try Actual Size and 125% zoom, and resize the window to exercise both axes.

The native diagnostic also exercises complex-background contrast and content
resize/removal, without activating its window:

```sh
cargo run --release -p playbit --example check-scrollbars
```

The `playbit/benchmarks/check-scrollbars.swift` driver targets a specific
process and leaves the active app in place.

### Scrollbar feedback update

Automatic bars can be hovered only while revealed by scrolling or an extent
change. Hidden bars do not intercept pointer input. With macOS **Show scroll
bars: Always**, the thumbs remain visible at their expanded width; tracks still
appear only on hover. Keyboard focus scrolls a clipped control into view with
an 8dp margin where space permits, including nested clips.

The gallery's scroll view takes the space available in the window and contains
all controls and a 512×341dp photo. Resize the window and scroll over the photo
to check the complex-background thumb treatment. `assets/scroll-photo.jpg` is the
user-owned photo supplied for this gallery (original filename `2013-10-30 11.48.21.jpg`).
Its sRGB pixels are decoded off the UI thread and converted to Display P3.
The guest scene buffer is 128 KiB; individual JPEG payloads are limited to
128 KiB and decoded images to one million pixels. Backdrop blur remains deferred.

### Dragging and trackpad behavior

A pressed scrollbar retains its track, width and pressed thumb until pointerup,
including outside the viewport and outside the window. Wheel events move only the
deepest viewport that can reveal content in their direction. A trackpad gesture
chooses its viewport on the first moving event and keeps that owner through
momentum; reaching an edge stretches and rebounds instead of moving both views.
Starting a new gesture at an inner edge can scroll the parent.

Scrollbar centers follow `max(viewport corner radius, 8dp)`. The square gallery
viewport therefore uses 8dp; window scrollbars follow the unscaled 16dp window
corners. The macOS elastic renderer uses private AppKit elasticity functions
(the same SPI used by WebKit); guest view/input semantics do not change.
