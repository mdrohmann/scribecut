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

Record → pick a screen → Pause / Resume as often as you like → Stop & edit. Each piece is saved to
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
