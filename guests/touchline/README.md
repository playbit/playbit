# Touchline

Play three minutes of seven-a-side soccer at North Quay. Portside wear coral and
attack the goal on the right. Rovers wear blue and attack the goal on the left.
The marker above a player and the ring under their boots identify your shirt.

| Control | Action |
| --- | --- |
| Arrow keys | Run in the direction shown on screen. |
| Click or tap the grass | Run to that point. Drag to change course. |
| Tap Space with the ball | Pass. Face the teammate you want to find. If an AI teammate receives your pass, you take control of them. |
| Hold Space with the ball | Shoot. Steer to aim or release the arrows to shoot across the keeper. |
| Tap Space without the ball | Tackle a nearby opponent. From farther away, switch to an available teammate near the ball. |
| Run into a loose ball | Collect it, including a pass you can intercept. |

The hint at the bottom of the window changes with possession. You can tap or
hold it, or open the pitch’s context menu to choose an action. At full time,
press Space or click the result panel to start another match with the same group.

## Start a match

Use the Playbit client and OCaml SDK from this checkout. From this directory, run

```sh
pb run --release
```

Use the session ID printed by `pb run` with `pb session volumes SESSION` to
find the volume whose role is `session`. Grant each friend's Playbit account
read access and send them the session link from the window title bar.

```sh
pb volume grant SHARED_VOLUME_ID friend@example.com r
```

Read access is enough to play. If Playbit asks you to reopen the game after
changing permissions, rejoin the same session. Your friends open its link in
Playbit. Each arriving instance takes a shirt on the team with fewer people.
Ties go to Portside. AI plays every unoccupied shirt, including the keepers.
Closing an instance returns its player to AI. When you are the only human on
your team, you also take control of an outfield teammate who wins the ball.

Fourteen people can play at once. An additional participant sees “Game is full.
Start a new game session”. Run Touchline again to start a separate game. If
disconnected, you can play a local practice game and use **Reconnect** from
the pitch’s context menu.

## How it works

- `match.ml` runs one fixed step of soccer at a time. A slot with human input
  uses it. A slot without it follows the AI for its role. Players pass,
  intercept, shoot and take set pieces under the same rules.
- `session.ml` uses Playbit presence and a session-scoped channel. One instance
  runs the shared match and sends snapshots. Peers send their controls and
  predict their own movement between snapshots. Remaining instances retain
  the latest match for an authority handoff. `wire.ml` encodes the channel
  messages and checks snapshots before accepting them.
- `art.ml` draws the stands, turf and chalk once and composites depth-sorted
  figures into a reusable RGBA buffer. `match_view.ml` arranges the stadium,
  scoreboard and pixel-font lettering on the window. `pixel_layout.ml` uses
  the display scale to size each texel to a whole square of physical pixels.
  `controls.ml` translates screen input into pitch coordinates.
- `app.ml` ties Playbit events to the simulation and the view. At full time,
  seated participants with write access record the same final score in the
  Shared `scorebook` under `last-result` as UTF-8 text. Match positions,
  presence and inputs are ephemeral.

With OCaml 5.3 installed, run `sh tests/run.sh` from this directory to exercise
gameplay and multi-participant behavior.
