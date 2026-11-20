# Audio Piano

A small Rust guest demonstrating Playbit's asynchronous `Audio` capability,
immutable PCM `Sound` resources, independent `SoundInstance` playback, and master
volume. It is based on the earlier `api-audio.c` piano example.

Click a colored pad, or hold the pointer and drag across pads to play a scale.
Use Left/Right to select C4 through C5, Space to play the selected note, and
Up/Down or the −/+ buttons to change volume. Different notes and repeated notes
can overlap; a ninth playback replaces the oldest of the eight instances.

The guest waits for the matching `DataReady` event after `begin_open_audio`.
It then synthesizes and uploads one half-second, mono PCM16LE note per display
frame at 48 kHz, with a short attack and release to avoid clicks. Each note is
generated once and cached for audio retries. Eight sounds retain 384,000
PCM bytes in the guest and another 384,000 bytes in the host. No database or
network work is needed. Static idle controls request no display frames.

Build using a Playbit installation that provides this branch's audio API:

```sh
cd guests/audio-piano
pb run --release
```

`pb build --release` packages without opening a window and prints the bundle
path. Pass that printed `.pbapp` path to `pb run` to open an existing build.

The native host suspends output while the app is inactive. It releases the
execution's audio, sounds, playback instances, and any pending open operation
when the app closes. Opening or preparing sound errors show a Retry audio
button; a rejected note is reported and can be played again.

Synthesis, input, and headless native error/retry tests require no output device:

```sh
cargo test -p audio-piano --lib
```

The implementation targets the repository's <400 ms startup and <8 ms local
input budgets. Those timings and physical audio output require separate native
qualification; passing these tests does not establish them.
