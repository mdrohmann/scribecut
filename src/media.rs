//! Thin wrappers around the ffmpeg/ffprobe/fc-match command line tools.
//! ffmpeg gets `-nostdin` and a null stdin everywhere: otherwise it touches the
//! terminal, and a backgrounded scribecut is stopped by SIGTTOU (hangs).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Where package managers put their binaries, searched after `PATH`.
///
/// An app bundle launched from the Finder or the Dock inherits a bare
/// `PATH` of `/usr/bin:/bin:/usr/sbin:/sbin`, so ffmpeg installed by MacPorts
/// or Homebrew would be invisible without this.
const EXTRA_BIN_DIRS: [&str; 4] = ["/opt/local/bin", "/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"];

/// Resolves a helper tool to a path. `SCRIBECUT_FFMPEG` / `SCRIBECUT_FFPROBE`
/// override the lookup for those two.
pub fn tool(name: &str) -> PathBuf {
    let override_var = format!("SCRIBECUT_{}", name.to_uppercase());
    if let Some(path) = std::env::var_os(override_var).map(PathBuf::from).filter(|p| p.is_file()) {
        return path;
    }
    let on_path = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .iter()
        .map(|d| d.join(name))
        .chain(EXTRA_BIN_DIRS.iter().map(|d| Path::new(d).join(name)))
        .find(|p| is_executable(p))
        // Fall back to the bare name so the error says "ffmpeg", not a guess.
        .unwrap_or_else(|| PathBuf::from(name))
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    path.is_file()
}

/// `Command` for ffmpeg, wherever it is installed.
pub fn ffmpeg() -> Command {
    Command::new(tool("ffmpeg"))
}

pub struct ProbeInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub has_audio: bool,
}

pub fn probe(path: &Path) -> Result<ProbeInfo> {
    let out = Command::new(tool("ffprobe"))
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
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .context("no video stream")?;
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
        width: video["width"].as_u64().unwrap_or(0) as u32,
        height: video["height"].as_u64().unwrap_or(0) as u32,
        fps,
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
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
    let status = ffmpeg()
        .stdin(Stdio::null())
        .args(["-nostdin", "-y", "-v", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-c", "copy"])
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
///
/// The voice cleanup runs here too: it is not only about how the export sounds.
/// On a real recording it took whisper from 128 recognised words to 188, and it
/// also makes the editor's waveform show the gaps between words more clearly.
pub fn extract_audio_16k(path: &Path) -> Result<Vec<f32>> {
    let mut child = ffmpeg();
    child
        .stdin(Stdio::null())
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", "16000"]);
    if let Some(cleanup) = crate::audio::cleanup() {
        child.args(["-af", &cleanup]);
    }
    let mut child = child
        .args(["-f", "f32le", "-"])
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
///
/// ffmpeg's `drawtext` is given this file by path rather than a font name:
/// macOS builds have no fontconfig, so a name would not resolve.
pub fn overlay_font() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SCRIBECUT_FONT").map(PathBuf::from) {
        return path.is_file().then_some(path);
    }
    fontconfig_font().or_else(bundled_font)
}

/// Asks fontconfig for a bold sans face (Linux and other X desktops).
fn fontconfig_font() -> Option<PathBuf> {
    let out = Command::new(tool("fc-match")).args(["-f", "%{file}", "Sans:bold"]).output().ok()?;
    let path = PathBuf::from(String::from_utf8(out.stdout).ok()?);
    path.is_file().then_some(path)
}

/// Faces shipped with the OS, for systems without fontconfig. ffmpeg's
/// `drawtext` cannot read `.ttc` collections or macOS `.dfont`, so these are
/// all plain `.ttf`/`.otf` files.
fn bundled_font() -> Option<PathBuf> {
    const CANDIDATES: [&str; 5] = [
        "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
        "/System/Library/Fonts/Supplemental/Verdana Bold.ttf",
        "/System/Library/Fonts/Supplemental/Tahoma Bold.ttf",
        "/Library/Fonts/Arial Bold.ttf",
        "/System/Library/Fonts/SFNSRounded.ttf",
    ];
    CANDIDATES.iter().map(PathBuf::from).find(|p| p.is_file())
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
