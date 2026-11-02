//! Local speech-to-text with word timestamps via whisper.cpp.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperVadContext, WhisperVadContextParams,
    WhisperVadParams,
};

use crate::project::Word;

pub enum Status {
    Downloading(f32),
    Transcribing(f32),
}

/// Model name, e.g. `small.en` (default) or `base.en`; override with `SCRIBECUT_MODEL`.
fn model_name() -> String {
    std::env::var("SCRIBECUT_MODEL").unwrap_or_else(|_| "small.en".into())
}

/// Returns the local model path, downloading it once if needed.
pub fn ensure_model(mut status: impl FnMut(Status)) -> Result<PathBuf> {
    let dir = dirs::data_dir().context("no data dir")?.join("scribecut/models");
    std::fs::create_dir_all(&dir)?;
    let file = format!("ggml-{}.bin", model_name());
    let path = dir.join(&file);
    if path.is_file() {
        return Ok(path);
    }
    let url = format!("https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{file}");
    crate::media::download(&url, &path, |f| status(Status::Downloading(f)))?;
    Ok(path)
}

/// Silero voice-activity model for whisper.cpp (under 1 MB), downloaded once.
/// It finds the stretches with speech, which whisper then transcribes one by
/// one (see `transcribe`).
fn vad_model() -> Option<PathBuf> {
    const FILE: &str = "ggml-silero-v5.1.2.bin";
    let path = dirs::data_dir()?.join("scribecut/models").join(FILE);
    if path.is_file() {
        return Some(path);
    }
    let download = || -> Result<()> {
        let url = format!("https://huggingface.co/ggml-org/whisper-vad/resolve/main/{FILE}");
        let mut resp = ureq::get(&url).call().with_context(|| format!("downloading {url}"))?;
        let data = resp.body_mut().with_config().limit(32 << 20).read_to_vec()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let part = path.with_extension("part");
        std::fs::write(&part, data)?;
        std::fs::rename(&part, &path)?;
        Ok(())
    };
    match download() {
        Ok(()) => Some(path),
        Err(e) => {
            eprintln!("scribecut: transcribing without voice detection: {e:#}");
            None
        }
    }
}

/// Transcribes the audio track of `media` into individual timed words.
pub fn transcribe(media: &Path, mut status: impl FnMut(Status) + Send + 'static) -> Result<Vec<Word>> {
    let model = ensure_model(&mut status)?;
    let audio = crate::media::extract_audio_16k(media)?;
    if audio.is_empty() {
        bail!("no audio in recording");
    }

    let ctx = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
        .context("loading whisper model")?;
    let mut state = ctx.create_state().context("creating whisper state")?;

    // Whisper places the first words after a long pause seconds too early, so
    // it gets the speech with long pauses shortened, in one pass (running it
    // per stretch costs a full 30 s window each), and the times are mapped
    // back. Without the voice detector: the whole track as it is.
    let chunks = speech_chunks(&audio).unwrap_or_else(|| vec![(0, audio.len())]);
    if chunks.is_empty() {
        return Ok(Vec::new());
    }
    let (compact, map) = compact(&audio, &chunks);
    let mut params = whisper_params();
    params.set_progress_callback_safe(move |p: i32| status(Status::Transcribing(p as f32 / 100.0)));
    state.full(params, &compact).context("running whisper")?;
    let mut words = Vec::new();
    collect_words(&state, 0.0, &mut words);
    for w in &mut words {
        w.start = map.to_source(w.start);
        w.end = map.to_source(w.end).max(w.start);
    }
    fit_to_speech(&mut words, &pauses(&audio));
    Ok(words)
}

/// Audio level below which a 50 ms frame counts as quiet, in dB below full scale.
const QUIET_DB: f32 = -42.0;
/// Shorter quiet stretches are gaps inside a word, not pauses.
const MIN_PAUSE: f64 = 0.3;
const FRAME: usize = SAMPLE_RATE / 20;

/// The quiet stretches of the audio, as (start, end) in seconds.
fn pauses(audio: &[f32]) -> Vec<(f64, f64)> {
    let quiet = audio.chunks(FRAME).map(|c| {
        let rms = (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt();
        20.0 * (rms + 1e-9).log10() < QUIET_DB
    });
    let secs = |k: usize| (k * FRAME) as f64 / SAMPLE_RATE as f64;
    let mut out = Vec::new();
    let mut start = None;
    for (k, q) in quiet.chain([false]).enumerate() {
        match (q, start) {
            (true, None) => start = Some(k),
            (false, Some(s)) => {
                if secs(k - s) >= MIN_PAUSE - 1e-9 {
                    out.push((secs(s), secs(k)));
                }
                start = None;
            }
            _ => {}
        }
    }
    out
}

/// Whisper stretches a word over the pause after it and starts the next word where
/// that one ends, so a word can sit a second away from where it is heard. Each word
/// is trimmed to its longest stretch outside the pauses; a word lying entirely in a
/// pause keeps whisper's time, the only guess there is.
fn fit_to_speech(words: &mut [Word], pauses: &[(f64, f64)]) {
    for w in words {
        let mut parts = vec![(w.start, w.end)];
        for &(a, b) in pauses.iter().filter(|&&(a, b)| b > w.start && a < w.end) {
            parts = parts
                .into_iter()
                .flat_map(|(p, q)| [(p, q.min(a)), (p.max(b), q)])
                .filter(|(p, q)| q - p > 1e-6)
                .collect();
        }
        if let Some(&(p, q)) = parts.iter().max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0))) {
            (w.start, w.end) = (p, q);
        }
    }
}

/// Silence put between speech stretches in the shortened audio.
const GAP: usize = SAMPLE_RATE / 2;

/// The speech stretches back to back with `GAP` silence between them, and
/// where each one came from.
fn compact(audio: &[f32], chunks: &[(usize, usize)]) -> (Vec<f32>, TimeMap) {
    let mut out = Vec::with_capacity(chunks.iter().map(|(a, b)| b - a + GAP).sum());
    let mut pieces = Vec::with_capacity(chunks.len());
    for &(a, b) in chunks {
        if !out.is_empty() {
            out.resize(out.len() + GAP, 0.0);
        }
        pieces.push((out.len(), a, b - a));
        out.extend_from_slice(&audio[a..b]);
    }
    (out, TimeMap { pieces })
}

/// Maps times in the shortened audio back to the recording.
struct TimeMap {
    /// (start in the shortened audio, start in the recording, length), in samples.
    pieces: Vec<(usize, usize, usize)>,
}

impl TimeMap {
    fn to_source(&self, t: f64) -> f64 {
        let at = (t.max(0.0) * SAMPLE_RATE as f64) as usize;
        // The last piece starting at or before `at`; times in a gap snap to
        // the end of the piece before it.
        let i = self.pieces.partition_point(|&(c, _, _)| c <= at).saturating_sub(1);
        let (c, src, len) = self.pieces[i];
        (src + (at - c.min(at)).min(len)) as f64 / SAMPLE_RATE as f64
    }
}

const SAMPLE_RATE: usize = 16_000;

fn whisper_params() -> FullParams<'static, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
    params.set_n_threads(threads as i32);
    params.set_language(Some(if model_name().ends_with(".en") { "en" } else { "auto" }));
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // One word per segment, with token-level timing.
    params.set_token_timestamps(true);
    params.set_split_on_word(true);
    params.set_max_len(1);
    params
}

/// Appends the words whisper found, shifted by `offset` seconds.
fn collect_words(state: &whisper_rs::WhisperState, offset: f64, words: &mut Vec<Word>) {
    for seg in state.as_iter() {
        let text = seg.to_str_lossy().map(|t| t.trim().to_string()).unwrap_or_default();
        // Skip empty pieces and annotations like [BLANK_AUDIO] or (music).
        if text.is_empty() || text.starts_with('[') || text.starts_with('(') {
            continue;
        }
        let start = offset + seg.start_timestamp() as f64 / 100.0;
        let end = (offset + seg.end_timestamp() as f64 / 100.0).max(start);
        // Punctuation sometimes comes as its own segment: glue it to the previous word.
        if text.chars().all(|c| c.is_ascii_punctuation())
            && let Some(prev) = words.last_mut()
        {
            prev.text.push_str(&text);
            continue;
        }
        words.push(Word { text, start, end });
    }
}

/// Sample ranges with speech, padded and with short gaps bridged so whisper
/// keeps the context of a sentence. `None` without the voice detector.
fn speech_chunks(audio: &[f32]) -> Option<Vec<(usize, usize)>> {
    let model = vad_model()?;
    let mut ctx_params = WhisperVadContextParams::default();
    ctx_params.set_n_threads(std::thread::available_parallelism().map_or(2, |n| n.get()).min(4) as i32);
    let mut vad = WhisperVadContext::new(&model.to_string_lossy(), ctx_params).ok()?;
    let segments = vad.segments_from_samples(WhisperVadParams::new(), audio).ok()?;
    // Centiseconds to samples.
    let ranges = segments.map(|s| ((s.start / 100.0 * SAMPLE_RATE as f32) as usize, (s.end / 100.0 * SAMPLE_RATE as f32) as usize));
    Some(merge_ranges(ranges, audio.len()))
}

/// Pads each range by 0.2 s and joins ranges less than a second apart.
fn merge_ranges(ranges: impl IntoIterator<Item = (usize, usize)>, len: usize) -> Vec<(usize, usize)> {
    let pad = SAMPLE_RATE / 5;
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in ranges {
        let (a, b) = (a.saturating_sub(pad), (b + pad).min(len));
        if a >= b {
            continue;
        }
        match out.last_mut() {
            Some(last) if a <= last.1 + SAMPLE_RATE => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn shortened_audio_maps_back() {
        let s = super::SAMPLE_RATE;
        let audio = vec![0.5f32; 20 * s];
        let (compact, map) = super::compact(&audio, &[(2 * s, 4 * s), (10 * s, 11 * s)]);
        assert_eq!(compact.len(), 2 * s + super::GAP + s);
        assert_eq!(map.to_source(1.0), 3.0);
        // In the gap: end of the first stretch.
        assert_eq!(map.to_source(2.2), 4.0);
        assert_eq!(map.to_source(2.5 + 0.25), 10.25);
    }

    #[test]
    fn words_are_trimmed_to_speech() {
        use crate::project::Word;
        let s = super::SAMPLE_RATE;
        // Speech 0-1 s, quiet 1-3 s, speech 3-4 s, a 0.1 s dip at 3.5 s.
        let mut audio = vec![0.3f32; 4 * s];
        audio[s..3 * s].fill(0.0);
        audio[7 * s / 2..7 * s / 2 + s / 10].fill(0.0);
        let pauses = super::pauses(&audio);
        assert_eq!(pauses, vec![(1.0, 3.0)]);
        let word = |start, end| Word { text: "x".into(), start, end };
        let mut words = vec![word(0.2, 2.5), word(2.5, 3.8), word(1.5, 2.0)];
        super::fit_to_speech(&mut words, &pauses);
        let times: Vec<_> = words.iter().map(|w| (w.start, w.end)).collect();
        assert_eq!(times, vec![(0.2, 1.0), (3.0, 3.8), (1.5, 2.0)]);
    }

    #[test]
    fn speech_ranges_are_padded_and_bridged() {
        let s = super::SAMPLE_RATE;
        // 0.5 s gap: bridged. 3 s gap: kept apart. Padding stops at the ends.
        let got = super::merge_ranges([(s, 2 * s), (5 * s / 2, 3 * s), (6 * s, 20 * s)], 10 * s);
        assert_eq!(got, vec![(s - s / 5, 3 * s + s / 5), (6 * s - s / 5, 10 * s)]);
    }

    /// Downloads the model and whisper.cpp's JFK sample. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn transcribes_sample_with_word_times() {
        let dir = std::env::temp_dir().join(format!("scribecut-asr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("jfk.wav");
        let mut resp = ureq::get("https://github.com/ggerganov/whisper.cpp/raw/master/samples/jfk.wav").call().unwrap();
        std::fs::write(&wav, resp.body_mut().read_to_vec().unwrap()).unwrap();

        let words = super::transcribe(&wav, |_| {}).unwrap();
        let text: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
        println!("{text:?}");
        println!("{:?}", words.iter().map(|w| (w.start, w.end)).collect::<Vec<_>>());
        assert!(text.iter().any(|w| w.to_lowercase().starts_with("country")));
        assert!(words.windows(2).all(|p| p[0].start <= p[1].start));
        assert!(words.iter().all(|w| w.end <= 11.5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
