//! Renders the edited video with ffmpeg: overlays are drawn on the source
//! timeline, then the kept ranges are trimmed and concatenated.

use std::fmt::Write as _;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::project::{CameraMode, Corner, Project};
use crate::sources::{self, Input};

/// Single-quotes a filtergraph option value (paths must not contain ":").
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `between(t,a,b)+…` for an overlay's `enable` option.
fn enable(ranges: impl IntoIterator<Item = (f64, f64)>) -> String {
    let parts: Vec<String> = ranges.into_iter().map(|(a, b)| format!("between(t,{a:.3},{b:.3})")).collect();
    if parts.is_empty() { "0".into() } else { parts.join("+") }
}

/// Scales a picture to fit the frame, letterboxed on black.
fn fit(w: u32, h: u32) -> String {
    format!("scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1")
}

/// Builds the `-filter_complex` graph. Input 0 is the recording (camera = its
/// 2nd video stream); input `i + 1` is slide `i`, a video clip if `clips[i]`.
/// `text_files[i]` holds overlay i's text.
/// `audio_cleanup` is an ffmpeg filter chain applied to the finished
/// programme, after the kept ranges have been joined, so that leveling and
/// loudness see exactly what the viewer will hear rather than each piece
/// separately.
pub fn filter_graph(
    p: &Project,
    clips: &[bool],
    text_files: &[PathBuf],
    font: Option<&Path>,
    audio_cleanup: Option<&str>,
) -> String {
    let mut g = String::new();
    let (w, h) = (p.width, p.height);
    // Everything below is composed on the source timeline, then trimmed.
    let mut base = "[0:v:0]".to_string();
    let mut step = 0;
    let mut layer = |g: &mut String, base: &mut String, top: &str, options: String| {
        let _ = write!(g, "{base}{top}overlay=eof_action=pass:{options}[b{step}];");
        *base = format!("[b{step}]");
        step += 1;
    };

    // Slides replace the screen recording.
    for (i, (s, &clip)) in p.slides.iter().zip(clips).enumerate() {
        let shift = if clip { format!(",setpts=PTS-STARTPTS+{:.3}/TB", s.start) } else { String::new() };
        let _ = write!(g, "[{}:v]{},fps={}{shift}[s{i}];", i + 1, fit(w, h), p.fps);
        layer(&mut g, &mut base, &format!("[s{i}]"), format!("enable='{}'", enable([(s.start, s.end)])));
    }

    // Camera: full frame and/or a small window, on top of slides.
    if p.camera_size.is_some() {
        let full = p.camera_intervals(CameraMode::Full);
        let pip = p.camera_intervals(CameraMode::Pip);
        let wanted: Vec<&str> = [(!full.is_empty()).then_some("[cf]"), (!pip.is_empty()).then_some("[cp]")]
            .into_iter()
            .flatten()
            .collect();
        if !wanted.is_empty() {
            let _ = write!(g, "[0:v:1]split={}{};", wanted.len(), wanted.concat());
        }
        if !full.is_empty() {
            let _ = write!(g, "[cf]{}[camfull];", fit(w, h));
            layer(&mut g, &mut base, "[camfull]", format!("enable='{}'", enable(full.iter().map(|r| (r.start, r.end)))));
        }
        if !pip.is_empty() {
            let c = p.camera;
            let pw = ((w as f32 * c.width / 2.0).round() as u32 * 2).max(2);
            let m = (w as f32 * c.margin).round() as u32;
            let x = if matches!(c.corner, Corner::TopLeft | Corner::BottomLeft) { format!("{m}") } else { format!("main_w-overlay_w-{m}") };
            let y = if matches!(c.corner, Corner::TopLeft | Corner::TopRight) { format!("{m}") } else { format!("main_h-overlay_h-{m}") };
            let _ = write!(g, "[cp]scale={pw}:-2,setsar=1[campip];");
            layer(&mut g, &mut base, "[campip]", format!("x={x}:y={y}:enable='{}'", enable(pip.iter().map(|r| (r.start, r.end)))));
        }
    }

    // Text overlays on top of everything.
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
    let _ = write!(g, "{base}{}[vov];", chain.join(","));

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
        match audio_cleanup {
            Some(cleanup) => {
                let _ = write!(g, "[acat];[acat]{cleanup}[outa]");
            }
            None => g.push_str("[outa]"),
        }
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
    let inputs: Vec<Input> = p
        .slides
        .iter()
        .enumerate()
        .map(|(i, s)| sources::export_input(p, s).with_context(|| format!("slide {}", i + 1)))
        .collect::<Result<_>>()?;
    let clips: Vec<bool> = inputs.iter().map(|i| matches!(i, Input::Clip { .. })).collect();
    let cleanup = crate::audio::cleanup(&p.media);
    let graph = filter_graph(p, &clips, &text_files, font.as_deref(), cleanup.as_deref());

    let mut cmd = crate::media::ffmpeg();
    // -nostdin + null stdin: ffmpeg must not touch the terminal, or a
    // backgrounded scribecut gets stopped by SIGTTOU.
    cmd.stdin(Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"])
        .arg(&p.media);
    for (input, slide) in inputs.iter().zip(&p.slides) {
        match input {
            Input::Still(img) => cmd
                .args(["-loop", "1", "-framerate", &format!("{}", p.fps), "-t", &format!("{:.3}", p.duration), "-i"])
                .arg(img),
            Input::Clip { path, start } => cmd
                .args(["-ss", &format!("{start:.3}"), "-t", &format!("{:.3}", slide.end - slide.start), "-i"])
                .arg(path),
        };
    }
    cmd.args(["-filter_complex", &graph, "-map", "[outv]"]);
    if p.has_audio {
        // Stereo AAC-LC at 48 kHz: what Slack, browsers and phones all play.
        // The microphone track is mono, which some players reject or play on
        // one side only.
        cmd.args(["-map", "[outa]", "-c:a", "aac", "-b:a", "160k", "-ac", "2", "-ar", "48000"]);
    }
    // H.264 High, 4:2:0, constant frame rate, `avc1` tag, index at the front
    // (`+faststart`): the MP4 flavour Slack and browsers play inline.
    cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"])
        .args(["-profile:v", "high", "-fps_mode", "cfr", "-tag:v", "avc1"])
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
    use crate::project::{Overlay, Range, Slide};
    use std::process::Command;

    fn project() -> Project {
        Project { media: PathBuf::from("in.mkv"), duration: 20.0, width: 1280, height: 720, fps: 30.0, has_audio: true, ..Default::default() }
    }

    #[test]
    fn graph_without_edits_is_single_piece() {
        let g = filter_graph(&project(), &[], &[], None, None);
        assert_eq!(
            g,
            "[0:v:0]null[vov];[vov]split=1[vs0];[0:a]asplit=1[as0];\
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
        p.overlays.push(Overlay { start: 1.0, end: 3.5, text: "hi".into(), x: 0.25, y: 0.75, size: 0.1, color: [255, 0, 16] });
        let g = filter_graph(&p, &[], &[PathBuf::from("/t/o0.txt")], Some(Path::new("/f/a b.ttf")), None);
        assert!(g.starts_with("[0:v:0]drawtext=fontfile='/f/a b.ttf':textfile='/t/o0.txt':"));
        assert!(g.contains("fontsize=72:fontcolor=0xff0010:"));
        assert!(g.contains("x=w*0.25000-text_w/2:y=h*0.75000-text_h/2:enable='between(t,1.000,3.500)'[vov]"));
        assert!(g.contains("[vs0]trim=start=0.0000:end=5.0000"));
        assert!(g.contains("[vs1]trim=start=8.0000:end=20.0000"));
        assert!(g.ends_with("[v0][v1]concat=n=2:v=1:a=0[outv]"));
        assert!(!g.contains("asplit"));
    }

    #[test]
    fn graph_with_slides_and_camera() {
        let mut p = project();
        let slide = |start, end| Slide { start, end, file: "x".into(), page: 1, clip_start: 0.0 };
        p.slides = vec![slide(2.0, 4.0), slide(10.0, 12.0)];
        p.camera_size = Some((640, 480));
        p.set_camera(Range::new(13.0, 14.0), CameraMode::Full);
        p.set_camera(Range::new(15.0, 18.0), CameraMode::Pip);
        let g = filter_graph(&p, &[false, true], &[], None, None);
        let fit = "scale=1280:720:force_original_aspect_ratio=decrease,pad=1280:720:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1";
        assert!(g.starts_with(&format!(
            "[1:v]{fit},fps=30[s0];[0:v:0][s0]overlay=eof_action=pass:enable='between(t,2.000,4.000)'[b0];\
             [2:v]{fit},fps=30,setpts=PTS-STARTPTS+10.000/TB[s1];[b0][s1]overlay=eof_action=pass:enable='between(t,10.000,12.000)'[b1];\
             [0:v:1]split=2[cf][cp];[cf]{fit}[camfull];[b1][camfull]overlay=eof_action=pass:enable='between(t,13.000,14.000)'[b2];\
             [cp]scale=320:-2,setsar=1[campip];[b2][campip]overlay=eof_action=pass:x=main_w-overlay_w-26:y=main_h-overlay_h-26:enable='between(t,15.000,18.000)'[b3];\
             [b3]null[vov];"
        )), "{g}");
    }

    /// Renders a real clip with ffmpeg. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn exports_real_clip() {
        let dir = std::env::temp_dir().join(format!("scribecut-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ffmpeg = |args: &[&str]| {
            assert!(crate::media::ffmpeg().args(["-nostdin", "-y", "-v", "error"]).args(args).status().unwrap().success())
        };
        let path = |name: &str| dir.join(name).display().to_string();
        // Screen (test pattern) + camera (solid green, 2nd video stream) + audio.
        ffmpeg(&["-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=20", "-f", "lavfi", "-i",
            "color=c=0x00ff00:size=320x240:rate=30:duration=20", "-f", "lavfi", "-i", "sine=frequency=440:duration=20",
            "-map", "0", "-map", "1", "-map", "2", "-c:v", "libx264", "-c:a", "libopus", &path("in.mkv")]);
        ffmpeg(&["-f", "lavfi", "-i", "color=c=0xff0000:size=800x600", "-frames:v", "1", &path("red.png")]);
        ffmpeg(&["-f", "lavfi", "-i", "color=c=0x0000ff:size=320x240:rate=30:duration=5", "-c:v", "libx264", &path("blue.mp4")]);
        // A PDF slide needs LibreOffice to make one and poppler to rasterise
        // it. Both are optional extras, so this covers them where they are
        // installed and skips that one slide where they are not.
        let have = |tool: &str| crate::media::tool(tool).is_absolute();
        let pdf_slides = have("soffice") && have("pdfinfo") && have("pdftoppm");
        if pdf_slides {
            std::fs::write(dir.join("page.txt"), "Slide").unwrap();
            assert!(Command::new(crate::media::tool("soffice"))
                .arg(format!("-env:UserInstallation=file://{}", path("lo-profile")))
                .args(["--headless", "--convert-to", "pdf", "--outdir", &path(""), &path("page.txt")])
                .stdin(Stdio::null())
                .output()
                .unwrap()
                .status
                .success());
        } else {
            eprintln!("skipping the PDF slide: needs LibreOffice and poppler");
        }

        let mut p = crate::editor::new_project(&dir.join("in.mkv"), vec![]).unwrap();
        assert!((p.duration - 20.0).abs() < 0.1);
        assert_eq!(p.camera_size, Some((320, 240)));
        let slide = |start, end, file: &str| Slide { start, end, file: file.into(), page: 1, clip_start: 0.0 };
        p.slides = [
            Some(slide(2.0, 4.0, "red.png")),
            pdf_slides.then(|| slide(6.0, 8.0, "page.pdf")),
            Some(slide(10.0, 12.0, "blue.mp4")),
        ]
        .into_iter()
        .flatten()
        .collect();
        p.set_camera(Range::new(13.0, 14.0), CameraMode::Full);
        p.set_camera(Range::new(15.0, 18.0), CameraMode::Pip);
        p.add_cut(Range::new(18.5, 19.5));
        p.overlays.push(Overlay { start: 0.5, end: 1.5, text: "Hello: it's 100%".into(), x: 0.5, y: 0.5, size: 0.1, color: [255, 255, 0] });
        let out = dir.join("out.mp4");
        let mut last = 0.0;
        export(&p, &out, |f| last = f).unwrap();
        assert_eq!(last, 1.0);
        let info = crate::media::probe(&out).unwrap();
        assert!((info.duration - 19.0).abs() < 0.15, "duration {}", info.duration);
        assert!(info.has_audio);
        // The flavour Slack and browsers play inline.
        let streams = Command::new(crate::media::tool("ffprobe"))
            .args(["-v", "error", "-show_entries", "stream=codec_name,profile,pix_fmt,channels", "-of", "compact"])
            .arg(&out)
            .output()
            .unwrap()
            .stdout;
        let streams = String::from_utf8_lossy(&streams);
        assert!(streams.contains("codec_name=h264|profile=High|pix_fmt=yuv420p"), "{streams}");
        assert!(streams.contains("codec_name=aac|profile=LC") && streams.contains("channels=2"), "{streams}");

        // Average colour of a frame region (fractions of the frame).
        let color = |file: &Path, t: f64, (x0, y0, x1, y1): (f64, f64, f64, f64)| -> [u8; 3] {
            let raw = crate::media::ffmpeg()
                .args(["-nostdin", "-v", "error", "-ss", &t.to_string(), "-i"])
                .arg(file)
                .args(["-frames:v", "1", "-vf", "scale=64:36", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
                .output()
                .unwrap()
                .stdout;
            let (mut sum, mut n) = ([0u64; 3], 0u64);
            for y in (y0 * 36.0) as usize..(y1 * 36.0) as usize {
                for x in (x0 * 64.0) as usize..(x1 * 64.0) as usize {
                    for c in 0..3 {
                        sum[c] += raw[(y * 64 + x) * 3 + c] as u64;
                    }
                    n += 1;
                }
            }
            sum.map(|v| (v / n) as u8)
        };
        let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) < 40);
        let all = (0.0, 0.0, 1.0, 1.0);
        let middle = (0.4, 0.4, 0.6, 0.6);
        let expect = |t: f64, region, want: [u8; 3], is: bool, what: &str| {
            let got = color(&out, t, region);
            assert_eq!(close(got, want), is, "{what} at {t}s: got {got:?}");
        };
        expect(3.0, middle, [255, 0, 0], true, "red image slide");
        if pdf_slides {
            // A portrait page, letterboxed: white in the middle, black at the sides.
            expect(7.0, (0.45, 0.02, 0.55, 0.1), [255, 255, 255], true, "PDF page");
            expect(7.0, (0.0, 0.0, 0.1, 1.0), [0, 0, 0], true, "PDF letterbox");
        }
        expect(11.0, middle, [0, 0, 255], true, "blue clip slide");
        expect(13.5, middle, [0, 255, 0], true, "camera full frame");
        expect(16.0, (0.82, 0.78, 0.96, 0.95), [0, 255, 0], true, "camera window bottom right");
        expect(16.0, middle, [0, 255, 0], false, "screen around the camera window");
        expect(1.0, all, [255, 0, 0], false, "no slide at the start");
        expect(9.0, middle, [0, 0, 255], false, "clip only during its range");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
