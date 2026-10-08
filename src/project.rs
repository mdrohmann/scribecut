//! Project data model: media file, transcript words, cuts and text overlays.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
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

    pub fn contains(&self, t: f64) -> bool {
        t >= self.start && t < self.end
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub media: PathBuf,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    /// Start times of each recorded piece after the first (pause/resume points).
    #[serde(default)]
    pub segment_starts: Vec<f64>,
    #[serde(default)]
    pub words: Vec<Word>,
    #[serde(default)]
    pub cuts: Vec<Range>,
    #[serde(default)]
    pub overlays: Vec<Overlay>,
}

impl Project {
    /// Where the project for a given media file lives: `<media>.scribecut.json`.
    pub fn path_for(media: &Path) -> PathBuf {
        media.with_extension("scribecut.json")
    }

    pub fn load(path: &Path) -> Result<Self> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path_for(&self.media);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Adds a cut, merging it with any cuts it overlaps or touches.
    pub fn add_cut(&mut self, cut: Range) {
        let cut = Range::new(cut.start.max(0.0), cut.end.min(self.duration));
        if cut.len() <= 0.0 {
            return;
        }
        self.cuts.push(cut);
        self.cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
        let mut merged: Vec<Range> = Vec::with_capacity(self.cuts.len());
        for c in self.cuts.drain(..) {
            match merged.last_mut() {
                Some(last) if c.start <= last.end => last.end = last.end.max(c.end),
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

    /// Index of the word being spoken at `t` (or the last one before it).
    pub fn word_at(&self, t: f64) -> Option<usize> {
        let i = self.words.partition_point(|w| w.start <= t);
        (i > 0).then(|| i - 1).filter(|&i| t < self.words[i].end.max(self.words[i].start + 0.05) + 0.5)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project {
            media: PathBuf::from("/tmp/x.mkv"),
            duration: 20.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            has_audio: true,
            segment_starts: vec![],
            words: vec![],
            cuts: vec![],
            overlays: vec![],
        }
    }

    #[test]
    fn cuts_merge_and_clamp() {
        let mut p = project();
        p.add_cut(Range::new(5.0, 3.0));
        p.add_cut(Range::new(4.0, 6.0));
        p.add_cut(Range::new(18.0, 25.0));
        p.add_cut(Range::new(10.0, 10.0));
        assert_eq!(p.cuts, vec![Range { start: 3.0, end: 6.0 }, Range { start: 18.0, end: 20.0 }]);
        assert_eq!(
            p.kept_ranges(),
            vec![Range { start: 0.0, end: 3.0 }, Range { start: 6.0, end: 18.0 }]
        );
        assert_eq!(p.output_duration(), 15.0);
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
        assert_eq!(p.word_at(0.5), None);
        assert_eq!(p.word_at(1.2), Some(0));
        assert_eq!(p.word_at(2.1), Some(1));
        assert_eq!(p.word_at(10.0), None);
    }

    #[test]
    fn json_round_trip() {
        let mut p = project();
        p.words.push(Word { text: "hi".into(), start: 0.1, end: 0.3 });
        p.overlays.push(Overlay {
            start: 1.0,
            end: 2.0,
            text: "x".into(),
            x: 0.5,
            y: 0.5,
            size: 0.06,
            color: [255, 255, 255],
        });
        let back: Project = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
    }
}
