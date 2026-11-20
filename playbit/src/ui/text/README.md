# Text

Text is a portable Archipelago subsystem. It owns text presentation, selection,
and editing without assuming that the text lives in a conventional GUI. The
same layout must eventually be usable in a form field, on a transformed canvas,
or in a 3D scene.

The shipped Playbit implementation is the behavioral reference. In particular,
we will preserve its UTF-8 indexing, grapheme and word movement, shaped-glyph
hit testing, preferred-X vertical movement, and multi-click drag selection.

## Boundaries

`archipelago-text` contains platform-independent state and algorithms:

- a UTF-8 buffer;
- directional selections and multiple carets;
- editing commands and undo/redo;
- Unicode grapheme, word, and logical-line boundaries; and
- layout results expressed as glyph positions, caret stops, lines, and
  selection geometry.

It must not depend on GPUI, AppKit, a window, or a particular renderer.

The macOS application has a small GPUI adapter. It translates keyboard,
pointer, clipboard, and IME events into text commands and paints the resulting
layout. GPUI is the first host, not part of the text contract.

The first adapter may use GPUI's shaping and glyph painting while the editing
model is established. This is a bridge. Portable shaping and font handling will
use HarfBuzz and FreeType, following the current Playbit runtime, and will
replace the host-specific layout without changing editor behavior.

Archipelago's runtime renderer will eventually be a Rust compositor capable of
combining ordinary UI, text, and arbitrarily nested Dawn/WebGPU canvases. Text
layout must therefore produce renderer-neutral glyph, caret, and selection
geometry that can be transformed, clipped, blended, and masked like any other
graphics primitive. GPUI remains a shell and workbench adapter; its CoreText
layout and glyph painter are not part of the runtime or text contract.
