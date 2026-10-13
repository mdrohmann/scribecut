//! Thin wrappers around the ffmpeg/ffprobe/fc-match command line tools.
//! ffmpeg gets `-nostdin` and a null stdin everywhere: otherwise it touches the
//! terminal, and a backgrounded scribecut is stopped by SIGTTOU (hangs).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub struct ProbeInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
    /// Size of a second video stream (scribecut's camera track), if any.
    pub camera: Option<(u32, u32)>,
}

pub fn probe(path: &Path) -> Result<ProbeInfo> {
    let out = Command::new("ffprobe")
        .stdin(Stdio::null())
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .context("running ffprobe")?;
    if !out.status.success() {
        bail!("ffprobe failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let streams = v["streams"].as_array().cloned().unwrap_or_default();
    let videos: Vec<&serde_json::Value> = streams
        .iter()
        .filter(|s| s["codec_type"] == "video" && s["disposition"]["attached_pic"] != 1)
        .collect();
    let video = *videos.first().context("no video stream")?;
    let size = |v: &serde_json::Value| (v["width"].as_u64().unwrap_or(0) as u32, v["height"].as_u64().unwrap_or(0) as u32);
    let duration = v["format"]["duration"]
        .as_str()
        .and_then(|d| d.parse().ok())
        .context("unknown duration")?;
    let fps = video["avg_frame_rate"]
        .as_str()
        .and_then(parse_ratio)
        .filter(|f| f.is_finite() && *f > 0.0)
        .or_else(|| video["r_frame_rate"].as_str().and_then(parse_ratio))
        .unwrap_or(30.0);
    Ok(ProbeInfo {
        duration,
        width: size(video).0,
        height: size(video).1,
        fps,
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
        camera: videos.get(1).map(|v| size(v)),
    })
}

fn parse_ratio(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d != 0.0).then(|| n / d)
}

/// Joins recorded pieces into one file without re-encoding.
pub fn concat(segments: &[PathBuf], out: &Path) -> Result<()> {
    let list = out.with_extension("concat.txt");
    let body: String = segments
        .iter()
        .map(|s| format!("file '{}'\n", s.display().to_string().replace('\'', r"'\''")))
        .collect();
    std::fs::write(&list, body)?;
    let status = Command::new("ffmpeg")
        .stdin(Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        // All streams: ffmpeg would otherwise keep only one video (the camera, being larger).
        .args(["-map", "0", "-c", "copy"])
        .arg(out)
        .status()
        .context("running ffmpeg concat")?;
    let _ = std::fs::remove_file(&list);
    if !status.success() {
        bail!("ffmpeg concat failed");
    }
    Ok(())
}

/// Decodes the audio track to 16 kHz mono f32 samples (what Whisper expects).
pub fn extract_audio_16k(path: &Path) -> Result<Vec<f32>> {
    let mut child = Command::new("ffmpeg")
        .stdin(Stdio::null())
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000", "-f", "f32le", "-"])
        .stdout(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;
    let mut bytes = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut bytes)?;
    if !child.wait()?.success() {
        bail!("ffmpeg audio extraction failed");
    }
    Ok(bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}

/// Loudness envelope resolution of [`audio_peaks`].
pub const PEAKS_PER_SEC: usize = 100;

/// Peak amplitude per 10 ms of the audio track, normalised so the loudest is 1.
pub fn audio_peaks(path: &Path) -> Result<Vec<f32>> {
    Ok(peaks(&extract_audio_16k(path)?, 16_000 / PEAKS_PER_SEC))
}

fn peaks(samples: &[f32], window: usize) -> Vec<f32> {
    let mut out: Vec<f32> = samples
        .chunks(window)
        .map(|c| c.iter().fold(0.0f32, |m, s| m.max(s.abs())))
        .collect();
    let max = out.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        out.iter_mut().for_each(|v| *v /= max);
    }
    out
}

/// Font file used both for the preview and for ffmpeg's drawtext, so they match.
pub fn overlay_font() -> Option<PathBuf> {
    let out = Command::new("fc-match").args(["-f", "%{file}", "Sans:bold"]).output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    #[test]
    fn peaks_are_windowed_and_normalised() {
        let samples = [0.1, -0.5, 0.2, 0.0, 0.25, -0.1, 0.05];
        assert_eq!(super::peaks(&samples, 3), vec![1.0, 0.5, 0.1]);
        assert_eq!(super::peaks(&[0.0; 4], 2), vec![0.0, 0.0]);
    }
}
