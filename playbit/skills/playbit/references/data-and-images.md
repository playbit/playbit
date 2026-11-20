# Data and images

Start from the generated TODO's data operations and event loop. Read the
installed SDK source for signatures instead of guessing from other Rust UI
libraries. `playbit-api` is the guest API; engine/store implementation crates
are not app dependencies.

## Durable records

Volumes contain named entries. A database entry has a schema with stable type
and field IDs. Preserve existing IDs when extending it. The generated TODO
demonstrates resolving entries, reading/establishing schemas, querying objects,
creating records, editing fields, and reacting to `DataChanged`.

The initializer JSON describes a fresh root, not the live database. To change
both the running app and future copies, update the live schema through supported
operations and update the initializer accordingly. Inspect existing schemas
before changing them; retain fields and records the task does not replace.

Context handles belong to an instance/generation. Persist volume, entry, and
object IDs when needed, then resolve current handles after launch/reload. Never
save an `OperationHandle` or `VolumeHandle` as durable state.

## Attach and display an image

Use a file entry in a volume and a typed file value on the TODO object. Keep the
image bytes out of text fields and source code. `pb file-write VOLUME ENTRY PATH`
imports bytes into an existing file entry; it does not create that entry. A
fresh-volume initializer can declare file entries in `blobs` (15-byte hex `id`,
`name`, `bytes`, and optional `mtime`). To prepare a separate image volume, use
the bundled `playbit-mcp` tool's `initialize-root` operation with a fresh volume
ID and a declaration, then explicitly mount that volume. Inspect its `tools/list`
schema first. Do not reinitialize an existing volume to add a file.
These trusted tool operations are distinct from the guest's file APIs.

The SDK declares `begin_file_selection`, but the current native app host returns
`Unsupported`: a declaration is not proof of a working picker. For this
increment, import a test image through the trusted tool workflow, and let the
app select an already mounted volume and resolve a known file name with
`begin_resolve_entry` / `take_resolve_entry`. Label that flow clearly; it is not
a Mac file chooser. Cancel should leave the TODO unchanged. Store the selected
file's durable volume/entry identity using the schema's file field.

`ViewEncoder::file_image` is explicitly documented as not rendered by shipping
hosts. For this increment, support bounded JPEG attachments: read the volume
file asynchronously with `begin_read_file` / `take_read_file`, then display those
bytes with `ViewEncoder::jpeg_image`. Keep the durable file reference in the
record; the bytes in memory are only a bounded display buffer. Limit attachment
size, concurrent reads and total display memory, show unsupported formats/large
files clearly, and refresh on data changes. Respect host read and scene limits
and include meaningful alternative text. Do not claim arbitrary image formats
or a native file picker are supported.

File-copy declarations also require host support; do not promise them from SDK
signatures alone. A destination would need to be an existing authorized file
entry. Neither selection nor copying grants additional volume authority.

## Cooperative execution

Data operations use begin/take or begin/poll pairs. Retain the admitted handle,
yield, and collect on its matching readiness event. Bound pending operations and
query pages. `WouldBlock` means wait for readiness, not retry on a timer.
Keep rendering/input responsive during import or image loading and expose
failure without reporting missing data as an empty successful result.

For a TODO with images, demonstrate add/toggle, attach/display, picker cancel,
and reopening with both the TODO and attachment retained. Check an existing row
after hot reload. A successful build alone does not establish those behaviors.
