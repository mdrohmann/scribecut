# scribecut

Screen recorder with transcript-driven editing (Rust, egui, GStreamer, whisper.cpp, ffmpeg).

## Run

```sh
cargo run --release                       # home screen: record or open
cargo run --release -- video.mkv          # open a video (or its .edit.toml) directly
cargo run --release -- video.mkv --export out.mp4   # render the saved edits without a window
cargo run --release -- video.mkv --script out.md    # write the edited transcript as a script
cargo run --release -- video.mkv --check            # validate video.edit.toml (after editing it by hand)
```

Needs: `ffmpeg`, `fc-match`, `pdftoppm`/`pdfinfo` (PDF slides), LibreOffice (presentation slides), GStreamer (good/ugly plugins: `pipewiresrc`, `x264enc`, `opusenc`, `matroskamux`),
and a ScreenCast portal (`xdg-desktop-portal-wlr` on sway). The Whisper model (`small.en`, ~480 MB) is
downloaded on first transcription to `~/.local/share/scribecut/models/`; set `SCRIBECUT_MODEL=base.en`
for a smaller, faster one (or e.g. `small` for non-English).

## Recording

Pick what to record:

- **Full screen** – choose a monitor in the system's sharing dialog.
- **Window** – where the screen-sharing portal can share single windows (GNOME, KDE, sway ≥ 1.12),
  its own picker is used and the recording follows the window, whatever covers it or whichever
  desktop you are on. On sway 1.11, which can only share monitors, scribecut lists the open windows
  (browsers first, filterable by title, or click one on screen), brings the chosen one to the front and
  records the monitor cropped to it. Keep it in place; the recording pauses automatically while the
  window is not visible (e.g. you switch workspace) and resumes when it is.
- **Region** – drag out a rectangle (sway, needs `slurp`).

For window and region recordings the monitor choice is remembered, so the dialog usually only appears once.
dunst notifications are paused while recording and restored afterwards.

Options on the home screen (saved in `~/.config/scribecut/settings.toml`):

- **Record camera** – records the webcam (`camera_device`, default `/dev/video0`, 1280×720) as a second
  video track of the same file, in sync with screen and mic. Whether you appear is decided per part of
  the video when editing.
- **Bigger pointer** (sway) – doubles the mouse pointer size while recording (`pointer_scale`) and
  restores it afterwards. Apps that draw their own pointer may keep theirs.

On sway ≥ 1.12, xdg-desktop-portal-wlr's default picker only offers monitors. To pick windows, use a
dmenu-style chooser in `~/.config/xdg-desktop-portal-wlr/config`:

```ini
[screencast]
chooser_type=dmenu
chooser_cmd=rofi -dmenu -p Share
```

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

Rendered PDF pages, converted presentations and video poster frames are cached in `.scribecut-cache/`.
Projects from older versions (`<video>.scribecut.json`) are converted on open (the old file is kept as
`.json.bak`).

## Tests

```sh
cargo test                      # unit tests
cargo test -- --ignored         # real ffmpeg export + whisper transcription (downloads model + sample)
```
