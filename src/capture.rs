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
pub mod chooser;
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
    /// A window listed by [`crate::platform`]. Shared on its own through the
    /// portal where [`records_windows_directly`]; otherwise brought to the
    /// front and recorded like an area, pausing while it isn't visible.
    Window { id: i64, pid: i32, toplevel: Option<String> },
}

/// Whether the system itself can offer a single-window picker (the portal on
/// some Linux desktops). Elsewhere scribecut lists windows and crops.
pub fn supports_system_windows() -> bool {
    backend::supports_system_windows()
}

/// Whether the system puts up its own "which screen?" dialog when a capture
/// starts, as the portal does unless scribecut's chooser answers it. Where it
/// does not, scribecut has to ask first.
pub fn system_monitor_picker() -> bool {
    #[cfg(target_os = "linux")]
    return !chooser::installed();
    #[cfg(not(target_os = "linux"))]
    return false;
}

/// Whether a window from scribecut's list can be recorded on its own (sway
/// >= 1.12 with scribecut's portal chooser), rather than cut out of the screen.
pub fn records_windows_directly(system_windows: bool) -> bool {
    #[cfg(target_os = "linux")]
    return system_windows && crate::platform::available() && chooser::installed();
    #[cfg(not(target_os = "linux"))]
    return {
        let _ = system_windows;
        false
    };
}

/// Whether installing scribecut's portal chooser would enable
/// [`records_windows_directly`].
pub fn can_set_up_window_capture(system_windows: bool) -> bool {
    cfg!(target_os = "linux")
        && system_windows
        && crate::platform::available()
        && !records_windows_directly(system_windows)
}

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

/// The name a new project gets unless the user picks one: the current time.
pub fn default_project_name() -> String {
    chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string()
}

/// A fresh folder for a new recording, named after the project. An existing
/// folder of that name is never reused: " 2", " 3"… is appended instead.
pub fn new_recording_dir(name: &str) -> Result<PathBuf> {
    let base = dirs::video_dir().or_else(dirs::home_dir).context("no home dir")?;
    let dir = unused_dir(&base.join("scribecut"), name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// `parent/<name>`, with the name made safe for a folder and numbered if taken.
fn unused_dir(parent: &Path, name: &str) -> PathBuf {
    let clean: String = name
        .trim()
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == ':' || c.is_control() { '-' } else { c })
        .collect();
    let clean = clean.trim_start_matches('.').trim();
    let clean = if clean.is_empty() { default_project_name() } else { clean.to_owned() };
    (1..)
        .map(|n| parent.join(if n == 1 { clean.clone() } else { format!("{clean} {n}") }))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folders_are_safe_and_never_reused() {
        let parent = std::env::temp_dir().join(format!("scribecut-dirs-{}", std::process::id()));
        std::fs::create_dir_all(parent.join("Billing demo")).unwrap();
        assert_eq!(unused_dir(&parent, "  Billing demo "), parent.join("Billing demo 2"));
        assert_eq!(unused_dir(&parent, "a/b: c"), parent.join("a-b- c"));
        assert_eq!(unused_dir(&parent, "../up"), parent.join("-up"));
        // Nothing usable left: falls back to a timestamp.
        assert!(unused_dir(&parent, " .. ").file_name().unwrap().to_str().unwrap().starts_with("20"));
        let _ = std::fs::remove_dir_all(&parent);
    }
}
