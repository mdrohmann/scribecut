---
name: scribecut-edit
description: Edit a scribecut screen recording into a short, shareable video - cut repeated takes and silent pauses, add title/closing slides from a PDF, keep it under a length limit, and export an MP4 that plays in Slack. Use when asked to cut, trim, tighten, shorten or export a recording under ~/Videos/scribecut, to add slides to one, or to remove retakes, "ums" or pauses.
---

# Editing a scribecut recording

Learned from a real product demo: a 5:49 raw take with several retakes became a 1:51
video with a title slide at the start and a next-steps slide at the end.

## Inputs

Each recording is a folder `~/Videos/scribecut/<project name>/` (named on scribecut's home
screen; a timestamp unless the user typed a name, and they may rename it later) with:

- `recording.mkv` - the raw recording (never modify it)
- `recording.edit.toml` - cuts, slides, camera, overlays. Its header documents the
  format. All times are on the ORIGINAL timeline; cuts never shift them.
- `recording.transcript.txt` - phrases with `[mm:ss.ss]` times; `recording.transcript.json`
  has every word with `start`/`end` seconds.

"The latest recording" = the newest folder (`ls -td ~/Videos/scribecut/*/ | head -1`).
Confirm which one if the request is ambiguous.

Tool: `SC=~/projects/scribecut/target/release/scribecut`. Don't edit files in
`~/projects/scribecut` or `~/.local/share/scribecut/models` as part of an editing job -
that is the app's source, and other sessions may be working on it.

Helper scripts are in this skill's `scripts/` folder (`<skill>` below). Run without
arguments, `$SC` opens the app window instead of printing help.

## Steps

Three scripts do the mechanical work; your job is reading the overview and writing the plan.

1. **Prep**:
   ```bash
   python3 -I <skill>/scripts/prep.py <dir> <scratch>
   ```
   If the recording has no transcript yet, prep runs `$SC <dir>/recording.mkv --transcribe`
   (~30 s for 5 min). It writes `levels.txt`, `silences.txt`, `words.json` and
   `overview.txt` to `<scratch>` and prints the overview: one sentence per line with its
   time, then the repeated phrases (likely retakes).
   Read only that - the word list is rarely needed. Whisper mishears names
   (product names come out as similar-sounding words, "PLC" for "PoC"); read for meaning.
2. **Decide what to keep.** A retake restarts the same idea with similar wording; keep
   only the LAST complete take of each passage and cut from the start of the first take
   to the start of the last. Also cut abandoned starts ("Please also, so…") and side
   remarks repeated later ("feedback is welcome" mid-video and again at the end). Doubled
   phrases inside a kept take ("to the shared weekly view to the shared weekly view")
   become `extra_cuts`. Not every repeated phrase in the list is a
   retake - "and of course" twice a minute apart is just speech.
3. **Write the plan** (`<scratch>/plan.json`; format in `make_cuts.py`'s docstring) using
   phrase anchors: `"^words"` = just before them, `"words$"` = just after,
   `"...@m:ss"` to pick one of several occurrences. The script finds the pause at that
   boundary itself. A keep's 4th element is the note for the gap before it. Then:
   ```bash
   python3 -I <skill>/scripts/make_cuts.py <scratch>/plan.json <scratch>/silences.txt <dir>/recording.edit.toml
   ```
   It prints the length and every non-pause cut and slide with its EDITED time. A
   WARNING means a boundary had no pause and was put at the quietest 50 ms nearby - list
   those as spots to listen to. The first run backs the edit file up to
   `recording.edit.toml.orig` and every run rebuilds from it, so re-run freely. Pause cuts
   leave 0.10 s each side (`pad`) and only touch pauses of 0.35 s or more (`min_pause`).
   Plain seconds still work for any time.
4. **Hit the length limit.** If the length is over the limit, drop the least essential
   kept passage (a side remark, not the goal, the demo or the next steps) and tell the
   user exactly what was dropped and how long the video would be with it. Don't silently
   exceed a limit the user set.
5. **Slides.** Copy the PDF into the recording folder under a plain name
   (`cp ~/Downloads/X.pptx.pdf <dir>/slides.pdf`) and reference it as `"file": "slides.pdf"`:
   a local copy keeps the project self-contained. (scribecut before `068d14e` could not
   render slides from a file named `*.pptx.pdf`.) Slides replace the screen recording but keep the
   audio: the title page from 0 to where the screen demo starts (e.g. `"^So this is the
   view"`), the closing page from the last take of the next steps to `"end"`. Check the
   page count with `pdfinfo`.
6. **Export and verify** in the background (5+ minutes for a ~2 min video):
   ```bash
   python3 -I <skill>/scripts/finish.py <dir>/recording.edit.toml <dir>/<descriptive-name>.mp4 [--limit m:ss]
   ```
   It runs `--check`, exports, checks H.264/yuv420p + AAC in MP4 (what Slack plays) and the
   duration, and saves one frame per slide to `<name>.frames/` (look at them with Read).
   Then it transcribes the export and prints the words around every non-pause cut with
   `‖` at the cut - read those lines: a doubled phrase, a missing word or a stray word
   ("calculated here faster. So the next steps") means the cut needs moving. Whisper's
   time for a word next to a long pause can point at a stray sound (a breath, a false
   start); anchor on a later word then (`"^next steps are to integrate@4:35"`).
   `--verify-only` re-checks an existing export without rendering again. Wait for its notification - don't poll with
   `pgrep -f`, which matches its own shell. If you must stop it, also kill the ffmpeg it
   leaves behind (`pgrep -af 'ffmpeg -nostdin'`).

`$SC <dir>/recording.edit.toml --script out.md` gives an edited transcript, but a rough
one: words touching a cut edge are dropped even when the audio is intact.

## Report back

The output path, duration and size, the codec line, the list of removed retakes with
their notes, anything dropped for length, and any spot you are unsure about (with its time
in the edited video) so the user can listen to it.

Examples: `examples/anchored.plan.json` is a phrase-anchored plan (2:09 from 5:24, two
slides); `examples/seconds.plan.json` is an older plan in plain seconds.
