# gopro-import

**Browse, preview and import GoPro footage on a Mac — straight over the USB cable, no MTP, no Quik.**

A fast Rust CLI with a full-screen terminal UI: a date-grouped list view, a thumbnail
gallery, an in-terminal video player, and resumable multi-stream imports.
Built and tested with a HERO12 Black; should work with any HERO9 or newer in
*GoPro Connect* USB mode, over the camera's Wi-Fi, or from a mounted SD card.

```
gopro                 # open the TUI
gopro pull            # import everything new to ~/Movies/GoPro/YYYY-MM-DD/
gopro play GX010124   # stream a clip to VLC / IINA / mpv / QuickTime
```

## Why this exists

Plug a recent GoPro into a Mac and MacDroid, Android File Transfer and friends
fail with *"Device is being used by com.apple.driver.usb.cdc"*. That is because in
GoPro Connect mode the camera is not an MTP device at all: it enumerates as a USB
**network adapter**, which macOS's built-in CDC driver claims. The camera then runs
GoPro's own HTTP API (the [Open GoPro](https://gopro.github.io/OpenGoPro/) spec)
on that link at `172.2X.1YZ.51:8080`, where X Y Z are the last three digits of the
serial number.

`gopro` finds that link from `ifconfig`, talks to the API, and streams files
directly off the SD card. Nothing to install on the camera, no drivers.

## Features

- **TUI** — list view grouped by day with battery / card-space header, thumbnail
  gallery grid, preview pane with frame size, length, fps, HyperSmooth, audio.
  Real inline images in iTerm2, WezTerm, kitty and Ghostty; half-block graphics
  anywhere else (including tmux).
- **Video preview** — press `p` to play a clip in the terminal. ffmpeg decodes the
  camera's low-res proxy (`.LRV`, ~30× smaller than the 4K file) so playback starts
  instantly; `f` switches to the full-quality stream, `P` hands the same stream to a
  desktop player with sound.
- **Fast imports** — files are fetched with six parallel range requests written in
  place (≈ 2× a single connection on a HERO12). Interrupted transfers resume
  from per-segment checkpoints; already-imported files are skipped, so `pull` is a
  cheap "sync what's new". Every copy is size-verified and gets its original
  shot time as mtime.
- **Selection tools** — select one, all, or everything shot on a day; filter by name
  or by photos / videos; delete from the card after confirming.
- **Works offline too** — point it at an SD card reader with `--sd /Volumes/CARD`.

## Install

Requirements: macOS, Rust toolchain (`rustup`), and optionally `ffmpeg`
(`brew install ffmpeg`) for in-terminal video playback.

```sh
git clone https://github.com/vikyathshirva/gopro-import.git
cd gopro-import/gopro-rs
cargo build --release
ln -sf "$PWD/target/release/gopro" ~/.local/bin/gopro   # or anywhere on your PATH
```

On the camera: **Preferences › Connections › USB Connection → GoPro Connect**.
Plug in, wake the camera (the screen says *USB Connected*), run `gopro find`.

## Usage

```
gopro                         # full-screen browser (list, gallery, player, import)
gopro find                    # how the camera was located, is it answering
gopro status                  # model, firmware, battery, card space, counts
gopro ls [--since 2026-11-20] # list media, grouped by day
gopro pull --dry-run          # what would be imported
gopro pull                    # import everything → ~/Movies/GoPro/YYYY-MM-DD/
gopro pull GX010121 GOPR0113  # just these
gopro pull --since 2026-11-21 --delete-after
gopro play GX010124 [--full]  # stream to VLC / IINA / mpv / QuickTime
gopro rm GX010111             # delete from the card (asks first; -y to skip)
gopro open                    # open the import folder in Finder
gopro --sd /Volumes/CARD ls   # any command against a mounted SD card
```

Global options: `--dest DIR`, `--ip ADDR`, `--port N`, `--sd PATH`.
Pull options: `--flat` (no date folders), `--include-lrv`, `--dry-run`,
`--delete-after`, `--streams N` (parallel connections per file, default 6).

The import folder defaults to `~/Movies/GoPro`. Press `c` in the TUI to change it
(remembered in `~/Library/Application Support/gopro-import/config`), or pass
`--dest` for a single run.

### TUI keys

| key | action |
|---|---|
| ↑↓ / jk | move (newest files are at the top) |
| ctrl-d / ctrl-u | half a page down / up (PgUp/PgDn: a full page) |
| gg / G | jump to top / bottom |
| space | select / unselect |
| a | select all shown (toggle) |
| s | select everything shot that day |
| d / D | import selected / import everything shown |
| g / tab | gallery ⇄ list |
| enter | details with large preview |
| p / P | play video in the terminal / in a desktop player with sound |
| v | cycle videos / photos / all |
| / | filter by name (esc clears) |
| c | change the import folder |
| x | delete selected from camera (confirms) |
| r | refresh · o open folder · ? help · q quit |

Player: space pause · ←/→ ±10 s · ↑/↓ ±60 s · r restart · f proxy ⇄ full quality · P open with sound · esc close.

Image protocol is chosen from the environment; override with
`GOPRO_IMG=iterm2|kitty|sixel|halfblocks`, or `GOPRO_IMG=query` to run the
terminal capability query instead.

## Performance notes

Measured on a HERO12 Black over USB (USB 2.0 link, camera in turbo-transfer mode):

| | throughput |
|---|---|
| single connection | ≈ 8–10 MB/s |
| six parallel range requests (default) | ≈ 16 MB/s |

The camera's small HTTP server, not the cable, is the ceiling. A 20 GB card takes
about 20 minutes; an SD card reader is still several times faster if you have one.

## Troubleshooting

- **"camera is not answering"** — wake it (press Mode); it auto-powers-off after a
  few idle minutes even on USB. The screen should say *USB Connected*. Check the
  USB Connection setting is *GoPro Connect*, not MTP. Unplug / replug, then
  `gopro find`.
- **Transfers crawl (hundreds of KB/s)** — check `netstat -I en<N>` for input
  errors; plug the camera straight into the Mac rather than through a dock, or
  use an SD reader.
- **No video playback** — `brew install ffmpeg`. Without it, `p` opens the clip in
  VLC / QuickTime instead.

## Project layout

```
gopro-rs/src/camera.rs   discovery, Open GoPro HTTP client, media model, parallel download, SD fallback
gopro-rs/src/cli.rs      find / status / ls / pull / play / rm
gopro-rs/src/tui.rs      ratatui UI: list, gallery, preview, import dialog
gopro-rs/src/player.rs   ffmpeg-based in-terminal player, external player hand-off
gopro-rs/src/config.rs   persisted import folder
gopro.py                 stdlib-only Python fallback CLI (no TUI)
```

## License

MIT — see [LICENSE](LICENSE).
