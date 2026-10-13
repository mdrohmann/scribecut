//! Screen, microphone and camera recording.
//!
//! Each record/resume span is written to its own segment file; the segments are
//! concatenated when recording stops. Where the frames come from is up to the
//! backend: the xdg-desktop-portal ScreenCast portal (PipeWire) on Linux,
//! AVFoundation on macOS.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::geom::{Area, Output};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as backend;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as backend;

#[allow(unused_imports)] // named by neither caller, but `connect` returns it
pub use backend::Connection;
pub use backend::{Recorder, camera_available, connect, default_camera};

/// What to record.
pub enum Source {
    /// A whole monitor: picked in the portal's dialog on Linux, named up front
    /// on macOS, where the app has to offer the choice itself. The portal asks
    /// the user, so the Linux backend ignores the output named here.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    Monitor(Option<Output>),
    /// A single window, chosen in the system's own picker. Only where
    /// [`supports_system_windows`] is true.
    SystemWindow,
    /// A rectangle of the screen, recorded by cropping the monitor it is on.
    Area(Area),
    /// A window listed by [`crate::platform`]: brought to the front, then
    /// recorded like an area, pausing automatically while it isn't visible.
    Window { id: i64, pid: i32 },
}

/// Whether the system itself can offer a single-window picker (the portal on
/// some Linux desktops). Elsewhere scribecut lists windows and crops.
pub fn supports_system_windows() -> bool {
    backend::supports_system_windows()
}

/// Whether the system puts up its own "which screen?" dialog when a capture
/// starts, as the portal does. Where it does not, scribecut has to ask first.
pub const SYSTEM_MONITOR_PICKER: bool = cfg!(target_os = "linux");

/// Joins the recorded pieces into one file. Returns it and the start time of
/// each piece after the first, which the editor draws as resume points.
///
/// Shared by the backends: only the recording differs between platforms, not
/// what is done with the files afterwards.
pub fn join_pieces(dir: &Path, files: Vec<PathBuf>) -> Result<(PathBuf, Vec<f64>)> {
    let pieces: Vec<PathBuf> = files.into_iter().filter(|p| non_empty(p)).collect();
    if pieces.is_empty() {
        bail!("nothing was recorded");
    }
    let mut starts = Vec::new();
    let mut t = 0.0;
    for piece in &pieces[..pieces.len() - 1] {
        t += crate::media::probe(piece)?.duration;
        starts.push(t);
    }
    let out = dir.join("recording.mkv");
    if pieces.len() == 1 {
        std::fs::rename(&pieces[0], &out)?;
    } else {
        crate::media::concat(&pieces, &out)?;
        for piece in &pieces {
            let _ = std::fs::remove_file(piece);
        }
    }
    Ok((out, starts))
}

/// A segment with nothing in it but a container header is not a recording.
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
