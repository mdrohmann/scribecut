//! Local speech-to-text with word timestamps via whisper.cpp.

use std::io::{Read, Write};
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
    let mut resp = ureq::get(&url).call().with_context(|| format!("downloading {url}"))?;
    let total = resp.body().content_length().unwrap_or(0);
    let part = path.with_extension("part");
    let mut out = std::fs::File::create(&part)?;
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])?;
        done += n as u64;
        if total > 0 {
            status(Status::Downloading(done as f32 / total as f32));
        }
    }
    out.sync_all()?;
    std::fs::rename(&part, &path)?;
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
    Ok(words)
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
