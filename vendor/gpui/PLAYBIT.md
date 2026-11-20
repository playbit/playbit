# Playbit GPUI source

This directory vendors the published `gpui` 0.2.2 crate, not a new renderer.
The registry source was checked byte-for-byte against the published crate archive
before applying the changes below.

- Archive SHA-256: `979b45cfa6ec723b6f42330915a1b3769b930d02b2d505f9697f8ca602bee707`.
- Published `.cargo_vcs_info.json`: Zed commit
  `69e2130295c2649963eb639fc70b4f2ee8ea1624`, `dirty: true`, path `crates/gpui`.
  The archive is the source identity; the commit alone does not reproduce it.
- License: [Apache 2.0](LICENSE-APACHE), preserved with upstream source notices.
- The normalized published `Cargo.toml` is authoritative. `Cargo.toml.orig`
  is retained for provenance and requires the upstream Zed workspace.
- Registry extraction markers and the upstream standalone `Cargo.lock` are
  omitted. The application's root lockfile resolves this vendored package.

The workspace has one crates.io path override for GPUI. The package is also a
workspace member (CI tests it with `--no-default-features --features font-kit`),
so its `default` feature set is reduced to `font-kit`: with upstream's default
`wayland`/`x11`, a `--workspace` build unified those features into
`archipelago-app` and compiled blade and the Wayland/X11 helpers on macOS.
Preserve all upstream platforms, shaders and instance-buffer cleanup. Do not edit a machine's Cargo
registry to change production behavior.

## Removed from upstream

- `http_client` (`gpui_http_client`, a reqwest fork with hyper, h2 and a second
  TLS stack). It served only URI image loading: `Resource::Uri`, `SharedUri`,
  `ImageCacheError::BadStatus`, `App::http_client`/`set_http_client`,
  `Application::with_http_client` and the `image`/`image_gallery` examples are
  gone with it. `img(&str)` now always names an embedded asset; files go through
  `Resource::Path`. The Linux clients take `url::Url` directly.
- `image` default formats. Only the formats `ImageFormat` decodes are enabled:
  bmp, gif, jpeg, png, tiff, webp. This drops the AV1 encoder (`ravif`/`rav1e`),
  OpenEXR, qoi, hdr, pnm, dds, tga, farbfeld and `rayon`. SVG is unaffected
  (`resvg`/`usvg`).
- `naga` and the `check_wgsl_shaders` build step. It only validated the blade
  (WGSL) shaders, which the Metal renderer never uses, and it was declared as a
  macOS build-dependency while being compiled out on macOS.

## Local changes

macOS windows expose a mutable native minimum content size. Updates defer AppKit
mutation until the GPUI window borrow ends and grow undersized windows without
shrinking them when constraints relax. Test windows can simulate backing-scale
changes through their existing resize callback for UI zoom qualification.


Opt-in `playbit-trace` observations record local window, input, accessibility,
layout and text work, with explicit scene build references. Static stage names
and numbers carry no scene contents, account identity, pixels or native addresses.

Scene build and presentation references remain local observations. The recorder
keeps independent events, without implicit parents, global causal certification
or trace observer window ownership. See [event recording](../../playbit/trace/README.md).

The macOS completed frame projects existing control semantics through AccessKit,
with bounded nodes/text, stable entity identities and native actions routed to
the latest active controls. Publication coalesces per window and raises native
notifications outside GPUI window locks. Projection failure exposes an explicit
error instead of an incomplete native control tree.

Mac dirty-window notifications now request one coalesced application preparation
task. It builds at most one scene per executor turn, rotates across registered
windows, and stops when they are clean. It does not present or advance animation;
the existing display callback retains those responsibilities and can still build
a dirty/platform-forced frame. Tests use this same scheduling behavior instead
of drawing all dirty windows inline during effect flushing. The test platform
can explicitly deliver a frame callback to verify that separation.

`gpui.input.dispatch` and `gpui.accessibility.action` delimit native software
input receipt; `gpui.scene.preparation_delivery` observes the queued preparation
task. Local layout and text stages share the recording gate and bounded
producer queues. Independent events and timestamps do not certify complete
input-to-display paths. No tracing configuration changes layout or text behavior.
The host's explicit checkbox minimum uses its existing surface and padding
dimensions to avoid automatic minimum-content probes.

The desktop host uses existing `AnyView::cached` for checkbox subtrees. Intrinsic
bounds come from the same text style and shaping system, without another width
cache. Host tests compare uncached geometry and verify sibling reuse, state
invalidation and resize invalidation. The inline metadata size test here covers
cache/key/style structs only; it does not claim an allocator or total-heap bound.

While recording, Mac invalidators retain the first dirty timestamp
(8 bytes/window) until a draw consumes it. Stopped recording avoids the clock
read. `gpui.scene.dirty_ready` links that timestamp to the actual build;
the first construction draw uses `gpui.scene.initial_dirty`. Notifications do
not restart the timestamp, and clean forced draws emit no dirty-work sample.
These observations separate useful draw admission from delivery of a queued
preparation callback which another draw path may have made redundant.
