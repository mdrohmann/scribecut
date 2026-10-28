//! Re-recording the narration for part of a recording: the microphone take,
//! and splicing takes into the recording's sound for playback and export.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;

use crate::project::Project;

/// Sample rate takes are recorded and spliced at.
const RATE: u32 = 48_000;

/// Folder, next to the recording, that takes are saved in.
pub const DIR: &str = "voiceovers";

/// The microphone, recorded to an uncompressed file until stopped.
pub struct Microphone {
    pipeline: gst::Pipeline,
    path: PathBuf,
}

impl Microphone {
    pub fn start(path: &Path) -> Result<Self> {
        let source = if cfg!(target_os = "macos") { "osxaudiosrc" } else { "pulsesrc do-timestamp=true" };
        let pipeline = gst::parse::launch(&format!(
            "{source} ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate={RATE},channels=1 \
             ! wavenc ! filesink location=\"{}\"",
            path.display()
        ))
        .context("building the microphone pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        pipeline.set_state(gst::State::Playing).context("starting the microphone")?;
        Ok(Self { pipeline, path: path.to_path_buf() })
    }

    /// Seconds recorded so far.
    pub fn position(&self) -> f64 {
        self.pipeline.query_position::<gst::ClockTime>().map_or(0.0, |t| t.nseconds() as f64 / 1e9)
    }

    /// Any error the pipeline has reported.
    pub fn error(&self) -> Option<String> {
        let msg = self.pipeline.bus()?.pop_filtered(&[gst::MessageType::Error])?;
        match msg.view() {
            gst::MessageView::Error(e) => Some(format!("microphone: {}", e.error())),
            _ => None,
        }
    }

    /// Finishes the file. `wavenc` has to go back and write the data length,
    /// so this waits for the end of the stream to reach it.
    pub fn stop(self) -> PathBuf {
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            bus.timed_pop_filtered(gst::ClockTime::from_seconds(5), &[gst::MessageType::Eos, gst::MessageType::Error]);
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        self.path.clone()
    }
}

impl Drop for Microphone {
    fn drop(&mut self) {
        // A pipeline dropped while still playing crashes GStreamer.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// A file name in `voiceovers/` that is not taken yet: `take-1.flac`, `take-2.flac`, …
pub fn new_take_path(p: &Project) -> Result<PathBuf> {
    let dir = p.base_dir().join(DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok((1..).map(|n| dir.join(format!("take-{n}.flac"))).find(|f| !f.exists()).unwrap())
}

/// Cuts `length` seconds starting `offset` seconds into the raw microphone
/// file into the take, as FLAC.
pub fn save_take(raw: &Path, offset: f64, length: f64, out: &Path) -> Result<()> {
    let output = crate::media::ffmpeg()
        .stdin(Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-ss", &format!("{:.3}", offset.max(0.0)), "-i"])
        .arg(raw)
        .args(["-t", &format!("{length:.3}"), "-ac", "1", "-ar", &RATE.to_string(), "-c:a", "flac"])
        .arg(out)
        .output()
        .context("running ffmpeg")?;
    if !output.status.success() {
        bail!("saving the take failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

/// A `-filter_complex` fragment that takes the recording's sound from `audio`
/// (e.g. `[0:a]`), silences it in every re-recorded range and lays each take
/// over the pieces it was recorded over, ending in `[out]`. The result is on
/// the source timeline and does not depend on the cuts: trimming it to the
/// kept ranges cuts narration and picture alike.
/// Take `i` is ffmpeg input `first_input + i`. `None` without takes.
pub fn splice(p: &Project, audio: &str, first_input: usize, out: &str) -> Option<String> {
    if p.voiceovers.is_empty() {
        return None;
    }
    let mut g = String::new();
    let quiet: Vec<String> = p.voiceovers.iter().map(|v| format!("between(t,{:.4},{:.4})", v.start, v.end)).collect();
    // The takes are placed by sample count from the start, so the recording's
    // sound must start at exactly 0 (an Opus track can start a few ms early).
    // `enable` acts on whole frames, so they are made 2 ms long first: the
    // recording's sound goes quiet where the take starts, not up to a frame
    // later.
    let _ = write!(
        g,
        "{audio}aresample={RATE}:first_pts=0,asetnsamples=n={}:p=0,volume=0:enable='{}'[vo_bed];",
        RATE / 500,
        quiet.join("+")
    );
    let mut layers = vec!["[vo_bed]".to_string()];
    for (i, v) in p.voiceovers.iter().enumerate() {
        let pieces = Project::take_pieces(v);
        if pieces.is_empty() {
            continue;
        }
        let names: Vec<String> = (0..pieces.len()).map(|k| format!("[vo{i}_{k}]")).collect();
        let _ = write!(g, "[{}:a]aresample={RATE},asplit={}{};", first_input + i, pieces.len(), names.concat());
        for (k, (piece, offset)) in pieces.iter().enumerate() {
            let delay = (piece.start * RATE as f64).round() as u64;
            let _ = write!(
                g,
                "{}atrim=start={offset:.4}:end={:.4},asetpts=PTS-STARTPTS,adelay={delay}S:all=1[vo{i}_{k}d];",
                names[k],
                offset + piece.len(),
            );
            layers.push(format!("[vo{i}_{k}d]"));
        }
    }
    let _ = write!(g, "{}amix=inputs={}:normalize=0:duration=first{out}", layers.concat(), layers.len());
    Some(g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Range, Voiceover};

    /// A take of 4..9 recorded with the cut at 5..7.
    fn take(p: &Project) -> Voiceover {
        let pieces = p.kept_pieces(Range::new(4.0, 9.0)).into_iter().map(|(r, _)| r).collect();
        Voiceover { start: 4.0, end: 9.0, file: "t.flac".into(), pieces, stale_length: None, words: vec![] }
    }

    #[test]
    fn splice_lays_pieces_around_cuts() {
        let mut p = Project { duration: 20.0, ..Default::default() };
        assert_eq!(splice(&p, "[0:a]", 1, "[a]"), None);
        p.add_cut(Range::new(5.0, 7.0));
        p.voiceovers.push(take(&p));
        // Cutting more later doesn't move the take.
        p.add_cut(Range::new(7.5, 8.0));
        let g = splice(&p, "[0:a]", 2, "[a]").unwrap();
        assert_eq!(
            g,
            "[0:a]aresample=48000:first_pts=0,asetnsamples=n=96:p=0,volume=0:enable='between(t,4.0000,9.0000)'[vo_bed];\
             [2:a]aresample=48000,asplit=2[vo0_0][vo0_1];\
             [vo0_0]atrim=start=0.0000:end=1.0000,asetpts=PTS-STARTPTS,adelay=192000S:all=1[vo0_0d];\
             [vo0_1]atrim=start=1.0000:end=3.0000,asetpts=PTS-STARTPTS,adelay=336000S:all=1[vo0_1d];\
             [vo_bed][vo0_0d][vo0_1d]amix=inputs=3:normalize=0:duration=first[a]"
        );
    }

    /// Splices a loud take over a quiet recording with ffmpeg and checks the
    /// levels: take where the range is kept, nothing in the cut part, the
    /// recording's own sound around it. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn splices_a_real_take() {
        let dir = std::env::temp_dir().join(format!("scribecut-splice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ffmpeg = |args: &[&str]| {
            let ok = crate::media::ffmpeg().args(["-nostdin", "-y", "-v", "error"]).args(args).status().unwrap().success();
            assert!(ok, "ffmpeg {args:?}");
        };
        let path = |name: &str| dir.join(name).display().to_string();
        ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=440:duration=20", "-ar", "48000", &path("rec.wav")]);
        ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=1000:duration=3,volume=6", "-ar", "48000", &path("take.flac")]);
        let mut p = Project { duration: 20.0, ..Default::default() };
        p.add_cut(Range::new(5.0, 7.0));
        let mut v = take(&p);
        v.file = "take.flac".into();
        p.voiceovers.push(v);
        let graph = splice(&p, "[0:a]", 1, "[out]").unwrap();
        ffmpeg(&["-i", &path("rec.wav"), "-i", &path("take.flac"), "-filter_complex", &graph, "-map", "[out]", &path("out.wav")]);

        let rms = |from: f64| -> f64 {
            let out = crate::media::ffmpeg()
                .args(["-hide_banner", "-nostats", "-ss", &from.to_string(), "-t", "0.5", "-i", &path("out.wav")])
                .args(["-af", "astats=measure_overall=RMS_level:measure_perchannel=none", "-f", "null", "-"])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&out.stderr);
            log.lines()
                .rev()
                .find_map(|l| l.split("RMS level dB:").nth(1))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(f64::NEG_INFINITY)
        };
        let (own, take, cut) = (rms(1.0), rms(4.2), rms(5.7));
        assert!((-24.0..-18.0).contains(&own), "recording's own sound: {own}");
        assert!((-9.0..-2.0).contains(&take), "take, first piece: {take}");
        assert!(cut < -60.0, "cut part is silent: {cut}");
        assert!((-9.0..-2.0).contains(&rms(8.2)), "take, second piece");
        assert!((-24.0..-18.0).contains(&rms(10.0)), "recording again after the range");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records the real microphone for a moment. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn records_the_microphone() {
        gst::init().unwrap();
        let dir = std::env::temp_dir().join(format!("scribecut-mic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mic = Microphone::start(&dir.join("mic.wav")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let at = mic.position();
        assert!(mic.error().is_none());
        let wav = mic.stop();
        let recorded = crate::media::audio_duration(&wav).unwrap();
        assert!((1.2..1.8).contains(&at), "position {at}");
        assert!((recorded - at).abs() < 0.25, "recorded {recorded}, position {at}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
