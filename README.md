# scribecut

Screen recorder with transcript-driven editing (Rust, egui, GStreamer, whisper.cpp, ffmpeg).
Runs on Linux (sway / xdg-desktop-portal) and macOS.

## Run

```sh
cargo run --release                       # home screen: record or open
cargo run --release -- video.mkv          # open a video (or its .scribecut.json) directly
cargo run --release -- video.mkv --export out.mp4   # render the saved edits without a window
```

On macOS, record through an app bundle instead — see [macOS](#macos) below.

## Installing

Everywhere: `ffmpeg` (with `ffprobe`), GStreamer (for playback), plus `cmake` and a C/C++
compiler to build whisper.cpp.

The Whisper model (`small.en`, ~480 MB) is downloaded on first transcription to
`~/.local/share/scribecut/models/` (`~/Library/Application Support/scribecut/models/` on
macOS); set `SCRIBECUT_MODEL=base.en` for a smaller, faster one (or e.g. `small` for
non-English).

### Linux

GStreamer good/ugly plugins (`pipewiresrc`, `x264enc`, `opusenc`, `matroskamux`), a
ScreenCast portal (`xdg-desktop-portal-wlr` on sway), `fc-match` for the overlay font, and
`slurp` for region recording.

### macOS

```sh
sudo port install pkgconfig cmake ffmpeg gstreamer1 \
    gstreamer1-gst-plugins-base gstreamer1-gst-plugins-good gstreamer1-gst-libav
```

(Homebrew equivalents work too: `brew install pkg-config cmake ffmpeg gstreamer`.)

Then build the app bundle and launch it:

```sh
scripts/bundle-macos.sh
open target/scribecut.app
```

**Record through the bundle, not `cargo run`.** macOS grants Screen Recording and
Microphone permission to an *application*; a bare binary inherits whatever the terminal
was granted, which usually means recording a black screen. The first recording raises both
prompts — grant them under *System Settings › Privacy & Security* and restart the app.
Window *titles* also need Screen Recording permission: until it is granted the window list
shows application names only. Editing and exporting an existing video need no permissions,
so `cargo run` is fine for that.

Because the bundle is only ad-hoc signed, its identity changes whenever the binary does,
so macOS may ask for permission again after a rebuild.

## Recording

Pick what to record:

- **Full screen** – choose a monitor in the system's sharing dialog (Linux) or in
  scribecut's own list when more than one is attached (macOS).
- **Window** – choose from a list of open windows (browsers first, filterable by title).
  Neither sway's portal (as of 1.11) nor macOS' AVFoundation can hand out a single window,
  so scribecut records the monitor cropped to the window: keep it in place and uncovered
  while recording. On Linux, windows on another workspace are brought to the front first,
  or you can click one on screen (needs `slurp`). On macOS the window's application is
  brought to the front, but raising one specific window of another app is not possible
  without Accessibility permission, so put the window in front yourself first. On desktops
  whose portal shares windows, its own picker is used.
- **Region** – drag out a rectangle. On Linux this uses `slurp`; on macOS scribecut dims
  the screen and you drag on it directly (the menu bar stays on top, so start the drag
  below it).

For window and region recordings on Linux the monitor choice is remembered, so the portal
dialog usually only appears once.

Then Pause / Resume as often as you like → Stop & edit. Each piece is saved to
`~/Videos/scribecut/<timestamp>/` (`~/Movies/scribecut/...` on macOS), joined into
`recording.mkv`, and transcribed in the background. Resume points show as yellow lines on
the timeline.

Recordings are H.264: x264 on Linux, VideoToolbox hardware encoding on macOS. Override
with `SCRIBECUT_ENCODER` (e.g. `libx264`) and `SCRIBECUT_BITRATE` (default `12M`) on macOS.

## Audio

Narration from a built-in microphone comes in quiet, uneven and sitting on
low-frequency rumble, so the export, the waveform and the transcription all run it
through a voice cleanup chain first: a high-pass to clear the rumble, `speechnorm`
to level the speech, gentle downward compression, and loudness normalisation to
about -16 LUFS with a true-peak ceiling that leaves room for the AAC encoder.

The recording itself is never processed — cleanup happens on the way out, so the
original is always there to fall back on.

| Setting | Default | What it does |
| --- | --- | --- |
| `SCRIBECUT_AUDIO` | `voice` | `off` for the raw recording, `voice` for the chain above, `strong` to add denoising and de-essing for a noisy room |
| `SCRIBECUT_LOUDNESS` | `-16` | Target programme loudness in LUFS |

**You should not need to tune this per microphone.** The two leveling stages are
adaptive: `loudnorm` measures the actual programme and `speechnorm` works relative
to peaks. Measured over one speech recording fed in at levels from -6 to -40 LUFS,
and separately with added hiss, a 12 dB bass lift and a 10 dB treble lift, every
variant came out between -14.4 and -14.7 LUFS with the true peak on target. The one
judgement call left is how noisy your room is, which is what `strong` is for.

It helps far more to record at a sensible level in the first place. On a real
recording measured here the microphone was about 19 dB below the usual speech
target, and gain applied afterwards cannot add detail that was never captured —
raise the input level in the system's sound settings and the chain has less to do.

Everything here is plain ffmpeg filtering, so it behaves the same on Linux and
macOS. `speechnorm` and `deesser` need ffmpeg 4.3 or newer; an older build quietly
drops those stages instead of failing the export.

## Editing

| Action | How |
| --- | --- |
| Jump to a word | Click it in the transcript |
| Select a range | Shift+click a second word, drag on the timeline, or `I` / `O` at the playhead |
| Fine-tune | `←` `→` one frame, `Shift+←` `Shift+→` one second |
| Find pauses | The blue waveform under the words shows loudness (scaled to the recording's noise floor), so gaps between words stand out; zoom in to place cut edges in them |
| Cut | `Delete` / `X` / "Cut selection"; right-click a red cut (or "Restore") to undo it |
| Overlay text | "+ Add overlay" (uses the selection as its time range) → text → "Place on video…" → click the spot |
| Edit overlay | Click its green bar on the timeline, or "Edit" in the list |
| Undo | `Ctrl+Z` (`Cmd+Z` on macOS) |
| Zoom / pan timeline | Mouse wheel / Shift+wheel |
| Export | "Export…" → MP4 (H.264/AAC) |

Playback skips cuts and shows overlays, so the preview matches the export. Edits are saved
automatically to `<video>.scribecut.json`.

Overlays are drawn with a bold sans face found via `fc-match`, falling back to a face
shipped with the OS; `SCRIBECUT_FONT=/path/to/font.ttf` picks one explicitly. The same file
is used for the preview and the export so the two match.

## Tests

```sh
cargo test                      # unit tests
cargo test -- --ignored         # tests that touch real media and devices
```

The `--ignored` set does real work: an ffmpeg export, a playbin decode, a whisper
transcription (downloads the model and a sample), and two screen recordings. The recording
ones need Screen Recording permission for whatever runs them — granted to your terminal
when you use `cargo test`, which is separate from the app bundle's own permission.

## Layout

| Module | What it does |
| --- | --- |
| `app` | Top-level screens: home, display/window pickers, recording, editing |
| `capture` | Recording to segment files; `capture/linux.rs` (portal + PipeWire), `capture/macos.rs` (AVFoundation via ffmpeg) |
| `platform` | Listing windows and monitors; `platform/sway.rs`, `platform/macos.rs` (CoreGraphics) |
| `geom` | Screen rectangles, monitors, and the crop maths shared by both platforms |
| `region` | scribecut's own full-screen region picker |
| `editor` | The editing screen |
| `player` | Playback through GStreamer's `playbin` |
| `audio` | The voice cleanup chain and its presets |
| `media` | ffmpeg/ffprobe wrappers, audio peaks, tool lookup |
| `export` | Rendering the edit with ffmpeg |
| `transcribe` | whisper.cpp with word timestamps |
| `project` | The saved edit (`.scribecut.json`) |
