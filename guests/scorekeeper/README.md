# Scorekeeper

Keep score for Alice and Bob at the Friday table. Each player’s +5 button adds
five points to their score. Reset starts the next game at zero for both players.

With Playbit developer tools that include the OCaml `Data.keys` binding,
run the following from the guest directory.

```sh
pb run
```

Give the other player read/write access to the session’s Shared volume, then
send them the session link. Each +5 records a separate award in the shared
`scores` key-value database. Both players see the same totals when they reopen
the session. Reset starts a new round. A late award from an older round stays
in that round.
