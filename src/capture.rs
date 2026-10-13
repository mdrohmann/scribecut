//! Screen + microphone recording.
//!
//! Each record/resume span is written to its own segment file; the segments are
//! concatenated when recording stops. Where the frames come from is up to the
//! backend: the xdg-desktop-portal ScreenCast portal (PipeWire) on Linux,
//! AVFoundation on macOS.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::geom::{Area, Crop, Output};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

pub use backend::Target;

/// What to record.
pub enum Source {
    /// A whole monitor: picked in the portal's dialog on Linux, named up front
    /// on macOS (where the app has to offer its own choice).
    Monitor(Option<Output>),
    /// A single window, chosen in the system's own picker. Only on desktops
    /// where [`supports_system_windows`] is true.
    SystemWindow,
    /// A rectangle of the screen (a region, or a window's frame), recorded by
    /// cropping the monitor it is on.
    Area(Area),
}

/// Whether the system itself can offer a single-window picker (the portal on
/// some Linux desktops). Elsewhere scribecut lists windows and crops.
pub fn supports_system_windows() -> bool {
    backend::supports_system_windows()
}

/// Whether the system puts up its own "which screen?" dialog when a capture
/// starts, as the portal does. Where it does not, scribecut has to ask first.
pub const SYSTEM_MONITOR_PICKER: bool = cfg!(target_os = "linux");

/// Opens the screen stream for `source`, together with the crop to apply to
/// its frames. Blocks while the user picks, so call this off the UI thread.
pub fn connect(source: Source) -> Result<(Target, Option<Crop>)> {
    backend::connect(source)
}

/// A recording in progress: one screen stream, written to a series of segments.
pub struct Recorder {
    capture: backend::Capture,
    pub dir: PathBuf,
    pub segments: Vec<PathBuf>,
    /// When the segment being recorded started, if any.
    started: Option<Instant>,
    /// Time recorded in the segments already finished.
    recorded: Duration,
}

impl Recorder {
    pub fn new(target: Target, crop: Option<Crop>, dir: PathBuf) -> Result<Self> {
        Ok(Self {
            capture: backend::Capture::new(target, crop)?,
            dir,
            segments: Vec::new(),
            started: None,
            recorded: Duration::ZERO,
        })
    }

    pub fn is_recording(&self) -> bool {
        self.started.is_some()
    }

    pub fn elapsed(&self) -> Duration {
        self.recorded + self.started.map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Starts (or resumes into) a new segment file.
    pub fn start_segment(&mut self) -> Result<()> {
        if self.started.is_some() {
            return Ok(());
        }
        let path = self.dir.join(format!("segment_{:03}.mkv", self.segments.len()));
        self.capture.start(&path)?;
        self.segments.push(path);
        self.started = Some(Instant::now());
        Ok(())
    }

    /// Finishes the current segment (pause). Safe to call when already paused.
    pub fn stop_segment(&mut self) -> Result<()> {
        let Some(started) = self.started.take() else { return Ok(()) };
        self.recorded += started.elapsed();
        self.capture.stop()
    }

    /// Checks for errors (e.g. the user stopped sharing the screen).
    pub fn poll_error(&mut self) -> Option<String> {
        self.capture.poll_error()
    }

    /// Stops recording and joins the pieces. Returns the final file and the
    /// start time of each piece after the first.
    pub fn finish(mut self) -> Result<(PathBuf, Vec<f64>)> {
        let stopped = self.stop_segment();
        self.capture.shutdown();
        stopped?;
        let segments: Vec<PathBuf> = self.segments.into_iter().filter(|s| non_empty(s)).collect();
        if segments.is_empty() {
            bail!("nothing was recorded");
        }
        let mut starts = Vec::new();
        let mut t = 0.0;
        for s in &segments[..segments.len() - 1] {
            t += crate::media::probe(s)?.duration;
            starts.push(t);
        }
        let out = self.dir.join("recording.mkv");
        if segments.len() == 1 {
            std::fs::rename(&segments[0], &out)?;
        } else {
            crate::media::concat(&segments, &out)?;
            for s in &segments {
                let _ = std::fs::remove_file(s);
            }
        }
        Ok((out, starts))
    }
}

fn non_empty(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.len() > 1024)
}

/// A fresh, timestamped folder for a new recording.
pub fn new_recording_dir() -> Result<PathBuf> {
    let base = dirs::video_dir().or_else(dirs::home_dir).context("no home dir")?;
    let dir = base.join("scribecut").join(chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
