//! Exports the edited transcript as a Markdown script, e.g. to re-record from.

use std::fmt::Write as _;

use crate::editor::fmt_time;
use crate::project::{Project, Word};

/// A pause this long (seconds) starts a new paragraph.
const PARAGRAPH_GAP: f64 = 1.5;

/// The words that survive the cuts, as paragraphs prefixed with their time in
/// the exported video. Recording pieces (pause/resume) become sections and
/// overlays appear as `[On screen: …]` notes where they start.
pub fn script(p: &Project) -> String {
    let title = p.media.file_stem().and_then(|s| s.to_str()).unwrap_or("Recording");
    let mut out = format!("# {title}\n");
    let heard = p.heard_words();
    let kept: Vec<&Word> = heard
        .iter()
        .filter(|w| p.cut_at((w.start + w.end) / 2.0).is_none())
        .collect();
    let mut overlays: Vec<_> = p
        .overlays
        .iter()
        .filter(|o| p.kept_ranges().iter().any(|r| o.start < r.end && o.end > r.start))
        .collect();
    overlays.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut overlays = overlays.into_iter().peekable();

    let mut prev: Option<&Word> = None;
    let mut line = String::new();
    let flush = |out: &mut String, line: &mut String| {
        if !line.is_empty() {
            let _ = writeln!(out, "{}", line.trim_end());
            line.clear();
        }
    };
    for w in kept {
        let new_section = prev.is_some_and(|pw| p.segment_starts.iter().any(|&b| pw.end <= b && b <= w.start));
        // Measured in the exported video, so a cut doesn't read as a pause.
        let new_paragraph = prev.is_none_or(|pw| p.output_time(w.start) - p.output_time(pw.end) > PARAGRAPH_GAP);
        if new_section || new_paragraph {
            flush(&mut out, &mut line);
            out.push_str(if new_section { "\n---\n\n" } else { "\n" });
            let _ = write!(line, "**[{}]** ", short_time(p.output_time(w.start)));
        }
        while let Some(o) = overlays.next_if(|o| o.start <= w.start) {
            let _ = write!(line, "*[On screen: “{}”]* ", o.text.replace('\n', " / "));
        }
        line.push_str(&w.text);
        line.push(' ');
        prev = Some(w);
    }
    for o in overlays {
        let _ = write!(line, "*[On screen: “{}”]* ", o.text.replace('\n', " / "));
    }
    flush(&mut out, &mut line);
    if heard.is_empty() {
        out.push_str("\n*(no transcript)*\n");
    }
    out
}

/// `mm:ss` (or `h:mm:ss`), without the hundredths `fmt_time` shows.
fn short_time(t: f64) -> String {
    let s = fmt_time(t.floor());
    s.split_once('.').map_or(s.clone(), |(whole, _)| whole.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Overlay, Range};
    use std::path::PathBuf;

    fn word(text: &str, start: f64) -> Word {
        Word { text: text.into(), start, end: start + 0.4 }
    }

    #[test]
    fn builds_paragraphs_sections_and_notes() {
        let mut p = Project {
            media: PathBuf::from("/v/demo.mkv"),
            duration: 30.0,
            width: 640,
            height: 360,
            fps: 30.0,
            has_audio: true,
            segment_starts: vec![10.0],
            words: vec![
                word("Hello", 0.0),
                word("um", 0.5),
                word("world.", 3.0),
                word("Next", 6.0),
                word("bit.", 6.5),
                word("After", 11.0),
                word("resume.", 11.5),
            ],
            overlays: vec![Overlay {
                start: 6.0,
                end: 8.0,
                text: "Title".into(),
                x: 0.5,
                y: 0.5,
                size: 0.1,
                color: [255; 3],
            }],
            ..Default::default()
        };
        // Cutting "um" and the pause after it must not split the paragraph.
        p.add_cut(Range::new(0.45, 2.95));
        assert_eq!(
            script(&p),
            "# demo\n\n\
             **[00:00]** Hello world.\n\n\
             **[00:03]** *[On screen: “Title”]* Next bit.\n\n---\n\n\
             **[00:08]** After resume.\n"
        );
    }
}
