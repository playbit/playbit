# Shared control assets

`status-sync.svg`, `status-plug.svg`, and `status-connection.svg` are the
16 × 16 Figma glyph exports of `4542:11529`, `4546:17666`, and
`4546:17715` in file `5SKbaHPVS2qJ99f6UHN9M1`. The Sessions status pill renders
them at 16 logical points through the shared GPUI asset source. They are
decorative; the accompanying polite live text communicates the activity.
The sync and plug exports' group opacity is removed so GPUI applies the same
80% black tint once to all three glyphs and their labels. Vector paths are unchanged.

App icons live in `playbit/platform/macos/`: `icon.icon` for release builds,
`icon-debug.icon` for debug builds, and `icon-guest.icon` for Mac guest apps
without custom artwork. Both Apple hosts compile the shared Icon Composer
sources with Xcode. The Mac package includes the compiled guest assets so
launching a guest does not require Xcode or compile artwork.

`check-on.svg` is the unmodified Figma export for the Controls check glyph
(`292:232117`). Its parent component uses a 16 × 16 logical-point box. The
check export includes its stroke bounds: 9.91421 × 7.62132 at
(3.142893, 4.392893) within that box. Hosts supply the foreground color and
disabled/pressed opacity.

`volume.png` is the unmodified 2× sRGB export of the 16 × 16 default volume
component (`4314:74575`). Render it at 16 points as a color image. The PNG
preserves Figma's color and blend modes unsupported by the SVG renderer;
it is decorative and does not communicate volume permissions or sync state.
The current Mac sidebar uses this common default, without per-volume colors.

`entry-type.svg` traces the browser design's diamond type glyph.
The browser renders its glyphs at 16 points with the navigation foreground color.

`InterVariable.ttf` is the unmodified Inter 4.1 variable font from
[the Inter source release](https://github.com/rsms/inter/tree/v4.1),
`docs/font-files/InterVariable.ttf` at tag `v4.1`. Its family name is
`Inter Variable`. Desktop and web control
rendering use the bundled face and glyphs, without fetching Figma or reading
individual files in an input callback. iOS uses native UIKit typography and
system symbols instead.

`JetBrainsMonoNL-Medium.ttf` is the JetBrains Mono NL Medium face used by the
session log design, distributed under `JetBrainsMono-OFL.txt`.

The following glyphs use the 16 × 16 SVGs from the icons-1.5 set.
Asset names remain specific to their controls; hosts supply foreground colors
and control-state opacity. The popup glyph uses a full 16-point slot.

| Asset | icons-1.5 source |
| --- | --- |
| `add.svg` | `plus.svg` |
| `check-mixed.svg` | `minus.svg` |
| `entry-directory.svg` | `folder.svg` |
| `entry-file.svg` | `file.svg` |
| `entry-kv-db.svg` | `table-1d.svg` |
| `entry-object-db.svg` | `table-2d.svg` |
| `menu-avatar.svg` | `avatar.svg` |
| `menu-copy.svg` | `copy.svg` |
| `menu-exit-right.svg` | `exit-east.svg` |
| `menu-open-book.svg` | `open-book.svg` |
| `menu-signature.svg` | `signature.svg` |
| `menu-talk.svg` | `talk.svg` |
| `popup-glyph.svg` | `pop-up.svg` |
| `session-clear.svg` | `trash.svg` |
| `session-icon.svg`, `session-share.svg`, `sidebar-session.svg` | `session.svg` |
| `session-overflow.svg` | `drop-down.svg` |
| `session-start.svg` | `play.svg` |
| `sidebar-volume.svg` | `volume.svg` |
| `progress-spinner.svg` | `progress-spinner.svg` |
| `upgrade.svg` | `upgrade.svg` |

The other glyphs are unmodified exports. `progress-spinner.svg` embeds its
gradient as a PNG so GPUI can render it as a tinted mask. Sign-in and app-launch
progress share this asset and rotate it at display cadence while visible.

`menu-avatar.svg`, `menu-signature.svg`, `menu-copy.svg`, `menu-exit-right.svg`,
`menu-open-book.svg`, and `menu-talk.svg` serve the account menu. Menus render
these decorative symbols at 16dp with the row foreground color. Icons reserve
24dp within their divider-separated section; checkmarks reserve their own column
across the menu, including when all checkable actions are unchecked.
