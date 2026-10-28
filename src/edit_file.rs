//! The on-disk project: `<media>.edit.toml` (everything editable, meant to be
//! changed by hand or by an agent) and `<media>.transcript.json` / `.txt`
//! (generated word timings).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::editor::fmt_time;
use crate::project::{
    CameraMode, CameraRange, CameraSettings, Corner, Cut, Overlay, Project, Range, Slide, Voiceover, Word,
};

const HEADER: &str = r##"# scribecut edit file. Safe to edit by hand or by an agent while scribecut is
# open: the app reloads it. Check your changes with `scribecut <this file> --check`.
#
# Times ("from", "to", ...) are "m:ss.s" or "h:mm:ss.s" strings (plain numbers are
# seconds), always on the ORIGINAL recording's timeline - cuts don't shift them.
# The words and their times are in the .transcript.txt / .transcript.json files
# next to this one. Relative file paths are relative to this file's folder.
#
# [source]          written by scribecut, don't edit
# [camera]          default = "off" | "pip" | "full" where no [[camera_range]] applies;
#                   pip = small window: corner ("top-left", "top-right", "bottom-left",
#                   "bottom-right"), width and margin as fractions of the frame width
# [[cut]]           from, to, note (optional) - removed from the exported video
# [[slide]]         from, to, file, page (PDF/presentation, 1-based), clip_start (video
#                   clips) - shown INSTEAD of the screen recording. file: .png .jpg .pdf
#                   .pptx .odp .key .mp4 .mkv .webm .mov (clip audio is not used)
# [[camera_range]]  from, to, mode = "full" (only me) | "pip" (small window) | "off"
# [[overlay]]       from, to, text, x, y (0..1, centre of the text), size (fraction of
#                   the frame height), color ("#rrggbb")
# [[voiceover]]     from, to, file, kept - narration recorded again in scribecut,
#                   heard instead of the recording's own sound. It was spoken over the
#                   edited video: the file holds the kept ranges listed in kept, back to
#                   back. Cuts added inside from..to later remove narration and picture
#                   together; a part of from..to outside kept has no narration.
#                   Its words are in <file>.transcript.json/.txt, on the file's timeline.
# Layering: screen recording < slide < camera < overlay text.

"##;

/// A time on the source timeline, written as "m:ss.ss".
#[derive(Clone, Copy, Debug, PartialEq)]
struct Time(f64);

impl Serialize for Time {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&fmt_time(self.0))
    }
}

impl<'de> Deserialize<'de> for Time {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(f64),
            Int(i64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Num(n) => Ok(Time(n)),
            Raw::Int(n) => Ok(Time(n as f64)),
            Raw::Str(s) => parse_time(&s).map(Time).map_err(serde::de::Error::custom),
        }
    }
}

/// Parses "h:mm:ss.s", "m:ss.s" or "ss.s".
pub fn parse_time(s: &str) -> Result<f64> {
    let mut total = 0.0;
    for part in s.trim().split(':') {
        let v: f64 = part.trim().parse().with_context(|| format!("bad time \"{s}\" (use m:ss.s)"))?;
        total = total * 60.0 + v;
    }
    if total < 0.0 || !total.is_finite() {
        bail!("bad time \"{s}\"");
    }
    Ok(total)
}

#[derive(Serialize, Deserialize)]
struct EditFile {
    source: SourceDto,
    #[serde(default)]
    camera: CameraDto,
    #[serde(default, rename = "cut", skip_serializing_if = "Vec::is_empty")]
    cuts: Vec<CutDto>,
    #[serde(default, rename = "slide", skip_serializing_if = "Vec::is_empty")]
    slides: Vec<SlideDto>,
    #[serde(default, rename = "camera_range", skip_serializing_if = "Vec::is_empty")]
    camera_ranges: Vec<CameraRangeDto>,
    #[serde(default, rename = "overlay", skip_serializing_if = "Vec::is_empty")]
    overlays: Vec<OverlayDto>,
    #[serde(default, rename = "voiceover", skip_serializing_if = "Vec::is_empty")]
    voiceovers: Vec<VoiceoverDto>,
}

#[derive(Serialize, Deserialize)]
struct SourceDto {
    media: PathBuf,
    duration: Time,
    size: [u32; 2],
    fps: f64,
    has_audio: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    camera_size: Option<[u32; 2]>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    resumed_at: Vec<Time>,
}

#[derive(Serialize, Deserialize)]
struct CameraDto {
    #[serde(default = "default_mode")]
    default: String,
    #[serde(default)]
    pip: PipDto,
}

fn default_mode() -> String {
    "off".into()
}

impl Default for CameraDto {
    fn default() -> Self {
        Self { default: default_mode(), pip: PipDto::default() }
    }
}

#[derive(Serialize, Deserialize)]
struct PipDto {
    corner: String,
    width: f32,
    margin: f32,
}

impl Default for PipDto {
    fn default() -> Self {
        let d = CameraSettings::default();
        Self { corner: d.corner.name().into(), width: d.width, margin: d.margin }
    }
}

#[derive(Serialize, Deserialize)]
struct CutDto {
    from: Time,
    to: Time,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    note: String,
}

#[derive(Serialize, Deserialize)]
struct SlideDto {
    from: Time,
    to: Time,
    file: PathBuf,
    #[serde(default = "first_page", skip_serializing_if = "is_first_page")]
    page: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clip_start: Option<Time>,
}

fn first_page() -> u32 {
    1
}

fn is_first_page(p: &u32) -> bool {
    *p == 1
}

#[derive(Serialize, Deserialize)]
struct CameraRangeDto {
    from: Time,
    to: Time,
    mode: String,
}

#[derive(Serialize, Deserialize)]
struct OverlayDto {
    from: Time,
    to: Time,
    text: String,
    x: f32,
    y: f32,
    size: f32,
    color: String,
}

#[derive(Serialize, Deserialize)]
struct VoiceoverDto {
    from: Time,
    to: Time,
    file: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    kept: Vec<[Time; 2]>,
    /// Older files: seconds recorded over, instead of `kept`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    length: Option<f64>,
}

fn color_hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn parse_color(s: &str) -> Result<[u8; 3]> {
    let hex = s.trim().trim_start_matches('#');
    let v = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6).with_context(|| format!("bad color \"{s}\" (use #rrggbb)"))?;
    Ok([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn range(from: Time, to: Time) -> (f64, f64) {
    (from.0.min(to.0), from.0.max(to.0))
}

/// See `Project::load`.
pub fn load(path: &Path) -> Result<Project> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("toml") => load_toml(path),
        Some("json") => migrate(path),
        _ => {
            let edit = Project::path_for(path);
            let legacy = path.with_extension("scribecut.json");
            if edit.is_file() {
                load_toml(&edit)
            } else if legacy.is_file() {
                migrate(&legacy)
            } else {
                bail!("no edit file for {}", path.display())
            }
        }
    }
}

fn load_toml(path: &Path) -> Result<Project> {
    // Absolute, so a file named relative to the current folder still gives
    // an absolute media path (playback needs one).
    let path = &std::path::absolute(path)?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut p = from_toml(&text, path.parent().unwrap_or(Path::new(".")))
        .with_context(|| format!("in {}", path.display()))?;
    let transcript = transcript_path(&p.media);
    if transcript.is_file() {
        let data = std::fs::read(&transcript)?;
        p.words = serde_json::from_slice(&data).with_context(|| format!("parsing {}", transcript.display()))?;
    }
    for v in &mut p.voiceovers {
        let transcript = transcript_path(&p.media.parent().unwrap_or(Path::new(".")).join(&v.file));
        if transcript.is_file() {
            let data = std::fs::read(&transcript)?;
            v.words = serde_json::from_slice(&data).with_context(|| format!("parsing {}", transcript.display()))?;
        }
    }
    Ok(p)
}

fn from_toml(text: &str, dir: &Path) -> Result<Project> {
    let f: EditFile = toml::from_str(text)?;
    let s = f.source;
    let camera = CameraSettings {
        default: CameraMode::parse(&f.camera.default)
            .with_context(|| format!("camera.default: \"{}\" is not off, pip or full", f.camera.default))?,
        corner: Corner::parse(&f.camera.pip.corner)
            .with_context(|| format!("camera.pip.corner: unknown corner \"{}\"", f.camera.pip.corner))?,
        width: f.camera.pip.width.clamp(0.05, 1.0),
        margin: f.camera.pip.margin.clamp(0.0, 0.5),
    };
    let camera_ranges = f
        .camera_ranges
        .into_iter()
        .enumerate()
        .map(|(i, r)| {
            let (start, end) = range(r.from, r.to);
            let mode = CameraMode::parse(&r.mode)
                .with_context(|| format!("camera_range {}: mode \"{}\" is not off, pip or full", i + 1, r.mode))?;
            Ok(CameraRange { start, end, mode })
        })
        .collect::<Result<_>>()?;
    let overlays = f
        .overlays
        .into_iter()
        .enumerate()
        .map(|(i, o)| {
            let (start, end) = range(o.from, o.to);
            let color = parse_color(&o.color).with_context(|| format!("overlay {}", i + 1))?;
            Ok(Overlay { start, end, text: o.text, x: o.x, y: o.y, size: o.size, color })
        })
        .collect::<Result<_>>()?;
    let mut cuts: Vec<Cut> = f
        .cuts
        .into_iter()
        .map(|c| {
            let (start, end) = range(c.from, c.to);
            Cut { start, end, note: c.note }
        })
        .collect();
    cuts.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut p = Project {
        media: dir.join(&s.media),
        duration: s.duration.0,
        width: s.size[0],
        height: s.size[1],
        fps: s.fps,
        has_audio: s.has_audio,
        camera_size: s.camera_size.map(|[w, h]| (w, h)),
        segment_starts: s.resumed_at.into_iter().map(|t| t.0).collect(),
        words: Vec::new(),
        cuts: Vec::new(),
        overlays,
        slides: f
            .slides
            .into_iter()
            .map(|sl| {
                let (start, end) = range(sl.from, sl.to);
                Slide { start, end, file: sl.file, page: sl.page.max(1), clip_start: sl.clip_start.map_or(0.0, |t| t.0) }
            })
            .collect(),
        camera,
        camera_ranges,
        voiceovers: Vec::new(),
    };
    // Through add_cut so overlapping cuts written by hand are merged.
    for c in cuts {
        p.add_cut_with_note(Range { start: c.start, end: c.end }, c.note);
    }
    for v in f.voiceovers {
        let (start, end) = range(v.from, v.to);
        let mut take = Voiceover { start, end, file: v.file, pieces: Vec::new(), stale_length: None, words: Vec::new() };
        if v.kept.is_empty() {
            // Older files: assume the cuts it was recorded with are the ones
            // now, unless the length says otherwise.
            let now = p.kept_pieces(Range { start, end });
            take.pieces = now.iter().map(|(r, _)| *r).collect();
            let kept: f64 = take.pieces.iter().map(Range::len).sum();
            take.stale_length = v.length.filter(|l| (l - kept).abs() > 0.05);
        } else {
            take.pieces = v.kept.iter().map(|[a, b]| Range::new(a.0, b.0)).collect();
        }
        p.voiceovers.push(take);
    }
    p.voiceovers.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(p)
}

fn to_toml(p: &Project) -> Result<String> {
    let base = p.base_dir();
    let rel = |path: &Path| path.strip_prefix(&base).map(Path::to_path_buf).unwrap_or_else(|_| path.to_path_buf());
    let f = EditFile {
        source: SourceDto {
            media: rel(&p.media),
            duration: Time(p.duration),
            size: [p.width, p.height],
            fps: p.fps,
            has_audio: p.has_audio,
            camera_size: p.camera_size.map(|(w, h)| [w, h]),
            resumed_at: p.segment_starts.iter().map(|&t| Time(t)).collect(),
        },
        camera: CameraDto {
            default: p.camera.default.name().into(),
            pip: PipDto { corner: p.camera.corner.name().into(), width: p.camera.width, margin: p.camera.margin },
        },
        cuts: p.cuts.iter().map(|c| CutDto { from: Time(c.start), to: Time(c.end), note: c.note.clone() }).collect(),
        slides: p
            .slides
            .iter()
            .map(|s| SlideDto {
                from: Time(s.start),
                to: Time(s.end),
                file: s.file.clone(),
                page: s.page,
                clip_start: (s.clip_start > 0.0).then_some(Time(s.clip_start)),
            })
            .collect(),
        camera_ranges: p
            .camera_ranges
            .iter()
            .map(|r| CameraRangeDto { from: Time(r.start), to: Time(r.end), mode: r.mode.name().into() })
            .collect(),
        overlays: p
            .overlays
            .iter()
            .map(|o| OverlayDto {
                from: Time(o.start),
                to: Time(o.end),
                text: o.text.clone(),
                x: o.x,
                y: o.y,
                size: o.size,
                color: color_hex(o.color),
            })
            .collect(),
        voiceovers: p
            .voiceovers
            .iter()
            .map(|v| VoiceoverDto {
                from: Time(v.start),
                to: Time(v.end),
                file: v.file.clone(),
                kept: if v.stale_length.is_some() {
                    Vec::new()
                } else {
                    v.pieces.iter().map(|r| [Time(r.start), Time(r.end)]).collect()
                },
                length: v.stale_length,
            })
            .collect(),
    };
    Ok(format!("{HEADER}{}", toml::to_string(&f)?))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    // A temporary name of its own per write: two writers (a finished
    // transcription and the UI, or two transcriptions of one file) must not
    // share one, or a rename could move the other's half-written file.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().context("no file name")?.to_owned();
    name.push(format!(".{}-{n}.tmp", std::process::id()));
    let tmp = path.with_file_name(name);
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// See `Project::save`.
pub fn save(p: &Project) -> Result<()> {
    write_atomic(&Project::path_for(&p.media), to_toml(p)?.as_bytes())
}

/// Writes the words as JSON (for scribecut) and as readable timed lines (for people and agents).
pub fn save_transcript(p: &Project) -> Result<()> {
    write_transcript(&p.media, &p.words)
}

/// [`save_transcript`] for any video, with or without an edit file.
pub fn write_transcript(media: &Path, words: &[Word]) -> Result<()> {
    write_atomic(&transcript_path(media), &serde_json::to_vec_pretty(words)?)?;
    write_atomic(&media.with_extension("transcript.txt"), transcript_text(words).as_bytes())
}

/// Where a video's word timings are kept.
pub fn transcript_path(media: &Path) -> PathBuf {
    media.with_extension("transcript.json")
}

/// One line per phrase (split at pauses over a second or every 15 words),
/// each starting with its time: `[02:19.36] and we want to give ...`.
fn transcript_text(words: &[Word]) -> String {
    let mut out = String::from(
        "# Generated by scribecut - edit the .edit.toml file, not this one.\n\
         # [time] on the original recording's timeline, as used in the edit file.\n",
    );
    let mut count = 0;
    for (i, w) in words.iter().enumerate() {
        let new_line = i == 0 || w.start - words[i - 1].end > 1.0 || count >= 15;
        if new_line {
            if i > 0 {
                out.push('\n');
            }
            let _ = write!(out, "[{}]", fmt_time(w.start));
            count = 0;
        }
        out.push(' ');
        out.push_str(&w.text);
        count += 1;
    }
    out.push('\n');
    out
}

/// Converts an old `.scribecut.json` project, keeping it as `.json.bak`.
fn migrate(json: &Path) -> Result<Project> {
    #[derive(Deserialize)]
    struct Legacy {
        media: PathBuf,
        duration: f64,
        width: u32,
        height: u32,
        fps: f64,
        has_audio: bool,
        #[serde(default)]
        segment_starts: Vec<f64>,
        #[serde(default)]
        words: Vec<Word>,
        #[serde(default)]
        cuts: Vec<Range>,
        #[serde(default)]
        overlays: Vec<Overlay>,
    }
    let data = std::fs::read(json).with_context(|| format!("reading {}", json.display()))?;
    let l: Legacy = serde_json::from_slice(&data).with_context(|| format!("parsing {}", json.display()))?;
    let mut p = Project {
        media: l.media,
        duration: l.duration,
        width: l.width,
        height: l.height,
        fps: l.fps,
        has_audio: l.has_audio,
        segment_starts: l.segment_starts,
        words: l.words,
        overlays: l.overlays,
        ..Default::default()
    };
    for c in l.cuts {
        p.add_cut(c);
    }
    p.save()?;
    p.save_transcript()?;
    std::fs::rename(json, json.with_extension("json.bak"))?;
    Ok(p)
}

/// Problems an agent (or person) should fix after editing; empty if fine.
pub fn validate(p: &Project) -> Vec<String> {
    let mut problems = Vec::new();
    let mut check_range = |what: String, start: f64, end: f64| {
        if end - start < 0.01 {
            problems.push(format!("{what}: empty time range ({} - {})", fmt_time(start), fmt_time(end)));
        }
        if end > p.duration + 0.01 {
            problems.push(format!("{what}: ends at {} after the video ({})", fmt_time(end), fmt_time(p.duration)));
        }
    };
    for (i, c) in p.cuts.iter().enumerate() {
        check_range(format!("cut {}", i + 1), c.start, c.end);
    }
    for (i, o) in p.overlays.iter().enumerate() {
        check_range(format!("overlay {}", i + 1), o.start, o.end);
    }
    for (i, r) in p.camera_ranges.iter().enumerate() {
        check_range(format!("camera_range {}", i + 1), r.start, r.end);
    }
    for (i, s) in p.slides.iter().enumerate() {
        check_range(format!("slide {}", i + 1), s.start, s.end);
    }
    for (i, v) in p.voiceovers.iter().enumerate() {
        check_range(format!("voiceover {}", i + 1), v.start, v.end);
    }
    for (i, s) in p.slides.iter().enumerate() {
        if let Err(e) = crate::sources::check(p, s) {
            problems.push(format!("slide {}: {e:#}", i + 1));
        }
    }
    for (i, v) in p.voiceovers.iter().enumerate() {
        if !p.resolve(&v.file).is_file() {
            problems.push(format!("voiceover {}: file {} not found", i + 1, v.file.display()));
        }
        if let Some(problem) = p.voiceover_problem(v) {
            problems.push(format!("voiceover {}: {problem}", i + 1));
        }
        if p.voiceovers[..i].iter().any(|o| o.end > v.start && o.start < v.end) {
            problems.push(format!("voiceover {}: overlaps an earlier voiceover", i + 1));
        }
    }
    if p.camera_size.is_none() && (!p.camera_ranges.is_empty() || p.camera.default != CameraMode::Off) {
        problems.push("camera settings are ignored: this recording has no camera track".into());
    }
    for (i, o) in p.overlays.iter().enumerate() {
        if !(0.0..=1.0).contains(&o.x) || !(0.0..=1.0).contains(&o.y) {
            problems.push(format!("overlay {}: x and y must be between 0 and 1", i + 1));
        }
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("2:19.3").unwrap(), 139.3);
        assert_eq!(parse_time("1:02:03.5").unwrap(), 3723.5);
        assert_eq!(parse_time("12.5").unwrap(), 12.5);
        assert!(parse_time("1:xx").is_err());
        let t: Time = toml::from_str::<std::collections::HashMap<String, Time>>("a = 75").unwrap()["a"];
        assert_eq!(t, Time(75.0));
    }

    fn sample() -> Project {
        Project {
            media: PathBuf::from("/rec/recording.mkv"),
            duration: 120.0,
            width: 1920,
            height: 1080,
            fps: 30.0,
            has_audio: true,
            camera_size: Some((1280, 720)),
            segment_starts: vec![60.5],
            words: vec![Word { text: "hi".into(), start: 1.0, end: 1.2 }],
            cuts: vec![Cut { start: 10.0, end: 12.5, note: "first take".into() }],
            overlays: vec![Overlay { start: 1.0, end: 2.0, text: "T".into(), x: 0.5, y: 0.8, size: 0.06, color: [255, 16, 0] }],
            slides: vec![Slide { start: 20.0, end: 30.0, file: "slides/deck.pdf".into(), page: 3, clip_start: 0.0 }],
            camera: CameraSettings { default: CameraMode::Pip, ..Default::default() },
            camera_ranges: vec![CameraRange { start: 20.0, end: 25.0, mode: CameraMode::Full }],
            voiceovers: vec![Voiceover {
                start: 40.0,
                end: 50.5,
                file: "voiceovers/take-1.flac".into(),
                pieces: vec![Range { start: 40.0, end: 50.5 }],
                stale_length: None,
                words: vec![],
            }],
        }
    }

    #[test]
    fn toml_round_trip() {
        let p = sample();
        let text = to_toml(&p).unwrap();
        assert!(text.starts_with("# scribecut edit file"));
        assert!(text.contains("media = \"recording.mkv\""), "{text}");
        assert!(text.contains("from = \"00:10.00\""), "{text}");
        assert!(text.contains("color = \"#ff1000\""), "{text}");
        let mut back = from_toml(&text, Path::new("/rec")).unwrap();
        back.words = p.words.clone();
        assert_eq!(back, p);
    }

    #[test]
    fn hand_written_file() {
        let text = r##"
            [source]
            media = "v.mkv"
            duration = "2:00"
            size = [640, 360]
            fps = 30
            has_audio = true

            [[cut]]
            from = "0:10"
            to = "0:05"

            [[cut]]
            from = 8
            to = "0:20"

            [[slide]]
            from = "1:00"
            to = "1:10"
            file = "intro.png"
        "##;
        let p = from_toml(text, Path::new("/d")).unwrap();
        assert_eq!(p.media, PathBuf::from("/d/v.mkv"));
        assert_eq!(p.cuts.len(), 1, "overlapping cuts merge");
        assert_eq!((p.cuts[0].start, p.cuts[0].end), (5.0, 20.0));
        assert_eq!(p.slides[0].page, 1);
        assert_eq!(p.camera.default, CameraMode::Off);
        let bad = text.replace("from = \"1:00\"", "from = \"1:zz\"");
        assert!(from_toml(&bad, Path::new("/d")).unwrap_err().to_string().contains("1:zz"));
    }

    #[test]
    fn validation_messages() {
        let mut p = sample();
        p.slides[0].file = "missing.png".into();
        p.cuts.push(Cut { start: 110.0, end: 130.0, note: String::new() });
        p.voiceovers[0].pieces = vec![Range { start: 40.0, end: 45.0 }, Range { start: 46.0, end: 50.5 }];
        let problems = validate(&p);
        assert!(problems.iter().any(|m| m.starts_with("voiceover 1: file") && m.contains("not found")), "{problems:?}");
        assert!(problems.iter().any(|m| m.starts_with("voiceover 1: 1.0s inside it has no narration")), "{problems:?}");
        assert!(problems.iter().any(|m| m.starts_with("slide 1:") && m.contains("not found")), "{problems:?}");
        assert!(problems.iter().any(|m| m.starts_with("cut 2:") && m.contains("after the video")), "{problems:?}");
    }

    #[test]
    fn older_voiceovers_without_kept_ranges() {
        let text = |length: &str| {
            format!(
                "[source]\nmedia = \"v.mkv\"\nduration = 60\nsize = [640, 360]\nfps = 30\nhas_audio = true\n\n\
                 [[cut]]\nfrom = 10\nto = 12\n\n\
                 [[voiceover]]\nfrom = 5\nto = 20\nfile = \"t.flac\"\nlength = {length}\n"
            )
        };
        // Still matching the cuts: the pieces are the kept parts, written out from then on.
        let p = from_toml(&text("13.0"), Path::new("/d")).unwrap();
        assert_eq!(p.voiceovers[0].pieces, vec![Range { start: 5.0, end: 10.0 }, Range { start: 12.0, end: 20.0 }]);
        assert_eq!(p.voiceover_problem(&p.voiceovers[0]), None);
        assert!(to_toml(&p).unwrap().contains("kept = [["));
        // Not matching: reported, and kept as it was.
        let p = from_toml(&text("15.0"), Path::new("/d")).unwrap();
        assert!(p.voiceover_problem(&p.voiceovers[0]).unwrap().contains("record it again"));
        let back = to_toml(&p).unwrap();
        assert!(back.contains("length = 15.0") && !back.contains("kept ="), "{back}");
    }

    #[test]
    fn transcript_of_a_video_without_an_edit_file() {
        let dir = std::env::temp_dir().join(format!("scribecut-transcript-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = dir.join("export.mp4");
        let words = vec![Word { text: "Hi".into(), start: 0.5, end: 0.9 }];
        write_transcript(&video, &words).unwrap();
        write_transcript(&video, &words).unwrap(); // replaces
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        // Both files, no temporaries left behind, and no edit file.
        assert_eq!(names, ["export.transcript.json", "export.transcript.txt"]);
        let back: Vec<Word> = serde_json::from_slice(&std::fs::read(transcript_path(&video)).unwrap()).unwrap();
        assert_eq!(back.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcript_lines() {
        let w = |t: &str, s| Word { text: t.into(), start: s, end: s + 0.3 };
        let text = transcript_text(&[w("a", 0.0), w("b", 0.4), w("c", 5.0)]);
        assert!(text.ends_with("[00:00.00] a b\n[00:05.00] c\n"), "{text}");
    }
}
