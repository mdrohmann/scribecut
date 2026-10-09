# scribecut

Screen recorder with transcript-driven editing (Rust, egui, GStreamer, whisper.cpp, ffmpeg).

## Run

```sh
cargo run --release                       # home screen: record or open
cargo run --release -- video.mkv          # open a video (or its .scribecut.json) directly
cargo run --release -- video.mkv --export out.mp4   # render the saved edits without a window
```

Needs: `ffmpeg`, `fc-match`, GStreamer (good/ugly plugins: `pipewiresrc`, `x264enc`, `opusenc`, `matroskamux`),
and a ScreenCast portal (`xdg-desktop-portal-wlr` on sway). The Whisper model (`small.en`, ~480 MB) is
downloaded on first transcription to `~/.local/share/scribecut/models/`; set `SCRIBECUT_MODEL=base.en`
for a smaller, faster one (or e.g. `small` for non-English).

## Recording

Pick what to record:

- **Full screen** – choose a monitor in the system's sharing dialog.
- **Window** – choose from a list of open windows (browsers first, filterable by title), or click one
  on screen. On sway, whose portal (as of 1.11) can only share whole monitors, scribecut records the
  monitor cropped to the window: keep it in place and uncovered while recording. Windows on another
  workspace are brought to the front first. On desktops whose portal shares windows, its own picker is used.
- **Region** – drag out a rectangle (sway, needs `slurp`).

For window and region recordings the monitor choice is remembered, so the dialog usually only appears once.

Then Pause / Resume as often as you like → Stop & edit. Each piece is saved to
`~/Videos/scribecut/<timestamp>/`, joined into `recording.mkv`, and transcribed in the background.
Resume points show as yellow lines on the timeline.

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
| Undo | `Ctrl+Z` |
| Zoom / pan timeline | Mouse wheel / Shift+wheel |
| Export | "Export…" → MP4 (H.264/AAC) |

Playback skips cuts and shows overlays, so the preview matches the export. Edits are saved
automatically to `<video>.scribecut.json`.

## Tests

```sh
cargo test                      # unit tests
cargo test -- --ignored         # real ffmpeg export + whisper transcription (downloads model + sample)
```
