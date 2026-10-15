# scribecut

Screen recorder with transcript-driven editing (Rust, egui, GStreamer, whisper.cpp, ffmpeg).
Runs on Linux (sway / xdg-desktop-portal) and macOS.

## Run

```sh
cargo run --release                       # home screen: record or open
cargo run --release -- video.mkv          # open a video (or its .edit.toml) directly
cargo run --release -- video.mkv --export out.mp4   # render the saved edits without a window
cargo run --release -- video.mkv --script out.md    # write the edited transcript as a script
cargo run --release -- video.mkv --check            # validate video.edit.toml (after editing it by hand)
cargo run --release -- video.mkv --transcribe       # (re)write video.transcript.json/.txt, no window;
                                                    # works on any video, e.g. an export, without an edit file
```

On macOS, record through an app bundle instead — see [macOS](#macos) below.

## Installing

Everywhere: `ffmpeg` (with `ffprobe`), GStreamer (for playback), plus `cmake` and a C/C++
compiler to build whisper.cpp. Slides need `pdfinfo` and `pdftoppm` (poppler) for PDFs and
LibreOffice for presentations; without them the other slide kinds still work.

The Whisper model (`small.en`, ~480 MB) is downloaded on first transcription to
`~/.local/share/scribecut/models/` (`~/Library/Application Support/scribecut/models/` on
macOS); set `SCRIBECUT_MODEL=base.en` for a smaller, faster one (or e.g. `small` for
non-English).

### Linux

GStreamer good/ugly plugins (`pipewiresrc`, `x264enc`, `opusenc`, `matroskamux`), a
ScreenCast portal (`xdg-desktop-portal-wlr` on sway), `fc-match` for the overlay font,
`slurp` for region recording, and `dunstctl` to mute notifications while recording.
The camera is read from `/dev/video0` by default.

### macOS

```sh
sudo port install pkgconfig cmake ffmpeg gstreamer1 \
    gstreamer1-gst-plugins-base gstreamer1-gst-plugins-good gstreamer1-gst-libav
# optional, for slides:
sudo port install poppler          # PDF slides
```

(Homebrew equivalents work too: `brew install pkg-config cmake ffmpeg gstreamer poppler`.)
LibreOffice is found in `/Applications` if it is installed; presentation slides need it.

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

The camera is an AVFoundation device, named rather than numbered (its index moves as
devices come and go), so `camera_device` in the settings holds something like
`FaceTime HD Camera`; a partial name or a bare index works too. Two things sway can do and
macOS cannot: enlarging the pointer for the duration of a recording, and muting
notifications — turn on Do Not Disturb yourself before recording.

## Recording

Pick what to record:

- **Full screen** – choose a monitor in the system's sharing dialog (Linux) or in
  scribecut's own list when more than one is attached (macOS).
- **Window** – where the screen-sharing portal can share single windows (GNOME, KDE,
  sway ≥ 1.12), its own picker is used and the recording follows the window, whatever
  covers it or whichever desktop you are on. Where it cannot — sway 1.11, and macOS, whose
  AVFoundation only hands out whole displays — scribecut lists the open windows (browsers
  first, filterable by title, or click one on screen on sway), brings the chosen one to the
  front and records the monitor cropped to it. Keep it in place; the recording pauses
  automatically while the window is not visible and resumes when it is. On macOS the
  window's *application* is brought forward, since raising one particular window of another
  app needs Accessibility permission, so put the right window in front yourself first.
- **Region** – drag out a rectangle. On Linux this uses `slurp`; on macOS scribecut dims
  the screen and you drag on it directly (the menu bar stays on top, so start below it).

For window and region recordings the monitor choice is remembered on Linux, so the portal
dialog usually only appears once. dunst notifications are paused while recording and
restored afterwards; macOS has no equivalent, so turn on Do Not Disturb yourself.

Options on the home screen (saved in `~/.config/scribecut/settings.toml`):

- **Record camera** – records the webcam as a second video track of the same file, in sync
  with screen and mic. Whether you appear is decided per part of the video when editing.
  `camera_device` is a V4L2 node on Linux (default `/dev/video0`) and an AVFoundation
  device name on macOS (e.g. `FaceTime HD Camera`; a partial name or a bare index also
  works). If the camera is busy the recording carries on without it and says so.
- **Bigger pointer** (sway) – doubles the mouse pointer size while recording
  (`pointer_scale`) and restores it afterwards. Apps that draw their own pointer may keep
  theirs. macOS only has a global accessibility setting that needs apps restarted, so the
  option is hidden there.

On sway ≥ 1.12, xdg-desktop-portal-wlr's default picker only offers monitors. To pick
windows, use a dmenu-style chooser in `~/.config/xdg-desktop-portal-wlr/config`:

```ini
[screencast]
chooser_type=dmenu
chooser_cmd=rofi -dmenu -p Share
```

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

On macOS the microphone is **not** captured by ffmpeg. Its AVFoundation audio input
drops 8 to 22 per cent of its samples even with nothing else running — measured at 4
per cent of one core, writing uncompressed, with no screen capture in the process —
and the demuxer offers nothing to tune. That is what made recordings sound chopped up,
and it took the waveform and the transcript with it, since both are derived from the
audio track. Padding the gaps afterwards only buys the right duration around missing
speech.

So the microphone is recorded through GStreamer's CoreAudio source, which captured
everything in the same test, and muxed with the picture when each piece ends. The
screen device takes a second or two to start delivering while the microphone opens
almost at once, so the extra sound at the front is trimmed: both halves stop together,
which makes the difference in their lengths the right amount to drop.

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
| Slides | "+ Slide…" → pick an image, PDF, presentation (.pptx/.odp/.key, converted with LibreOffice) or video clip; it replaces the screen recording for the selection. Set the page (or the clip's start) in the dialog; click its purple bar to edit |
| Camera | "Camera" menu → for the selection: Only me / Small window / Hide me; also the layout everywhere else and the small window's corner. Right-click an outlined range on the camera strip to remove it |
| Export | "Export…" → MP4 (H.264/AAC) |
| Script | "Export script…" → Markdown of the transcript after cuts: paragraphs at pauses with their time in the edited video, `---` between recorded pieces, `[On screen: …]` notes for overlays. Handy as a script for re-recording |

Playback skips cuts and shows slides, camera and overlays, so the preview matches the export
(layering: screen < slide < camera < text).

## Editing with text files (or an agent)

All edit state is plain text next to the video, so you can change it in an editor or have an agent do it:

- `<video>.edit.toml` – cuts (with optional notes), slides, camera layout and overlays. Its header
  explains the format. Times are `"m:ss.s"` strings on the original recording's timeline, so cuts never
  shift other times. scribecut reloads the file within a second when it changes on disk; mistakes are
  reported in the app and the previous state is kept.
- `<video>.transcript.txt` – the words, one phrase per line with its time (`[02:19.36] and we want…`),
  to find where things are said. `<video>.transcript.json` has every word's timing.
- `scribecut <video> --check` – validates the edit file (times, files, PDF pages) and exits non-zero on
  problems; run it after each change. `--export out.mp4` renders without the UI.

Example instruction for an agent: *"In recording.transcript.txt find where I say 'battery decline tab';
show slides/deck.pdf page 3 from there for 20 seconds, with me in the small window; then run --check."*

A Claude Code skill for the whole job (cut retakes and pauses, add title and closing slides, stay
under a length limit, export for Slack and check the cuts by re-transcribing the export) is in
[`skills/scribecut-edit/`](skills/scribecut-edit/SKILL.md). To use it from the recordings folder,
link it there: `ln -s ~/projects/scribecut/skills/scribecut-edit ~/Videos/scribecut/.claude/skills/`.

Rendered PDF pages, converted presentations and video poster frames are cached in `.scribecut-cache/`.
Projects from older versions (`<video>.scribecut.json`) are converted on open (the old file is kept as
`.json.bak`).

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
