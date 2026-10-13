//! Recording preferences, kept in `~/.config/scribecut/settings.toml`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Record the camera as a second video track.
    pub record_camera: bool,
    pub camera_device: String,
    /// Enlarge the mouse pointer while recording. sway only: macOS has no way
    /// to change the pointer size for the duration of a recording.
    pub big_pointer: bool,
    pub pointer_scale: f32,
}

impl Default for Settings {
    fn default() -> Self {
        // What names a camera differs by platform: a V4L2 device node on Linux,
        // an AVFoundation device name on macOS, so the capture backend decides.
        let camera_device = crate::capture::default_camera();
        let record_camera = crate::capture::camera_available(&camera_device);
        Self { record_camera, camera_device, big_pointer: true, pointer_scale: 2.0 }
    }
}

impl Settings {
    fn path() -> Option<std::path::PathBuf> {
        Some(dirs::config_dir()?.join("scribecut/settings.toml"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let (Some(path), Ok(text)) = (Self::path(), toml::to_string(self)) {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            let _ = std::fs::write(path, text);
        }
    }

    pub fn camera_available(&self) -> bool {
        crate::capture::camera_available(&self.camera_device)
    }
}
