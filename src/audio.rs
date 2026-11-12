//! Voice cleanup for recorded narration.
//!
//! Built from measurements of real scribecut recordings. Two kinds of
//! microphone showed up:
//!
//! * quiet but clean (a Mac's): 39-50 dB signal-to-noise above 250 Hz, noise
//!   mostly rumble below 120 Hz;
//! * quiet and noisy (a ThinkPad's array mic): only ~20 dB signal-to-noise
//!   across the whole voice band, broadband hiss included, and at arm's length
//!   so much of the room in it that speech sounds like it was recorded in a hall.
//!
//! The chain, in this order:
//!
//! * **High-pass** at 85 Hz (twice): rumble carries no speech.
//! * **Denoise and dereverb.** DeepFilterNet 3 (the `deep-filter` tool), when
//!   it is installed: it takes out the room's reverb as well as the noise. It
//!   is not an ffmpeg filter, so it runs once per file and its result is kept
//!   in `.scribecut-cache/` ([`voice`]). Without it, RNNoise (`arnndn`, with a
//!   model trained on recording noise under speech, downloaded once), which
//!   only takes out noise.
//! * **EQ**: a little less boxiness (300 Hz), a little more presence (4 kHz).
//! * **The measured gain**, from a first pass over the programme (EBU R128
//!   ignores the pauses), so the stages after it always see speech at the
//!   target loudness, whatever the microphone's level.
//! * **Downward expander** (`agate`, 2:1 below -35 dB, at most 18 dB): quiets
//!   the reverb tails and breaths between words. Gentle, so word endings stay.
//! * **Compressor**, 2:1 above -14 dB, downward only: evens out loud passages.
//!   Any more and it lifts the tails between words back up.
//! * **De-esser.**
//! * **A trim** from a second pass, since the dynamics took some loudness, then
//!   a limiter.
//!
//! Measured on 45 s of the arm's-length laptop mic, as the sound 80-300 ms
//! after a word ends relative to the word: -25 dB as recorded, -33 dB through
//! the old chain (no expander, and its compressor sat before the gain, so on a
//! quiet microphone it never engaged), -43 dB through this one with RNNoise,
//! -49 dB with DeepFilterNet. WPE dereverberation (`nara_wpe`) on the
//! one-channel recording added 2 dB on top of RNNoise and nothing on top of
//! DeepFilterNet, so it is not used.
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
    /// Rumble, denoise (and dereverb, with DeepFilterNet), EQ, dynamics,
    /// de-essing and loudness. The default.
    Voice,
    /// Also an extra stationary-hiss pass, for a very noisy microphone.
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

/// The trim after the dynamics only makes up what they took away.
const TRIM_RANGE: std::ops::RangeInclusive<f64> = -6.0..=12.0;

/// Less of the room's boxiness, a little more presence.
const EQ: &str = "equalizer=f=300:t=q:w=1:g=-3,equalizer=f=4000:t=q:w=1:g=1.5";

/// 2:1 below -35 dB (speech is at about -16 LUFS here), at most 18 dB down
/// (`range`). A longer release lets the tails through again; a higher
/// threshold starts to eat quiet syllables.
const EXPANDER: &str = "agate=threshold=-35dB:ratio=2:range=0.125:attack=5:release=150:knee=4";

/// Unity makeup: downward compression only.
const COMPRESSOR: &str = "acompressor=threshold=-14dB:ratio=2:attack=10:release=250:makeup=1:knee=6";

const DEESSER: &str = "deesser=i=0.2:m=0.5:f=0.5";

/// Two second-order sections: one is too gentle to clear the rumble that
/// sits just below the voice.
const HIGHPASS: &str = "highpass=f=85:poles=2,highpass=f=85:poles=2";

/// RNNoise model "beguiling-drafter" from github.com/GregorR/rnnoise-models,
/// trained on recording noise under voice. Those models are data, not
/// copyrighted work, per that repository.
const MODEL_URL: &str =
    "https://raw.githubusercontent.com/GregorR/rnnoise-models/master/beguiling-drafter-2018-08-30/bd.rnnn";
const MODEL_FILE: &str = "rnnoise-bd.rnnn";

/// Filter names this ffmpeg build offers.
///
/// `deesser` only arrived in ffmpeg 4.3, `agate` in 4.0 and `afftdn` in 4.1.
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

/// The `deep-filter` tool (DeepFilterNet 3), if installed: `SCRIBECUT_DEEP_FILTER`
/// names it (`off` to use RNNoise instead), else it is looked for on `PATH`
/// and in `~/.cargo/bin`, where `cargo install` puts it.
fn deep_filter() -> Option<PathBuf> {
    static TOOL: LazyLock<Option<PathBuf>> = LazyLock::new(|| {
        match std::env::var_os("SCRIBECUT_DEEP_FILTER") {
            Some(v) if v == "off" => return None,
            Some(v) => return Some(PathBuf::from(v)).filter(|p| p.is_file()),
            None => {}
        }
        let found = crate::media::tool("deep-filter");
        if found.is_absolute() {
            return Some(found);
        }
        Some(dirs::home_dir()?.join(".cargo/bin/deep-filter")).filter(|p| p.is_file())
    });
    TOOL.clone()
}

/// Where the denoising happens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Denoise<'a> {
    /// Already done: the sound comes from [`voice`] (high-passed, then
    /// DeepFilterNet).
    Done,
    /// RNNoise in the chain, with this model.
    Rnnoise(&'a Path),
    /// Not at all.
    Off,
}

/// Everything before the measured gain: shaping and denoising, no level changes upward.
fn stages_before_gain(preset: Preset, denoise: Denoise) -> Vec<String> {
    let mut stages: Vec<String> = Vec::new();
    if denoise != Denoise::Done {
        stages.push(HIGHPASS.into());
    }
    if let Denoise::Rnnoise(model) = denoise {
        // Quoted: on macOS the model lives under "Application Support".
        stages.push(format!("arnndn=m='{}'", model.display().to_string().replace('\'', r"'\''")));
    }
    if preset == Preset::Strong && have("afftdn") {
        // Whatever stationary hiss the denoiser left; gentle, or speech goes underwater.
        stages.push("afftdn=nr=12:nf=-50:tn=1".into());
    }
    stages.push(EQ.into());
    stages
}

/// The dynamics, which run on speech already at the target loudness.
fn stages_after_gain() -> Vec<String> {
    let mut stages: Vec<String> = Vec::new();
    if have("agate") {
        stages.push(EXPANDER.into());
    }
    stages.push(COMPRESSOR.into());
    if have("deesser") {
        stages.push(DEESSER.into());
    }
    stages
}

/// The full chain for a preset and denoiser, with the measured `gain` (dB)
/// that brings the programme to the target and the `trim` (dB) that makes up
/// what the dynamics took. Unknown (`None`) leaves the level alone but still
/// limits.
pub fn chain(preset: Preset, denoise: Denoise, gain: Option<f64>, trim: Option<f64>) -> Option<String> {
    if preset == Preset::Off {
        return None;
    }
    let mut stages = stages_before_gain(preset, denoise);
    if let Some(gain) = gain {
        stages.push(format!("volume={:.2}dB", gain.clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end())));
    }
    stages.extend(stages_after_gain());
    if let Some(trim) = trim {
        stages.push(format!("volume={:.2}dB", trim.clamp(*TRIM_RANGE.start(), *TRIM_RANGE.end())));
    }
    stages.push(format!("alimiter=limit={PEAK_LIMIT}:level=false"));
    Some(stages.join(","))
}

/// How to clean up a file's sound.
#[derive(Clone, Debug, PartialEq)]
pub struct Cleanup {
    /// The ffmpeg filter chain, suitable both as `-af` and as a link in a
    /// `-filter_complex` graph.
    pub chain: String,
    /// When DeepFilterNet is used: the file to read the sound from instead
    /// ([`voice`]), on the same timeline. The chain expects that sound, and so
    /// does every other file mixed in: read those through [`sound`].
    pub voice: Option<PathBuf>,
}

/// The cleanup for `input`'s audio, or `None` to leave it alone.
///
/// Measures the loudness in two audio-only passes (a few seconds per minute of
/// recording; DeepFilterNet's own pass the first time takes a little longer)
/// and is remembered per file, so the export, playback, waveform and
/// transcript all get the same gain.
pub fn cleanup(input: &Path) -> Option<Cleanup> {
    let preset = Preset::from_env();
    if preset == Preset::Off {
        return None;
    }
    type Key = (PathBuf, Option<SystemTime>, Preset, u64);
    static CACHE: LazyLock<Mutex<HashMap<Key, Option<Cleanup>>>> = LazyLock::new(Default::default);
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
    let voice = voice(input);
    let model = if voice.is_some() { None } else { denoise_model() };
    let denoise = match (&voice, &model) {
        (Some(_), _) => Denoise::Done,
        (None, Some(model)) => Denoise::Rnnoise(model),
        (None, None) => Denoise::Off,
    };
    let source = voice.as_deref().unwrap_or(input);
    let gain = measure_loudness(source, &stages_before_gain(preset, denoise).join(",")).map(|lufs| target - lufs);
    // The second pass measures everything up to the trim, the limiter left out.
    let trim = gain.and_then(|gain| {
        let full = chain(preset, denoise, Some(gain), None)?;
        let before_limiter = &full[..full.rfind(",alimiter")?];
        measure_loudness(source, before_limiter).map(|lufs| target - lufs)
    });
    let result = chain(preset, denoise, gain, trim).map(|chain| Cleanup { chain, voice });
    CACHE.lock().unwrap().insert(key, result.clone());
    result
}

/// The file to read another file's sound from (a re-recorded take) when it
/// is mixed in before `cleanup`'s chain: DeepFilterNet's [`voice`] of it if the
/// chain expects that, else the file itself.
pub fn sound(cleanup: Option<&Cleanup>, file: &Path) -> PathBuf {
    cleanup.filter(|c| c.voice.is_some()).and_then(|_| voice(file)).unwrap_or_else(|| file.to_path_buf())
}

/// `input`'s first audio track high-passed and run through DeepFilterNet, as
/// mono FLAC at 48 kHz, starting at 0 and exactly as long as the track.
/// Rendered once into `.scribecut-cache/` next to `input` (a few seconds per
/// minute of sound). `None` without `deep-filter`, or if it failed.
pub fn voice(input: &Path) -> Option<PathBuf> {
    let tool = deep_filter()?;
    match render_voice(&tool, input) {
        Ok(out) => Some(out),
        Err(e) => {
            eprintln!("scribecut: DeepFilterNet failed on {}, using RNNoise: {e:#}", input.display());
            None
        }
    }
}

fn render_voice(tool: &Path, input: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    use std::hash::{Hash, Hasher};
    // One render at a time: the transcript and the editor's playback can ask
    // for the same file together.
    static BUSY: Mutex<()> = Mutex::new(());
    let _busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
    let modified = |f: &Path| std::fs::metadata(f).and_then(|m| m.modified()).ok();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    input.hash(&mut h);
    modified(input).hash(&mut h);
    tool.hash(&mut h);
    modified(tool).hash(&mut h);
    let dir = input.parent().unwrap_or(Path::new(".")).join(".scribecut-cache");
    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
    let prefix = format!("{stem}-voice-");
    let out = dir.join(format!("{prefix}{:016x}.flac", h.finish()));
    if out.is_file() {
        return Ok(out);
    }
    let work = dir.join(format!("{prefix}work"));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let ffmpeg = |args: &[&std::ffi::OsStr]| -> anyhow::Result<()> {
        let output = crate::media::ffmpeg()
            .stdin(std::process::Stdio::null())
            .args(["-nostdin", "-y", "-v", "error"])
            .args(args)
            .output()
            .context("running ffmpeg")?;
        anyhow::ensure!(output.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&output.stderr).trim());
        Ok(())
    };
    let os = |s: &'static str| std::ffi::OsStr::new(s);
    // DeepFilterNet takes very quiet speech for noise, so it gets the sound at
    // broadcast level (-23 LUFS); the chain measures again afterwards anyway.
    let gain = measure_loudness(input, &format!("aresample=48000:first_pts=0,{HIGHPASS}"))
        .map_or(0.0, |lufs| (-23.0 - lufs).clamp(*GAIN_RANGE.start(), *GAIN_RANGE.end()));
    let high_passed = work.join("in.wav");
    ffmpeg(&[
        os("-i"),
        input.as_os_str(),
        os("-map"),
        os("0:a:0"),
        os("-af"),
        std::ffi::OsStr::new(&format!("aresample=48000:first_pts=0,{HIGHPASS},volume={gain:.2}dB")),
        os("-ac"),
        os("1"),
        os("-c:a"),
        os("pcm_f32le"),
        high_passed.as_os_str(),
    ])?;
    // -D: remove the model's delay, so the sound stays on the picture's timeline.
    let output = std::process::Command::new(tool)
        .stdin(std::process::Stdio::null())
        .arg("-D")
        .arg("-o")
        .arg(work.join("out"))
        .arg(&high_passed)
        .output()
        .with_context(|| format!("running {}", tool.display()))?;
    anyhow::ensure!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr).trim());
    // Its output ends a few frames early: padded with silence to the input's
    // exact length by mixing it onto a silenced copy of the input.
    let part = out.with_extension("part.flac");
    ffmpeg(&[
        os("-i"),
        high_passed.as_os_str(),
        os("-i"),
        work.join("out/in.wav").as_os_str(),
        os("-filter_complex"),
        os("[0:a]volume=0[z];[1:a]apad[d];[z][d]amix=inputs=2:duration=first:normalize=0[out]"),
        os("-map"),
        os("[out]"),
        os("-c:a"),
        os("flac"),
        part.as_os_str(),
    ])?;
    std::fs::rename(&part, &out)?;
    let _ = std::fs::remove_dir_all(&work);
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".flac") && !name.contains(".part") && entry.path() != out {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(out)
}

/// The narration for the editor's playback as a FLAC file: cleaned up, with
/// any re-recorded takes spliced in, rendered once into `.scribecut-cache/`
/// next to the recording. `None` when there is nothing to change about the
/// recording's own sound. Takes a few seconds per minute of recording.
pub fn render_clean(p: &crate::project::Project) -> anyhow::Result<Option<PathBuf>> {
    use anyhow::Context as _;
    use std::hash::{Hash, Hasher};
    let media = &p.media;
    let cleanup = cleanup(media);
    let chain = cleanup.as_ref().map(|c| &c.chain);
    let splice = crate::voiceover::splice(p, "[0:a:0]", 1, "[spliced]");
    if chain.is_none() && splice.is_none() {
        return Ok(None);
    }
    let dir = media.parent().unwrap_or(Path::new(".")).join(".scribecut-cache");
    std::fs::create_dir_all(&dir)?;
    let modified = |f: &Path| std::fs::metadata(f).and_then(|m| m.modified()).ok();
    let recording = cleanup.as_ref().and_then(|c| c.voice.clone()).unwrap_or_else(|| media.clone());
    let takes: Vec<PathBuf> = p.voiceovers.iter().map(|v| sound(cleanup.as_ref(), &p.resolve(&v.file))).collect();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    chain.hash(&mut h);
    splice.hash(&mut h);
    for file in std::iter::once(&recording).chain(&takes) {
        file.hash(&mut h);
        modified(file).hash(&mut h);
    }
    let stem = media.file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
    let out = dir.join(format!("{stem}-clean-{:016x}.flac", h.finish()));
    if out.is_file() {
        return Ok(Some(out));
    }
    let (source, mut graph) = match &splice {
        Some(splice) => ("[spliced]", format!("{splice};")),
        None => ("[0:a:0]", String::new()),
    };
    graph.push_str(&format!("{source}{}[out]", chain.map_or("anull", |c| c.as_str())));
    let part = out.with_extension("part.flac");
    let mut cmd = crate::media::ffmpeg();
    cmd.stdin(std::process::Stdio::null()).args(["-nostdin", "-y", "-v", "error", "-i"]).arg(&recording);
    for take in &takes {
        cmd.arg("-i").arg(take);
    }
    let output = cmd
        .args(["-filter_complex", &graph, "-map", "[out]", "-c:a", "flac"])
        .arg(&part)
        .output()
        .context("running ffmpeg")?;
    anyhow::ensure!(
        output.status.success(),
        "ffmpeg could not render the audio: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    std::fs::rename(&part, &out)?;
    // Earlier renders for other takes or cuts are not needed again. Playback
    // may still have one open, which is fine: it goes when closed.
    let prefix = format!("{stem}-clean-");
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && name.ends_with(".flac") && !name.contains(".part") && entry.path() != out {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(Some(out))
}

/// `programme` (the sound on the source timeline: [`render_clean`]'s file,
/// or the recording itself) with everything outside `kept` removed, as the
/// export will have it, rendered into `.scribecut-cache/` next to `media`.
/// Takes a couple of seconds: no filtering, only cutting and joining.
pub fn render_cut(media: &Path, programme: &Path, kept: &[crate::project::Range]) -> anyhow::Result<PathBuf> {
    use anyhow::Context as _;
    use std::fmt::Write as _;
    use std::hash::{Hash, Hasher};
    anyhow::ensure!(!kept.is_empty(), "everything is cut");
    let dir = media.parent().unwrap_or(Path::new(".")).join(".scribecut-cache");
    std::fs::create_dir_all(&dir)?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    programme.hash(&mut h);
    std::fs::metadata(programme).and_then(|m| m.modified()).ok().hash(&mut h);
    for r in kept {
        (r.start.to_bits(), r.end.to_bits()).hash(&mut h);
    }
    let stem = media.file_stem().and_then(|s| s.to_str()).unwrap_or("audio");
    let prefix = format!("{stem}-cut-");
    let out = dir.join(format!("{prefix}{:016x}.flac", h.finish()));
    if out.is_file() {
        return Ok(out);
    }
    let n = kept.len();
    let mut graph = format!("[0:a:0]asplit={n}{};", (0..n).map(|i| format!("[s{i}]")).collect::<String>());
    for (i, r) in kept.iter().enumerate() {
        let _ = write!(graph, "[s{i}]atrim=start={:.4}:end={:.4},asetpts=PTS-STARTPTS[p{i}];", r.start, r.end);
    }
    let _ = write!(graph, "{}concat=n={n}:v=0:a=1[out]", (0..n).map(|i| format!("[p{i}]")).collect::<String>());
    let part = out.with_extension("part.flac");
    let output = crate::media::ffmpeg()
        .stdin(std::process::Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-i"])
        .arg(programme)
        .args(["-filter_complex", &graph, "-map", "[out]", "-c:a", "flac"])
        .arg(&part)
        .output()
        .context("running ffmpeg")?;
    anyhow::ensure!(
        output.status.success(),
        "ffmpeg could not cut the audio: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    std::fs::rename(&part, &out)?;
    for entry in std::fs::read_dir(&dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) && !name.contains(".part") && entry.path() != out {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    Ok(out)
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
        assert!(chain(Preset::Off, Denoise::Off, Some(10.0), Some(1.0)).is_none());
    }

    #[test]
    fn voice_chain_order() {
        let c = chain(Preset::Voice, Denoise::Rnnoise(Path::new("/m/a b.rnnn")), Some(17.0), Some(1.5)).unwrap();
        assert_eq!(
            c,
            "highpass=f=85:poles=2,highpass=f=85:poles=2,arnndn=m='/m/a b.rnnn',\
             equalizer=f=300:t=q:w=1:g=-3,equalizer=f=4000:t=q:w=1:g=1.5,\
             volume=17.00dB,\
             agate=threshold=-35dB:ratio=2:range=0.125:attack=5:release=150:knee=4,\
             acompressor=threshold=-14dB:ratio=2:attack=10:release=250:makeup=1:knee=6,\
             deesser=i=0.2:m=0.5:f=0.5,\
             volume=1.50dB,alimiter=limit=0.65:level=false"
        );
        // Nothing that lifts quiet passages on its own: that raised the pauses.
        for stage in ["speechnorm", "loudnorm", "dynaudnorm"] {
            assert!(!c.contains(stage), "{c}");
        }
    }

    /// The expander and compressor have fixed thresholds, so they only behave
    /// the same for every microphone once the speech is at the target level.
    #[test]
    fn dynamics_come_after_the_gain() {
        let c = chain(Preset::Strong, Denoise::Rnnoise(Path::new("/m.rnnn")), Some(9.0), Some(2.0)).unwrap();
        let pos = |s: &str| c.find(s).unwrap_or_else(|| panic!("{s} missing: {c}"));
        assert!(pos("highpass") < pos("arnndn") && pos("arnndn") < pos("afftdn") && pos("afftdn") < pos("equalizer"));
        assert!(pos("equalizer") < pos("volume=9.00dB") && pos("volume=9.00dB") < pos("agate"));
        assert!(pos("agate") < pos("acompressor") && pos("acompressor") < pos("deesser"));
        assert!(pos("deesser") < pos("volume=2.00dB") && pos("volume=2.00dB") < pos("alimiter"));
    }

    #[test]
    fn deep_filter_replaces_highpass_and_rnnoise() {
        let c = chain(Preset::Voice, Denoise::Done, Some(3.0), Some(1.0)).unwrap();
        assert!(c.starts_with("equalizer=f=300"), "{c}");
        assert!(!c.contains("highpass") && !c.contains("arnndn"), "already done before the chain: {c}");
    }

    #[test]
    fn without_model_or_measurement() {
        let c = chain(Preset::Voice, Denoise::Off, None, None).unwrap();
        assert!(!c.contains("arnndn") && !c.contains("volume="), "{c}");
        assert!(c.contains("agate") && c.contains("deesser"), "{c}");
        assert!(c.ends_with("alimiter=limit=0.65:level=false"));
        // Absurd measurements are clamped.
        let c = chain(Preset::Voice, Denoise::Off, Some(80.0), Some(-20.0)).unwrap();
        assert!(c.contains("volume=30.00dB") && c.contains("volume=-6.00dB"), "{c}");
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
        let dir = test_dir("noisy");
        let speech = jfk(&dir);

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

        let clean = clean_up(&raw);

        let (before, after) = (measure(&raw), measure(&clean));
        assert!(before.lufs < -30.0, "test signal should start quiet: {before:?}");
        assert!((-18.5..=-14.0).contains(&after.lufs), "loudness off target: {after:?}");
        assert!(after.true_peak <= -1.5, "true peak too hot: {after:?}");
        let (snr_before, snr_after) = (before.speech - before.pause, after.speech - after.pause);
        assert!(snr_after > snr_before + 8.0, "pauses not cleaned: {snr_before:.1} -> {snr_after:.1} dB ({before:?} {after:?})");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same speech in a hall: up to the end of its last word, through a
    /// synthetic room (0.6 s reverb time), once at a normal level and once 20 dB
    /// quieter, like a microphone further away. Both must come out alike, since
    /// the dynamics run at the target level whatever the input level was, and
    /// with DeepFilterNet the reverb after the last word must come down well.
    #[test]
    #[ignore]
    fn tames_a_reverberant_recording() {
        let dir = test_dir("hall");
        let speech = jfk(&dir);
        let rms = |path: &Path, from: f64, len: f64| rms_db(path, from, len);
        // Relative to the speech: the 0.1-0.5 s after the last word ends (10.2 s).
        let tail = |path: &Path| rms(path, 10.3, 0.4) - rms(path, 0.5, 9.0);
        let mut results = Vec::new();
        for level in [-12, -32] {
            let raw = dir.join(format!("hall{level}.wav"));
            let ok = crate::media::ffmpeg()
                .args(["-y", "-v", "error", "-i"])
                .arg(&speech)
                .args(["-filter_complex", &format!(
                    "[0:a]atrim=end=10.2,aresample=48000,apad=pad_dur=3[v];\
                     aevalsrc='if(eq(n\\,0)\\,1\\,0.12*(random(0)*2-1)*exp(-11.5*t))':d=0.6:s=48000[ir];\
                     [v][ir]afir=irnorm=-1,volume={level}dB")])
                .args(["-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
                .arg(&raw)
                .status()
                .unwrap()
                .success();
            assert!(ok, "could not make the test recording");
            let clean = clean_up(&raw);
            let (before, after) = (tail(&raw), tail(&clean));
            eprintln!("{level} dB: reverb tail {before:.1} -> {after:.1} dB");
            assert!(before > -24.0, "the room should be audible: {before:.1} dB");
            // RNNoise leaves the room alone, and the expander cannot reach the
            // loud early part of the tail: only DeepFilterNet is held to this.
            if deep_filter().is_some() {
                assert!(after < before - 12.0, "reverb tail {before:.1} -> {after:.1} dB at {level} dB");
            }
            results.push((measure(&clean).lufs, after));
        }
        let [(lufs_a, tail_a), (lufs_b, tail_b)] = results[..] else { unreachable!() };
        assert!((-18.5..=-14.0).contains(&lufs_a), "loudness off target: {lufs_a}");
        assert!((lufs_a - lufs_b).abs() < 0.5, "loudness depends on the input level: {lufs_a} vs {lufs_b}");
        assert!((tail_a - tail_b).abs() < 3.0, "tail depends on the input level: {tail_a:.1} vs {tail_b:.1}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// DeepFilterNet's render must keep the sound exactly where it was: same
    /// length, no delay. Needs `deep-filter` installed.
    #[test]
    #[ignore]
    fn deep_filter_keeps_the_timeline() {
        let tool = deep_filter().expect("deep-filter not installed");
        let dir = test_dir("voice");
        let speech = jfk(&dir);
        // An Opus track in Matroska, like a recording.
        let raw = dir.join("recording.mkv");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-i"])
            .arg(&speech)
            .args(["-af", "aresample=48000", "-c:a", "libopus"])
            .arg(&raw)
            .status()
            .unwrap()
            .success();
        assert!(ok, "could not make the test recording");
        let voice = render_voice(&tool, &raw).unwrap();
        assert_eq!(render_voice(&tool, &raw).unwrap(), voice, "rendered once, then kept");
        let (a, b) = (samples(&raw), samples(&voice));
        assert_eq!(a.len(), b.len());
        // Best alignment within ±50 ms must be within a millisecond.
        let score = |lag: isize| -> f64 {
            (48_000..a.len() - 48_000).map(|i| a[i] as f64 * b[(i as isize + lag) as usize] as f64).sum()
        };
        let best = (-2400..=2400).step_by(24).max_by(|&x, &y| score(x).total_cmp(&score(y))).unwrap();
        let best = (best - 24..=best + 24).max_by(|&x, &y| score(x).total_cmp(&score(y))).unwrap();
        assert!(best.abs() <= 48, "shifted by {best} samples");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real recording through the cleanup, for listening: set
    /// `SCRIBECUT_TEST_MEDIA` to the recording and `SCRIBECUT_TEST_OUT` to the
    /// FLAC to write, optionally `SCRIBECUT_TEST_RANGE` ("from-to", seconds)
    /// for an excerpt. The chain is measured on the whole recording, as in the
    /// app. Prints the chain and how long each step took.
    #[test]
    #[ignore]
    fn cleans_a_real_recording() {
        let media = PathBuf::from(std::env::var("SCRIBECUT_TEST_MEDIA").expect("SCRIBECUT_TEST_MEDIA"));
        let out = PathBuf::from(std::env::var("SCRIBECUT_TEST_OUT").expect("SCRIBECUT_TEST_OUT"));
        let t = std::time::Instant::now();
        let cleanup = cleanup(&media).expect("cleanup chain");
        eprintln!("chain ({:.1} s, voice: {:?}): {}", t.elapsed().as_secs_f64(), cleanup.voice, cleanup.chain);
        let range = std::env::var("SCRIBECUT_TEST_RANGE").ok();
        let range: Vec<&str> = range.as_deref().map(|r| r.split('-').collect()).unwrap_or_default();
        let t = std::time::Instant::now();
        let mut cmd = crate::media::ffmpeg();
        cmd.args(["-y", "-v", "error"]);
        if let [from, to] = range[..] {
            cmd.args(["-ss", from, "-to", to]);
        }
        let ok = cmd
            .arg("-i")
            .arg(cleanup.voice.as_deref().unwrap_or(&media))
            .args(["-map", "0:a:0", "-af", &cleanup.chain, "-c:a", "flac"])
            .arg(&out)
            .status()
            .unwrap()
            .success();
        assert!(ok, "the cleanup chain did not run");
        eprintln!("rendered in {:.1} s: {}", t.elapsed().as_secs_f64(), out.display());
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scribecut-audio-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Real speech: whisper.cpp's JFK sample, 11 s, the last word ending at 10.2 s.
    fn jfk(dir: &Path) -> PathBuf {
        let speech = dir.join("jfk.wav");
        let mut resp = ureq::get("https://github.com/ggerganov/whisper.cpp/raw/master/samples/jfk.wav").call().unwrap();
        std::fs::write(&speech, resp.body_mut().read_to_vec().unwrap()).unwrap();
        speech
    }

    /// `raw` through [`cleanup`], as a 48 kHz WAV next to it.
    fn clean_up(raw: &Path) -> PathBuf {
        let cleanup = cleanup(raw).expect("cleanup chain");
        assert!(
            cleanup.voice.is_some() || cleanup.chain.contains("arnndn"),
            "no denoiser: {}",
            cleanup.chain
        );
        let clean = raw.with_extension("clean.wav");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-i"])
            .arg(cleanup.voice.as_deref().unwrap_or(raw))
            .args(["-af", &cleanup.chain, "-ar", "48000", "-ac", "1", "-c:a", "pcm_s16le"])
            .arg(&clean)
            .status()
            .unwrap()
            .success();
        assert!(ok, "the cleanup chain did not run");
        clean
    }

    /// The first audio track as 48 kHz mono samples.
    fn samples(path: &Path) -> Vec<f32> {
        let out = crate::media::ffmpeg()
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
            .output()
            .unwrap();
        out.stdout.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
    }

    /// RMS level in dBFS of `len` seconds from `from`.
    fn rms_db(path: &Path, from: f64, len: f64) -> f64 {
        let x = samples(path);
        let part = &x[(from * 48_000.0) as usize..((from + len) * 48_000.0) as usize];
        10.0 * (part.iter().map(|&v| (v as f64).powi(2)).sum::<f64>() / part.len() as f64 + 1e-20).log10()
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
