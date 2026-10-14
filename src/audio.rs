//! Voice cleanup for recorded narration.
//!
//! Built from measurements of real scribecut recordings. Two kinds of
//! microphone showed up:
//!
//! * quiet but clean (a Mac's): 39-50 dB signal-to-noise above 250 Hz, noise
//!   mostly rumble below 120 Hz;
//! * quiet and noisy (a ThinkPad's array mic): only ~20 dB signal-to-noise
//!   across the whole voice band, broadband hiss included.
//!
//! The chain has to work for both:
//!
//! * **High-pass** at 85 Hz (twice): rumble carries no speech.
//! * **RNNoise** (`arnndn`, with a model trained on recording noise under
//!   speech, downloaded once): on the noisy mic it took the pauses from -55 to
//!   -78 dBFS while the voice lost 1 dB. `afftdn` and `anlmdn` managed 2 dB.
//! * **Compressor, downward only** (`makeup=1`): evens out loud passages and
//!   can never raise the noise floor.
//! * **One measured gain, then a limiter.** The gain comes from a first pass
//!   that measures the programme loudness (EBU R128 ignores the pauses), so
//!   speech and pauses rise together and the pauses stay as far below the voice
//!   as the denoiser left them. The earlier single-pass `loudnorm` and
//!   `speechnorm` both raised pauses on their own: on a short, mostly silent
//!   noisy recording the pauses ended up *louder* than the voice.
//!
//! On the noisy recording: signal-to-noise 19.5 dB -> 45.6 dB at -17 LUFS (the
//! old chain: -3.9 dB). On a 5-minute talk: 21.5 -> 27 dB (breaths and clicks,
//! not hiss, are what is left there), -25.7 -> -16.6 LUFS.
//!
//! The same chain is used for the export, the editor's playback, the waveform
//! and transcription (cleaner audio also stops whisper "hearing" words in
//! noise). The recording itself is never changed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// How much cleanup to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Preset {
    /// Leave the audio exactly as recorded.
    Off,
    /// Rumble, leveling and loudness. The default.
    Voice,
    /// Also denoise and de-ess, for a noisy room or a hissy microphone.
    Strong,
}

impl Preset {
    /// Reads `SCRIBECUT_AUDIO`; anything unrecognised means the default.
    pub fn from_env() -> Self {
        match std::env::var("SCRIBECUT_AUDIO").unwrap_or_default().trim().to_lowercase().as_str() {
            "off" | "none" | "raw" => Self::Off,
            "strong" | "noisy" => Self::Strong,
            _ => Self::Voice,
        }
    }
}

/// Programme loudness aimed for, in LUFS. -16 is the usual target for speech;
/// override with `SCRIBECUT_LOUDNESS`.
fn target_lufs() -> f64 {
    std::env::var("SCRIBECUT_LOUDNESS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| (-40.0..=-5.0).contains(v))
        .unwrap_or(-16.0)
}

/// Highest sample peak the limiter allows, as a linear factor (-3.7 dBFS).
///
/// Below the -1 dBTP that EBU R128 asks for, because inter-sample peaks run
/// about 1 dB over the sample peak and the AAC encode in the export adds up to
/// another 1.3 dB.
const PEAK_LIMIT: f64 = 0.65;

/// The measured gain is clamped to this range (dB), so a silent or broken
/// track cannot ask for something absurd.
const GAIN_RANGE: std::ops::RangeInclusive<f64> = -12.0..=30.0;

/// RNNoise model "beguiling-drafter" from github.com/GregorR/rnnoise-models,
/// trained on recording noise under voice. Those models are data, not
/// copyrighted work, per that repository.
const MODEL_URL: &str =
    "https://raw.githubusercontent.com/GregorR/rnnoise-models/master/beguiling-drafter-2018-08-30/bd.rnnn";
const MODEL_FILE: &str = "rnnoise-bd.rnnn";

/// Filter names this ffmpeg build offers.
///
/// `speechnorm` and `deesser` only arrived in ffmpeg 4.3, and `afftdn` in 4.1.
/// Every current distribution is well past that, but an older build should lose
/// a stage rather than fail the whole export with "No such filter", so the
/// optional stages are checked once and dropped if missing.
fn available_filters() -> &'static std::collections::HashSet<String> {
    static FILTERS: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(|| {
            let Ok(out) = crate::media::ffmpeg().args(["-hide_banner", "-filters"]).output() else {
                return std::collections::HashSet::new();
            };
            // Each line is `  TSC name  in->out  description`.
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_whitespace().nth(1))
                .map(str::to_owned)
                .collect()
        });
    &FILTERS
}

/// Whether a stage can be used. An empty probe (no ffmpeg yet) assumes yes, so
/// the chain stays complete and ffmpeg reports the real problem.
fn have(filter: &str) -> bool {
    let filters = available_filters();
    filters.is_empty() || filters.contains(filter)
}

/// The RNNoise model, downloaded on first use. `None` if it can't be had
/// (offline, or no `arnndn` in this ffmpeg): the chain then skips denoising.
pub fn denoise_model() -> Option<PathBuf> {
    static MODEL: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
        if !have("arnndn") {
            return None;
        }
        let path = dirs::data_dir()?.join("scribecut/models").join(MODEL_FILE);
        if path.is_file() {
            return Some(path);
        }
        let download = || -> anyhow::Result<()> {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let mut resp = ureq::get(MODEL_URL).call()?;
            let data = resp.body_mut().read_to_vec()?;
            anyhow::ensure!(data.starts_with(b"rnnoise-nu model"), "not an RNNoise model");
            let part = path.with_extension("part");
            std::fs::write(&part, data)?;
            std::fs::rename(&part, &path)?;
            Ok(())
        };
        match download() {
            Ok(()) => Some(path),
            Err(e) => {
                eprintln!("scribecut: no noise reduction, could not get the RNNoise model: {e:#}");
                None
            }
        }
    });
    MODEL.clone()
}

/// Everything before the gain: shaping and denoising, no level changes upward.
fn stages_before_gain(preset: Preset, model: Option<&Path>) -> Vec<String> {
    let mut stages: Vec<String> = Vec::new();
    // Two second-order sections: one is too gentle to clear the rumble that
    // sits just below the voice.
    stages.push("highpass=f=85:poles=2".into());
    stages.push("highpass=f=85:poles=2".into());
    if let Some(model) = model {
        // Quoted: on macOS the model lives under "Application Support".
        stages.push(format!("arnndn=m='{}'", model.display().to_string().replace('\'', r"'\''")));
    }
    if preset == Preset::Strong && have("afftdn") {
        // Whatever stationary hiss RNNoise left; gentle, or speech goes underwater.
        stages.push("afftdn=nr=12:nf=-50:tn=1".into());
    }
    // Unity makeup: downward compression only, so the noise floor cannot rise.
    stages.push("acompressor=threshold=-18dB:ratio=3:attack=10:release=250:makeup=1:knee=6".into());
    if preset == Preset::Strong && have("deesser") {
        stages.push("deesser=i=0.2:m=0.5:f=0.5".into());
    }
    stages
}

/// The full chain for a preset, denoise model and measured gain (dB).
/// `gain_db: None` (loudness unknown) leaves the level alone but still limits.
pub fn chain(preset: Preset, model: Option<&Path>, gain_db: Option<f64>) -> Option<String> {
    if preset == Preset::Off {
        return None;
    }
    let mut stages = stages_before_gain(preset, model);
    if let Some(gain) = gain_db {
        stages.push(format!("volume={:.2}dB", gain.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end())));
    }
    stages.push(format!("alimiter=limit={PEAK_LIMIT}:level=false"));
    Some(stages.join(","))
}

/// The cleanup chain for `input`'s audio, or `None` to leave it alone.
///
/// Measures the loudness after denoising in a first, audio-only pass (a few
/// seconds per minute of recording) and is remembered per file, so the export,
/// playback, waveform and transcript all get the same gain. Suitable both as
/// `-af` and as a link in a `-filter_complex` graph.
pub fn cleanup(input: &Path) -> Option<String> {
    let preset = Preset::from_env();
    if preset == Preset::Off {
        return None;
    }
    type Key = (PathBuf, Option<SystemTime>, Preset, u64);
    static CACHE: LazyLock<Mutex<HashMap<Key, Option<String>>>> = LazyLock::new(Default::default);
    let target = target_lufs();
    let key = (
        input.to_path_buf(),
        std::fs::metadata(input).and_then(|m| m.modified()).ok(),
        preset,
        target.to_bits(),
    );
    if let Some(hit) = CACHE.lock().unwrap().get(&key) {
        return hit.clone();
    }
    let model = denoise_model();
    let measured = measure_loudness(input, &stages_before_gain(preset, model.as_deref()).join(","));
    let result = chain(preset, model.as_deref(), measured.map(|lufs| target - lufs));
    CACHE.lock().unwrap().insert(key, result.clone());
    result
}

/// The cleaned-up narration of `media` as a FLAC file for the editor's
/// playback, rendered once into `.scribecut-cache/` next to it. `None` when
/// cleanup is off. Takes a few seconds per minute of recording.
pub fn render_clean(media: &Path) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    let Some(chain) = cleanup(media) else { return Ok(None) };
    let dir = media.parent().unwrap_or(Path::new(".")).join(".scribecut-cache");
    std::fs::create_dir_all(&dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&chain, &mut h);
    std::hash::Hash::hash(&std::fs::metadata(media).and_then(|m| m.modified()).ok(), &mut h);
    let stem = media.file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
    let out = dir.join(format!("{stem}-clean-{:016x}.flac", std::hash::Hasher::finish(&h)));
    if out.is_file() {
        return Ok(Some(out));
    }
    let part = out.with_extension("part.flac");
    let status = crate::media::ffmpeg()
        .stdin(std::process::Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-i"])
        .arg(media)
        .args(["-map", "0:a:0", "-af", &chain, "-c:a", "flac"])
        .arg(&part)
        .status()
        .context("running ffmpeg")?;
    anyhow::ensure!(status.success(), "ffmpeg could not clean up the audio");
    std::fs::rename(&part, &out)?;
    Ok(Some(out))
}

/// Integrated loudness (LUFS) of the first audio track after `filters`.
fn measure_loudness(input: &Path, filters: &str) -> Option<f64> {
    let out = crate::media::ffmpeg()
        .stdin(std::process::Stdio::null())
        .args(["-nostdin", "-hide_banner", "-nostats", "-i"])
        .arg(input)
        .args(["-map", "0:a:0", "-af", &format!("{filters},ebur128=framelog=quiet"), "-f", "null", "-"])
        .output()
        .ok()?;
    parse_integrated(&String::from_utf8_lossy(&out.stderr))
}

/// The "I:" value from ebur128's summary.
fn parse_integrated(log: &str) -> Option<f64> {
    let summary = &log[log.rfind("Integrated loudness")?..];
    summary.lines().find_map(|l| l.trim().strip_prefix("I:")).and_then(|v| v.split_whitespace().next()?.parse().ok())
        .filter(|v: &f64| v.is_finite() && *v > -70.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_means_untouched() {
        assert!(chain(Preset::Off, None, Some(10.0)).is_none());
    }

    #[test]
    fn voice_chain_order() {
        let c = chain(Preset::Voice, Some(Path::new("/m/a b.rnnn")), Some(17.0)).unwrap();
        assert_eq!(
            c,
            "highpass=f=85:poles=2,highpass=f=85:poles=2,arnndn=m='/m/a b.rnnn',\
             acompressor=threshold=-18dB:ratio=3:attack=10:release=250:makeup=1:knee=6,\
             volume=17.00dB,alimiter=limit=0.65:level=false"
        );
        // Nothing that lifts quiet passages on its own: that raised the pauses.
        for stage in ["speechnorm", "loudnorm", "dynaudnorm"] {
            assert!(!c.contains(stage), "{c}");
        }
    }

    #[test]
    fn strong_chain_adds_denoise_and_deesser() {
        let c = chain(Preset::Strong, Some(Path::new("/m.rnnn")), Some(0.0)).unwrap();
        let pos = |s: &str| c.find(s).unwrap_or_else(|| panic!("{s} missing: {c}"));
        assert!(pos("highpass") < pos("arnndn") && pos("arnndn") < pos("afftdn") && pos("afftdn") < pos("acompressor"));
        assert!(pos("deesser") < pos("volume"));
    }

    #[test]
    fn without_model_or_measurement() {
        let c = chain(Preset::Voice, None, None).unwrap();
        assert!(!c.contains("arnndn") && !c.contains("volume="), "{c}");
        assert!(c.ends_with("alimiter=limit=0.65:level=false"));
        // An absurd measurement is clamped.
        assert!(chain(Preset::Voice, None, Some(80.0)).unwrap().contains("volume=30.00dB"));
    }

    #[test]
    fn reads_ebur128_summary() {
        let log = "[Parsed_ebur128_0 @ 0x1] Summary:\n\n  Integrated loudness:\n    I:         -32.7 LUFS\n    Threshold: -43.1 LUFS\n";
        assert_eq!(parse_integrated(log), Some(-32.7));
        assert_eq!(parse_integrated("  Integrated loudness:\n    I:         -70.0 LUFS\n"), None, "silence");
        assert_eq!(parse_integrated("no summary"), None);
    }

    #[test]
    fn loudness_target_from_env() {
        for bad in ["", "loud", "0", "-100", "12"] {
            // SAFETY: single-threaded test, and the value is read straight back.
            unsafe { std::env::set_var("SCRIBECUT_LOUDNESS", bad) };
            assert_eq!(target_lufs(), -16.0, "{bad:?} should fall back to the default");
        }
        unsafe { std::env::set_var("SCRIBECUT_LOUDNESS", "-20") };
        assert_eq!(target_lufs(), -20.0);
        unsafe { std::env::remove_var("SCRIBECUT_LOUDNESS") };
    }

    #[test]
    fn preset_names() {
        for (value, want) in [
            ("off", Preset::Off),
            ("RAW", Preset::Off),
            ("strong", Preset::Strong),
            ("noisy", Preset::Strong),
            ("voice", Preset::Voice),
            ("nonsense", Preset::Voice),
            ("", Preset::Voice),
        ] {
            // SAFETY: single-threaded test, and the value is read straight back.
            unsafe { std::env::set_var("SCRIBECUT_AUDIO", value) };
            assert_eq!(Preset::from_env(), want, "{value:?}");
        }
        unsafe { std::env::remove_var("SCRIBECUT_AUDIO") };
    }

    /// Real speech (whisper.cpp's JFK sample) made into a bad recording: 20 dB
    /// too quiet, over rumble and hiss, with 4 s of pause at the end. The
    /// cleaned file must be at the target loudness, not clip, and keep the
    /// pause far below the voice. Downloads the sample and the RNNoise model.
    #[test]
    #[ignore]
    fn cleans_up_a_noisy_recording() {
        let dir = std::env::temp_dir().join(format!("scribecut-audio-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let speech = dir.join("jfk.wav");
        let mut resp = ureq::get("https://github.com/ggerganov/whisper.cpp/raw/master/samples/jfk.wav").call().unwrap();
        std::fs::write(&speech, resp.body_mut().read_to_vec().unwrap()).unwrap();

        let raw = dir.join("raw.wav");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-i"])
            .arg(&speech)
            .args(["-filter_complex",
                "[0:a]volume=-20dB,apad=pad_dur=4[v];\
                 sine=frequency=50:duration=15,volume=-40dB[r];\
                 anoisesrc=color=pink:amplitude=0.003:duration=15[n];\
                 [v][r][n]amix=inputs=3:duration=shortest:normalize=0"])
            .args(["-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&raw)
            .status()
            .unwrap()
            .success();
        assert!(ok, "could not make the test recording");

        let chain = cleanup(&raw).expect("cleanup chain");
        assert!(chain.contains("arnndn"), "no denoiser: {chain}");
        let clean = dir.join("clean.wav");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-i"])
            .arg(&raw)
            .args(["-af", &chain, "-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&clean)
            .status()
            .unwrap()
            .success();
        assert!(ok, "the cleanup chain did not run");

        let (before, after) = (measure(&raw), measure(&clean));
        assert!(before.lufs < -30.0, "test signal should start quiet: {before:?}");
        assert!((-18.5..=-14.0).contains(&after.lufs), "loudness off target: {after:?}");
        assert!(after.true_peak <= -1.5, "true peak too hot: {after:?}");
        let (snr_before, snr_after) = (before.speech - before.pause, after.speech - after.pause);
        assert!(snr_after > snr_before + 8.0, "pauses not cleaned: {snr_before:.1} -> {snr_after:.1} dB ({before:?} {after:?})");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[derive(Debug)]
    struct Measured {
        lufs: f64,
        true_peak: f64,
        /// RMS dBFS of the speech (0.5-10 s) and of the pause at the end.
        speech: f64,
        pause: f64,
    }

    fn measure(path: &Path) -> Measured {
        let run = |args: &[&str], filter: &str| -> String {
            let out = crate::media::ffmpeg()
                .args(["-hide_banner", "-nostats"])
                .args(args)
                .arg("-i")
                .arg(path)
                .args(["-af", filter, "-f", "null", "-"])
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stderr).into_owned()
        };
        // ffmpeg prefixes some lines with `[Parsed_astats_0 @ 0x..]`, so look
        // for the label anywhere in the line rather than at its start.
        let grab = |text: &str, label: &str| -> f64 {
            text.lines()
                .rev()
                .find_map(|l| l.find(label).map(|i| &l[i + label.len()..]))
                .and_then(|v| v.split_whitespace().next())
                .and_then(|v| v.parse().ok())
                .unwrap_or(f64::NAN)
        };
        let loudness = run(&[], "ebur128=peak=true:framelog=quiet");
        let rms = |from: &str, len: &str| {
            grab(&run(&["-ss", from, "-t", len], "astats=measure_overall=RMS_level:measure_perchannel=none"), "RMS level dB:")
        };
        Measured { lufs: grab(&loudness, "I:"), true_peak: grab(&loudness, "Peak:"), speech: rms("0.5", "9.5"), pause: rms("11.5", "3") }
    }
}
