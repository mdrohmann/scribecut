//! Project data model: media file, transcript words, cuts, text overlays,
//! slides, camera layout and re-recorded audio. Stored by `edit_file`.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A transcribed word with its time span in source seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// A half-open time range `[start, end)` in source seconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Range {
    pub start: f64,
    pub end: f64,
}

impl Range {
    pub fn new(a: f64, b: f64) -> Self {
        Self { start: a.min(b), end: a.max(b) }
    }

    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

/// Text drawn on top of the video during `[start, end)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overlay {
    pub start: f64,
    pub end: f64,
    pub text: String,
    /// Centre of the text, normalised to the video frame (0..1).
    pub x: f32,
    pub y: f32,
    /// Font size as a fraction of the video height.
    pub size: f32,
    pub color: [u8; 3],
}

/// A removed time range, with an optional note (e.g. "first take").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cut {
    pub start: f64,
    pub end: f64,
    pub note: String,
}

impl Cut {
    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
    }

    pub fn len(&self) -> f64 {
        self.end - self.start
    }
}

/// Material shown instead of the screen recording during `[start, end)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Slide {
    pub start: f64,
    pub end: f64,
    /// As written in the edit file; relative paths are relative to its folder.
    pub file: PathBuf,
    /// Page of a PDF or presentation (1-based).
    pub page: u32,
    /// For video clips: where in the clip playback starts.
    pub clip_start: f64,
}

/// Narration recorded again for `[start, end)`, replacing the recording's own
/// sound there. It was spoken over the edited video, so the file holds the
/// parts of the range that were kept at the time (`pieces`), back to back.
/// Cuts made later inside the range remove the narration along with the
/// picture.
#[derive(Clone, Debug, PartialEq)]
pub struct Voiceover {
    pub start: f64,
    pub end: f64,
    /// As written in the edit file; relative paths are relative to its folder.
    pub file: PathBuf,
    /// The source ranges the file covers, in order: what was kept of
    /// `[start, end)` when it was recorded.
    pub pieces: Vec<Range>,
    /// Older edit files only kept the length recorded over, not the pieces.
    /// Set when that length no longer matches the cuts, so which parts of the
    /// file belong where is unknown.
    pub stale_length: Option<f64>,
    /// What was said, on the file's own timeline (from its transcript file).
    pub words: Vec<Word>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraMode {
    /// Hide the camera.
    #[default]
    Off,
    /// Camera in a small window.
    Pip,
    /// Only the camera, full frame.
    Full,
}

impl CameraMode {
    pub const ALL: [CameraMode; 3] = [CameraMode::Full, CameraMode::Pip, CameraMode::Off];

    pub fn name(self) -> &'static str {
        match self {
            CameraMode::Off => "off",
            CameraMode::Pip => "pip",
            CameraMode::Full => "full",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CameraMode::Off => "Hide me",
            CameraMode::Pip => "Small window",
            CameraMode::Full => "Only me",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.name() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRange {
    pub start: f64,
    pub end: f64,
    pub mode: CameraMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

    pub fn name(self) -> &'static str {
        match self {
            Corner::TopLeft => "top-left",
            Corner::TopRight => "top-right",
            Corner::BottomLeft => "bottom-left",
            Corner::BottomRight => "bottom-right",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == s)
    }
}

/// How the camera is shown when no camera range says otherwise, and where the
/// small window goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraSettings {
    pub default: CameraMode,
    pub corner: Corner,
    /// Small-window width as a fraction of the frame width.
    pub width: f32,
    /// Gap to the frame edges as a fraction of the frame width.
    pub margin: f32,
}

impl Default for CameraSettings {
    fn default() -> Self {
        Self { default: CameraMode::Off, corner: Corner::BottomRight, width: 0.25, margin: 0.02 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Project {
    /// Absolute path of the recording.
    pub media: PathBuf,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    /// Pixel size of the camera track (the media's 2nd video stream), if recorded.
    pub camera_size: Option<(u32, u32)>,
    /// Start times of each recorded piece after the first (pause/resume points).
    pub segment_starts: Vec<f64>,
    /// From the transcript file, not the edit file: the recording's own
    /// words. [`Project::heard_words`] has them with re-recorded audio applied.
    pub words: Vec<Word>,
    pub cuts: Vec<Cut>,
    pub overlays: Vec<Overlay>,
    pub slides: Vec<Slide>,
    pub camera: CameraSettings,
    pub camera_ranges: Vec<CameraRange>,
    pub voiceovers: Vec<Voiceover>,
}

impl Project {
    /// Where the edit file for a media file lives: `<media>.edit.toml`.
    pub fn path_for(media: &Path) -> PathBuf {
        media.with_extension("edit.toml")
    }

    /// Loads a project from an edit file, a media file (its edit file, or an old
    /// `.scribecut.json` project, which is migrated) or an old project file.
    pub fn load(path: &Path) -> Result<Self> {
        crate::edit_file::load(path)
    }

    /// Writes the edit file (cuts, slides, …). The transcript is separate.
    pub fn save(&self) -> Result<()> {
        crate::edit_file::save(self)
    }

    pub fn save_transcript(&self) -> Result<()> {
        crate::edit_file::save_transcript(self)
    }

    /// Folder that relative paths in the edit file are relative to.
    pub fn base_dir(&self) -> PathBuf {
        self.media.parent().map(Path::to_path_buf).unwrap_or_default()
    }

    pub fn resolve(&self, file: &Path) -> PathBuf {
        if file.is_absolute() { file.to_path_buf() } else { self.base_dir().join(file) }
    }

    pub fn slide_at(&self, t: f64) -> Option<usize> {
        self.slides.iter().rposition(|s| s.start <= t && t < s.end)
    }

    /// Camera layout at `t`: the last camera range covering it, else the default.
    pub fn camera_mode_at(&self, t: f64) -> CameraMode {
        if self.camera_size.is_none() {
            return CameraMode::Off;
        }
        self.camera_ranges
            .iter()
            .rev()
            .find(|r| r.start <= t && t < r.end)
            .map_or(self.camera.default, |r| r.mode)
    }

    /// The parts of the timeline shown with camera mode `mode`, merged.
    pub fn camera_intervals(&self, mode: CameraMode) -> Vec<Range> {
        let mut bounds = vec![0.0, self.duration];
        for r in &self.camera_ranges {
            bounds.extend([r.start.clamp(0.0, self.duration), r.end.clamp(0.0, self.duration)]);
        }
        bounds.sort_by(f64::total_cmp);
        bounds.dedup();
        let mut out: Vec<Range> = Vec::new();
        for w in bounds.windows(2) {
            if w[1] - w[0] < 1e-6 || self.camera_mode_at((w[0] + w[1]) / 2.0) != mode {
                continue;
            }
            match out.last_mut() {
                Some(last) if (last.end - w[0]).abs() < 1e-9 => last.end = w[1],
                _ => out.push(Range { start: w[0], end: w[1] }),
            }
        }
        out
    }

    /// Sets the camera mode for a range, replacing whatever ranges it covers.
    pub fn set_camera(&mut self, range: Range, mode: CameraMode) {
        let mut kept = Vec::new();
        for r in self.camera_ranges.drain(..) {
            if r.end <= range.start || r.start >= range.end {
                kept.push(r);
                continue;
            }
            if r.start < range.start {
                kept.push(CameraRange { end: range.start, ..r });
            }
            if r.end > range.end {
                kept.push(CameraRange { start: range.end, ..r });
            }
        }
        kept.push(CameraRange { start: range.start, end: range.end, mode });
        kept.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.camera_ranges = kept;
    }

    /// Adds a cut, merging it with any cuts it overlaps or touches.
    pub fn add_cut(&mut self, cut: Range) {
        self.add_cut_with_note(cut, String::new());
    }

    pub fn add_cut_with_note(&mut self, cut: Range, note: String) {
        let cut = Range::new(cut.start.max(0.0), cut.end.min(self.duration));
        if cut.len() <= 0.0 {
            return;
        }
        self.cuts.push(Cut { start: cut.start, end: cut.end, note });
        self.cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut merged: Vec<Cut> = Vec::with_capacity(self.cuts.len());
        for c in self.cuts.drain(..) {
            match merged.last_mut() {
                Some(last) if c.start <= last.end => {
                    last.end = last.end.max(c.end);
                    if last.note.is_empty() {
                        last.note = c.note;
                    }
                }
                _ => merged.push(c),
            }
        }
        self.cuts = merged;
    }

    pub fn cut_at(&self, t: f64) -> Option<usize> {
        self.cuts.iter().position(|c| c.contains(t))
    }

    /// The parts of the source that survive the cuts, in order.
    pub fn kept_ranges(&self) -> Vec<Range> {
        let mut kept = Vec::new();
        let mut t = 0.0;
        for c in &self.cuts {
            if c.start > t {
                kept.push(Range { start: t, end: c.start });
            }
            t = t.max(c.end);
        }
        if self.duration > t {
            kept.push(Range { start: t, end: self.duration });
        }
        kept.retain(|r| r.len() > 1e-3);
        kept
    }

    pub fn output_duration(&self) -> f64 {
        self.kept_ranges().iter().map(Range::len).sum()
    }

    /// Where source time `t` ends up in the exported video (cut time removed).
    pub fn output_time(&self, t: f64) -> f64 {
        self.kept_ranges().iter().map(|r| (t.min(r.end) - r.start).max(0.0)).sum()
    }

    /// The kept parts of `range`, each with where it starts in a recording
    /// made over the edited video (seconds after the range's start).
    pub fn kept_pieces(&self, range: Range) -> Vec<(Range, f64)> {
        let mut offset = 0.0;
        let mut out = Vec::new();
        for k in self.kept_ranges() {
            let piece = Range { start: k.start.max(range.start), end: k.end.min(range.end) };
            if piece.len() > 1e-3 {
                out.push((piece, offset));
                offset += piece.len();
            }
        }
        out
    }

    /// Seconds of `range` left after the cuts.
    pub fn kept_length(&self, range: Range) -> f64 {
        self.kept_pieces(range).iter().map(|(r, _)| r.len()).sum()
    }

    /// What is wrong with a voiceover, if anything: kept parts of its range
    /// that it has no narration for (a cut restored after recording), or an
    /// older take that no longer lines up.
    pub fn voiceover_problem(&self, v: &Voiceover) -> Option<String> {
        if let Some(length) = v.stale_length {
            return Some(format!(
                "the cuts inside it changed since it was recorded ({length:.2}s then, {:.2}s now) and the \
                 edit file doesn't say which cuts it was recorded with - record it again",
                self.kept_length(Range { start: v.start, end: v.end })
            ));
        }
        // Kept time in the range minus the kept time the pieces cover.
        let kept = self.kept_pieces(Range { start: v.start, end: v.end });
        let kept_len: f64 = kept.iter().map(|(r, _)| r.len()).sum();
        let covered: f64 = kept
            .iter()
            .flat_map(|(r, _)| v.pieces.iter().map(move |p| (p.end.min(r.end) - p.start.max(r.start)).max(0.0)))
            .sum();
        let gap = kept_len - covered;
        (gap > 0.05).then(|| {
            format!("{gap:.1}s inside it has no narration (a cut there was restored after it was recorded)")
        })
    }

    /// A voiceover's pieces, each with where it starts in the file.
    pub fn take_pieces(v: &Voiceover) -> Vec<(Range, f64)> {
        let mut offset = 0.0;
        v.pieces
            .iter()
            .map(|p| {
                let at = offset;
                offset += p.len();
                (*p, at)
            })
            .collect()
    }

    /// Source time at `t` seconds into the exported video (the inverse of
    /// [`Project::output_time`]).
    pub fn source_time(&self, t: f64) -> f64 {
        let mut left = t.max(0.0);
        let kept = self.kept_ranges();
        for r in &kept {
            if left <= r.len() {
                return r.start + left;
            }
            left -= r.len();
        }
        kept.last().map_or(0.0, |r| r.end)
    }

    /// Adds a re-recording, replacing any earlier one it overlaps.
    pub fn add_voiceover(&mut self, v: Voiceover) {
        self.voiceovers.retain(|o| o.end <= v.start || o.start >= v.end);
        self.voiceovers.push(v);
        self.voiceovers.sort_by(|a, b| a.start.total_cmp(&b.start));
    }

    /// The words as the viewer hears them: the recording's own, with those in
    /// re-recorded ranges replaced by what was said in the re-recording.
    pub fn heard_words(&self) -> Vec<Word> {
        let mut words: Vec<Word> = self
            .words
            .iter()
            .filter(|w| {
                let mid = (w.start + w.end) / 2.0;
                !self.voiceovers.iter().any(|v| v.start <= mid && mid < v.end)
            })
            .cloned()
            .collect();
        for v in &self.voiceovers {
            let pieces = Self::take_pieces(v);
            for w in &v.words {
                // A word in the file belongs to the piece that was showing.
                let Some((piece, offset)) = pieces.iter().rev().find(|(_, o)| *o <= w.start + 1e-6) else { continue };
                let start = piece.start + (w.start - offset);
                let end = (piece.start + (w.end - offset)).min(piece.end).max(start);
                words.push(Word { text: w.text.clone(), start, end });
            }
        }
        words.sort_by(|a, b| a.start.total_cmp(&b.start));
        words
    }
}

/// Index of the word being spoken at `t` (or the last one before it).
pub fn word_at(words: &[Word], t: f64) -> Option<usize> {
    let i = words.partition_point(|w| w.start <= t);
    (i > 0).then(|| i - 1).filter(|&i| t < words[i].end.max(words[i].start + 0.05) + 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project { media: PathBuf::from("/tmp/x.mkv"), duration: 20.0, width: 1920, height: 1080, fps: 30.0, ..Default::default() }
    }

    fn cut(start: f64, end: f64) -> Cut {
        Cut { start, end, note: String::new() }
    }

    #[test]
    fn cuts_merge_and_clamp() {
        let mut p = project();
        p.add_cut(Range::new(5.0, 3.0));
        p.add_cut_with_note(Range::new(4.0, 6.0), "take".into());
        p.add_cut(Range::new(18.0, 25.0));
        p.add_cut(Range::new(10.0, 10.0));
        assert_eq!(p.cuts, vec![Cut { note: "take".into(), ..cut(3.0, 6.0) }, cut(18.0, 20.0)]);
        assert_eq!(
            p.kept_ranges(),
            vec![Range { start: 0.0, end: 3.0 }, Range { start: 6.0, end: 18.0 }]
        );
        assert_eq!(p.output_duration(), 15.0);
        assert_eq!(p.output_time(2.0), 2.0);
        assert_eq!(p.output_time(4.5), 3.0);
        assert_eq!(p.output_time(7.0), 4.0);
        assert_eq!(p.cut_at(4.0), Some(0));
        assert_eq!(p.cut_at(6.0), None);
    }

    #[test]
    fn word_lookup() {
        let mut p = project();
        p.words = vec![
            Word { text: "a".into(), start: 1.0, end: 1.5 },
            Word { text: "b".into(), start: 2.0, end: 2.4 },
        ];
        assert_eq!(word_at(&p.words, 0.5), None);
        assert_eq!(word_at(&p.words, 1.2), Some(0));
        assert_eq!(word_at(&p.words, 2.1), Some(1));
        assert_eq!(word_at(&p.words, 10.0), None);
    }

    #[test]
    fn voiceover_spans_a_cut() {
        let mut p = project();
        p.add_cut(Range::new(5.0, 7.0));
        let range = Range::new(4.0, 9.0);
        assert_eq!(
            p.kept_pieces(range),
            vec![(Range { start: 4.0, end: 5.0 }, 0.0), (Range { start: 7.0, end: 9.0 }, 1.0)]
        );
        assert_eq!(p.kept_length(range), 3.0);
        let w = |t: &str, s: f64| Word { text: t.into(), start: s, end: s + 0.4 };
        p.words = vec![w("old", 1.0), w("gone", 4.2), w("cut", 6.0), w("also", 8.0), w("kept", 9.5)];
        let v = |start, end, words| Voiceover {
            start,
            end,
            file: "v.flac".into(),
            pieces: vec![Range { start: 4.0, end: 5.0 }, Range { start: 7.0, end: 9.0 }],
            stale_length: None,
            words,
        };
        p.add_voiceover(v(4.0, 9.0, vec![w("new", 0.1), w("words", 1.5)]));
        let heard: Vec<(String, f64)> = p.heard_words().into_iter().map(|w| (w.text, w.start)).collect();
        // 1.5 s into the file is 0.5 s into the second piece, which starts at 7.
        assert_eq!(heard, [("old".into(), 1.0), ("new".into(), 4.1), ("words".into(), 7.5), ("kept".into(), 9.5)]);
        assert_eq!(p.voiceover_problem(&p.voiceovers[0]), None);
        // A later cut inside the range takes narration and picture out together.
        p.add_cut(Range::new(7.5, 8.0));
        assert_eq!(p.voiceover_problem(&p.voiceovers[0]), None);
        // Restoring the cut it was recorded around leaves a part without narration.
        p.cuts.clear();
        let problem = p.voiceover_problem(&p.voiceovers[0]).unwrap();
        assert!(problem.starts_with("2.0s inside it has no narration"), "{problem}");
        assert_eq!(p.source_time(p.output_time(8.5)), 8.5);
        // A new take replaces the one it overlaps.
        p.add_voiceover(v(8.0, 10.0, vec![]));
        p.add_voiceover(v(12.0, 13.0, vec![]));
        assert_eq!(p.voiceovers.iter().map(|v| v.start).collect::<Vec<_>>(), [8.0, 12.0]);
    }

    #[test]
    fn camera_layout() {
        let mut p = project();
        assert_eq!(p.camera_mode_at(1.0), CameraMode::Off, "no camera recorded");
        p.camera_size = Some((1280, 720));
        p.camera.default = CameraMode::Pip;
        p.set_camera(Range::new(2.0, 8.0), CameraMode::Full);
        p.set_camera(Range::new(4.0, 5.0), CameraMode::Off);
        assert_eq!(p.camera_mode_at(1.0), CameraMode::Pip);
        assert_eq!(p.camera_mode_at(3.0), CameraMode::Full);
        assert_eq!(p.camera_mode_at(4.5), CameraMode::Off);
        assert_eq!(p.camera_mode_at(6.0), CameraMode::Full);
        assert_eq!(p.camera_ranges.len(), 3);
        assert_eq!(
            p.camera_intervals(CameraMode::Full),
            vec![Range { start: 2.0, end: 4.0 }, Range { start: 5.0, end: 8.0 }]
        );
        assert_eq!(
            p.camera_intervals(CameraMode::Pip),
            vec![Range { start: 0.0, end: 2.0 }, Range { start: 8.0, end: 20.0 }]
        );
    }

    #[test]
    fn later_slides_win() {
        let mut p = project();
        let slide = |start, end| Slide { start, end, file: "a.png".into(), page: 1, clip_start: 0.0 };
        p.slides = vec![slide(0.0, 10.0), slide(2.0, 3.0)];
        assert_eq!(p.slide_at(2.5), Some(1));
        assert_eq!(p.slide_at(5.0), Some(0));
        assert_eq!(p.slide_at(10.0), None);
    }
}
