//! Renders the edited video with ffmpeg: overlays are drawn on the source
//! timeline, then the kept ranges are trimmed and concatenated.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::project::Project;

/// Single-quotes a filtergraph option value (paths must not contain ":").
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Builds the `-filter_complex` graph. `text_files[i]` holds overlay i's text.
pub fn filter_graph(p: &Project, text_files: &[PathBuf], font: Option<&Path>) -> String {
    let mut g = String::new();

    // Overlays, applied on source timestamps before trimming.
    let mut chain: Vec<String> = Vec::new();
    for (o, file) in p.overlays.iter().zip(text_files) {
        let font = font.map(|f| format!("fontfile={}:", quote(&f.display().to_string()))).unwrap_or_default();
        let [r, gr, b] = o.color;
        chain.push(format!(
            "drawtext={font}textfile={}:expansion=none:fontsize={}:fontcolor=0x{r:02x}{gr:02x}{b:02x}:\
             shadowcolor=black@0.6:shadowx=2:shadowy=2:\
             x=w*{:.5}-text_w/2:y=h*{:.5}-text_h/2:enable='between(t,{:.3},{:.3})'",
            quote(&file.display().to_string()),
            ((o.size * p.height as f32).round() as u32).max(4),
            o.x,
            o.y,
            o.start,
            o.end,
        ));
    }
    if chain.is_empty() {
        chain.push("null".into());
    }
    let _ = write!(g, "[0:v]{}[vov];", chain.join(","));

    let kept = p.kept_ranges();
    let n = kept.len();
    let _ = write!(g, "[vov]split={n}{};", (0..n).map(|i| format!("[vs{i}]")).collect::<String>());
    if p.has_audio {
        let _ = write!(g, "[0:a]asplit={n}{};", (0..n).map(|i| format!("[as{i}]")).collect::<String>());
    }
    for (i, r) in kept.iter().enumerate() {
        let _ = write!(g, "[vs{i}]trim=start={:.4}:end={:.4},setpts=PTS-STARTPTS[v{i}];", r.start, r.end);
        if p.has_audio {
            let _ = write!(g, "[as{i}]atrim=start={:.4}:end={:.4},asetpts=PTS-STARTPTS[a{i}];", r.start, r.end);
        }
    }
    let inputs: String = (0..n)
        .map(|i| if p.has_audio { format!("[v{i}][a{i}]") } else { format!("[v{i}]") })
        .collect();
    let _ = write!(g, "{inputs}concat=n={n}:v=1:a={}[outv]", u8::from(p.has_audio));
    if p.has_audio {
        g.push_str("[outa]");
    }
    g
}

/// Runs the export, calling `progress` with 0..1. Blocks until done.
pub fn export(p: &Project, out: &Path, mut progress: impl FnMut(f32)) -> Result<()> {
    if p.kept_ranges().is_empty() {
        bail!("everything is cut - nothing to export");
    }
    let tmp = std::env::temp_dir().join(format!("scribecut-export-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let text_files: Vec<PathBuf> = p
        .overlays
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let f = tmp.join(format!("overlay{i}.txt"));
            std::fs::write(&f, &o.text).map(|_| f)
        })
        .collect::<std::io::Result<_>>()?;
    let font = crate::media::overlay_font();
    let graph = filter_graph(p, &text_files, font.as_deref());

    let mut cmd = Command::new("ffmpeg");
    // -nostdin + null stdin: ffmpeg must not touch the terminal, or a
    // backgrounded scribecut gets stopped by SIGTTOU.
    cmd.stdin(Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"])
        .arg(&p.media)
        .args(["-filter_complex", &graph, "-map", "[outv]"]);
    if p.has_audio {
        cmd.args(["-map", "[outa]", "-c:a", "aac", "-b:a", "160k"]);
    }
    cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"])
        .args(["-movflags", "+faststart"])
        .arg(out)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().context("running ffmpeg")?;
    let total = p.output_duration().max(0.001);
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        if let Some(us) = line?.strip_prefix("out_time_us=").and_then(|v| v.parse::<f64>().ok()) {
            progress(((us / 1e6) / total).clamp(0.0, 1.0) as f32);
        }
    }
    let output = child.wait_with_output()?;
    let _ = std::fs::remove_dir_all(&tmp);
    if !output.status.success() {
        bail!("ffmpeg export failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    progress(1.0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Overlay, Range};
    use std::process::Command;

    fn project() -> Project {
        Project {
            media: PathBuf::from("in.mkv"),
            duration: 20.0,
            width: 1280,
            height: 720,
            fps: 30.0,
            has_audio: true,
            segment_starts: vec![],
            words: vec![],
            cuts: vec![],
            overlays: vec![],
        }
    }

    #[test]
    fn graph_without_edits_is_single_piece() {
        let g = filter_graph(&project(), &[], None);
        assert_eq!(
            g,
            "[0:v]null[vov];[vov]split=1[vs0];[0:a]asplit=1[as0];\
             [vs0]trim=start=0.0000:end=20.0000,setpts=PTS-STARTPTS[v0];\
             [as0]atrim=start=0.0000:end=20.0000,asetpts=PTS-STARTPTS[a0];\
             [v0][a0]concat=n=1:v=1:a=1[outv][outa]"
        );
    }

    #[test]
    fn graph_with_cut_and_overlay() {
        let mut p = project();
        p.has_audio = false;
        p.add_cut(Range::new(5.0, 8.0));
        p.overlays.push(Overlay {
            start: 1.0,
            end: 3.5,
            text: "hi".into(),
            x: 0.25,
            y: 0.75,
            size: 0.1,
            color: [255, 0, 16],
        });
        let g = filter_graph(&p, &[PathBuf::from("/t/o0.txt")], Some(Path::new("/f/a b.ttf")));
        assert!(g.starts_with("[0:v]drawtext=fontfile='/f/a b.ttf':textfile='/t/o0.txt':"));
        assert!(g.contains("fontsize=72:fontcolor=0xff0010:"));
        assert!(g.contains("x=w*0.25000-text_w/2:y=h*0.75000-text_h/2:enable='between(t,1.000,3.500)'[vov]"));
        assert!(g.contains("[vs0]trim=start=0.0000:end=5.0000"));
        assert!(g.contains("[vs1]trim=start=8.0000:end=20.0000"));
        assert!(g.ends_with("[v0][v1]concat=n=2:v=1:a=0[outv]"));
        assert!(!g.contains("asplit"));
    }

    /// Renders a real clip with ffmpeg. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn exports_real_clip() {
        let dir = std::env::temp_dir().join(format!("scribecut-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.mkv");
        let ok = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=20"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20", "-c:v", "libx264", "-c:a", "libopus"])
            .arg(&src)
            .status()
            .unwrap()
            .success();
        assert!(ok);

        let mut p = crate::editor::new_project(&src, vec![]).unwrap();
        assert!((p.duration - 20.0).abs() < 0.1);
        p.add_cut(Range::new(5.0, 9.0));
        p.overlays.push(Overlay {
            start: 1.0,
            end: 4.0,
            text: "Hello: it's 100%".into(),
            x: 0.5,
            y: 0.5,
            size: 0.1,
            color: [255, 255, 0],
        });
        let out = dir.join("out.mp4");
        let mut last = 0.0;
        export(&p, &out, |f| last = f).unwrap();
        assert_eq!(last, 1.0);
        let info = crate::media::probe(&out).unwrap();
        assert!((info.duration - 16.0).abs() < 0.15, "duration {}", info.duration);
        assert!(info.has_audio);

        // The overlay must change pixels at t=2 compared to the source frame.
        let grab = |file: &Path, t: &str| {
            Command::new("ffmpeg")
                .args(["-v", "error", "-ss", t, "-i"])
                .arg(file)
                .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .output()
                .unwrap()
                .stdout
        };
        assert_ne!(grab(&src, "2"), grab(&out, "2"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
