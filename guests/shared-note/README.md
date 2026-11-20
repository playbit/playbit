# Shared Note

Shared Note reads and writes `note.txt` in the current session’s Shared data.
`playbit-volumes.json` seeds that file on first access; reopening preserves edits,
renames and deletion. Session data synchronization is deferred in this increment.

```sh
pb run --release
```

Typing keeps a private draft. Save commits locally and reads back the committed
file; typing after Save remains in the draft. Pending saves disable Save. Errors
retain the draft and offer Reload. Normal Quit drains an accepted save and its
readback. Notes contain at most 8,192 UTF-8 bytes.

Live messages are separate ephemeral session channels. They do not synchronize
the saved file or grant access to automatic scopes.
