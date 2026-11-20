# Little List

A small task list backed by docdb. The role initializer declares a `docdb`
bloblog; the app imports its entries into a local document and appends edits
through docdb's sync helper.

Type a task and press Return to add it. Click its checkbox to mark it done or
undo completion. Tasks and their completion state are saved automatically.
The list loads when opened and refreshes on the engine's `DataChanged` events
from AP subscriptions. After reconnecting, the engine pulls missed bloblog entries
and notifies the open list when its data changes.

Run from the repository root:

```sh
./guests/little-list/run.py
```

The script finds `pb` on your PATH or in `target/debug/Playbit.app`. It requires
Python 3, Cargo, and the `wasm32-unknown-unknown` Rust target.

It calls `pb build` directly in this guest's directory, creates a session with
`pb session create`, then opens it with `pb session join`. It
reopens the same session on subsequent runs and keeps your tasks when the source
changes. Build output and local app data stay under this guest's `target` directory.

After opening, it follows guest logs in the terminal until the app exits.
Press Ctrl-C to stop following logs and leave Little List running.
