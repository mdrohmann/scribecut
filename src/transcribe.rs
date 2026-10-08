//! Local speech-to-text with word timestamps via whisper.cpp.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

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

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(12);
    params.set_n_threads(threads as i32);
    if model_name().ends_with(".en") {
        params.set_language(Some("en"));
    } else {
        params.set_language(Some("auto"));
    }
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // One word per segment, with token-level timing.
    params.set_token_timestamps(true);
    params.set_split_on_word(true);
    params.set_max_len(1);
    params.set_progress_callback_safe(move |p: i32| status(Status::Transcribing(p as f32 / 100.0)));

    state.full(params, &audio).context("running whisper")?;

    let mut words = Vec::new();
    for seg in state.as_iter() {
        let text = seg.to_str_lossy().map(|t| t.trim().to_string()).unwrap_or_default();
        // Skip empty pieces and annotations like [BLANK_AUDIO] or (music).
        if text.is_empty() || text.starts_with('[') || text.starts_with('(') {
            continue;
        }
        let start = seg.start_timestamp() as f64 / 100.0;
        let end = (seg.end_timestamp() as f64 / 100.0).max(start);
        // Punctuation sometimes comes as its own segment: glue it to the previous word.
        if text.chars().all(|c| c.is_ascii_punctuation())
            && let Some(prev) = words.last_mut()
        {
            let prev: &mut Word = prev;
            prev.text.push_str(&text);
            continue;
        }
        words.push(Word { text, start, end });
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
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
