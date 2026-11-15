# KensMirror

A screen-mirroring receiver for Windows. Pick your PC under **Screen Mirroring** on an
iPhone or iPad and its screen appears in a window, with sound. Press **R** to record
both to an MP4 file.

No watermark, no trial pop-ups, no account. Free software under GPL-3.0-or-later.

> **Status: early.** It has been used with one iPhone on one Windows 10 PC. Expect rough
> edges, and please open an issue with your `kensmirror.log` when something fails.

## What works

- Screen mirroring: H.264 video and AAC-ELD audio
- Low delay: pictures are shown as soon as they are decoded, and the sound queue is kept
  near 60 ms instead of being allowed to grow
- Recording to MP4 (H.264 + AAC), in `Videos\KensMirror`
- Fullscreen, always-on-top, a window that follows the picture's shape
- Discovery through a built-in responder (so far only tried on a PC that also has
  Bonjour installed)

## What does not work yet

- Audio-only streaming (music without mirroring)
- H.265 streams
- PIN or password protection: any device on the same network can connect
- More than one device at a time
- Content protected by DRM, which no receiver of this kind can show

## Build

You need Windows 10 or 11 and [Rust](https://rustup.rs) 1.85 or newer with the MSVC
toolchain. Nothing else: the video decoder and both encoders are the ones Windows ships.

```
cargo build --release
```

The program is `target\release\kensmirror.exe`.

## Use

Start `kensmirror.exe`. A window opens and waits. On the phone, open Control Center,
tap **Screen Mirroring** and pick the name of your PC. Phone and PC must be on the same
network, and the network must be set to *Private* in Windows. If Windows Firewall asks,
allow the program on private networks.

| Key | Action |
| --- | --- |
| R | Start or stop recording |
| F or F11 | Fullscreen on and off |
| Esc | Leave fullscreen |
| T | Always on top, on and off |

A recording starts with the next picture after you press R and is saved when you press
R again, when the phone disconnects or when you close the window. Rotating the phone
starts a new file, because an MP4 video track has one size.

### Options

| Option | Meaning | Default |
| --- | --- | --- |
| `--name <text>` | The name shown on the phone | The computer's name |
| `--bind <ip>` | The address to listen on | The adapter of the default route |
| `--port <n>` | The control port | 7000 |
| `--record` | Start recording as soon as a device connects | off |
| `--record-dir <dir>` | Where recordings go | `%USERPROFILE%\Videos\KensMirror` |
| `--audio-buffer-ms <n>` | How much sound to keep queued, 20 to 500 | 60 |
| `--data-dir <dir>` | Where the log and the identity file go | `%LOCALAPPDATA%\KensMirror` |
| `--keep-reorder` | Do not rewrite the stream's parameter set (see below) | off |
| `--debug` | Write more to the log | off |

Raise `--audio-buffer-ms` if the sound crackles on a busy Wi-Fi network; lower it for
less delay.

There is no console. Everything is written to `%LOCALAPPDATA%\KensMirror\kensmirror.log`.

## How the delay is kept low

- **No reordering wait.** A phone's mirror stream never reorders pictures, but its
  parameter set does not say so, and the Windows decoder then holds several pictures
  back before showing the first. KensMirror adds that declaration to the parameter set
  before decoding (`src/sps.rs`). Nothing else in the stream is touched.
- **Newest picture first.** If pictures arrive faster than the window can draw them, the
  older ones are skipped rather than queued.
- **A sound queue with a target.** Sound that arrives late after a network stall is
  played two percent fast until the queue is back at its target, and dropped outright
  after a long stall, so one hiccup does not become delay for the rest of the session.

## Self-tests

The program can run its own decoders, sound queue and recorder on recorded streams,
which is how changes are checked without a device:

```
kensmirror --data-dir out --selftest-video stream.h264 picture.bmp 100
kensmirror --data-dir out --selftest-audio audio.bin 10 0 600
kensmirror --data-dir out --record-dir out --selftest-record stream.h264 audio.bin 20
```

Results are written to `out\selftest.log`.

## Layout

| File | Purpose |
| --- | --- |
| `src/main.rs` | Options, logging, start-up and shutdown |
| `src/engine.rs` | Connects the protocol library to the decoders |
| `src/video.rs` | H.264 decoding (Media Foundation) |
| `src/sps.rs` | The parameter-set rewrite described above |
| `src/audio.rs` | AAC-ELD decoding and the sound queue |
| `src/recorder.rs` | MP4 recording (Media Foundation sink writer) |
| `src/paint.rs` | Colour conversion and scaling |
| `src/window.rs` | The window and its keys |
| `src/selftest.rs` | The self-tests |

## License and credits

KensMirror is free software: you can redistribute it and/or modify it under the terms
of the GNU General Public License as published by the Free Software Foundation, either
version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

The protocol is handled by the [rust-airplay](https://crates.io/crates/rust-airplay)
library. The libraries KensMirror is built on are listed in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

KensMirror is an independent project. It is not affiliated with, endorsed by or
sponsored by Apple Inc. iPhone and iPad are trademarks of Apple Inc.
