//! Desktop-specific window, monitor and region picking.
//!
//! Each backend exposes the same small set of calls; everything above this
//! module works in the shared [`crate::geom`] coordinate types. The screen
//! itself is captured by [`crate::capture`], which has its own backends.

pub use crate::geom::{Window, crop_for, output_for};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod sway;

#[cfg(target_os = "macos")]
pub use macos::{available, focus_window, outputs, window_visible, windows};
#[cfg(target_os = "linux")]
pub use sway::{available, click_window, focus_window, outputs, pick_region, set_cursor, window_visible, windows};

/// How the user drags out a rectangle to record on this desktop.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RegionPicker {
    /// An external helper blocks until the user is done (sway + `slurp`).
    External,
    /// scribecut draws its own full-screen overlay (macOS).
    InApp,
    /// Not supported here.
    None,
}

/// Which region picker this build should use. Written with `cfg!` rather than
/// `#[cfg]` so every variant still counts as reachable on every platform.
pub fn region_picker() -> RegionPicker {
    if cfg!(target_os = "macos") {
        RegionPicker::InApp
    } else if cfg!(target_os = "linux") && available() {
        RegionPicker::External
    } else {
        RegionPicker::None
    }
}

/// Whether the user can pick a window by clicking it on screen.
pub const CAN_CLICK_WINDOW: bool = cfg!(target_os = "linux");

/// Whether the pointer can be enlarged for the duration of a recording. sway
/// can be told to; macOS only has a global accessibility setting that needs
/// applications to be restarted, which is no use for a recording.
pub const CAN_SET_POINTER_SIZE: bool = cfg!(target_os = "linux");

/// Stand-ins for the external pickers, so callers need no `cfg` of their own:
/// where [`region_picker`] is not [`RegionPicker::External`] these are unused.
#[cfg(not(target_os = "linux"))]
mod external {
    use crate::geom::{Area, Window};
    use anyhow::{Result, bail};

    pub fn pick_region() -> Result<Option<Area>> {
        bail!("this desktop has no external region picker")
    }

    pub fn click_window(_windows: &[Window]) -> Result<Option<i64>> {
        bail!("this desktop cannot pick a window by clicking it")
    }
}

#[cfg(not(target_os = "linux"))]
pub use external::{click_window, pick_region};

/// Fallbacks for desktops without a backend: no window or region recording,
/// only whatever [`crate::capture`] can offer for a whole screen.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod fallback {
    use crate::geom::{Area, Output, Window};
    use anyhow::{Result, bail};

    pub fn available() -> bool {
        false
    }

    pub fn windows() -> Result<Vec<Window>> {
        bail!("listing windows is not supported on this desktop")
    }

    pub fn focus_window(_id: i64, _pid: i32) -> Result<Area> {
        bail!("focusing windows is not supported on this desktop")
    }

    pub fn outputs() -> Result<Vec<Output>> {
        bail!("listing monitors is not supported on this desktop")
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub use fallback::{available, focus_window, outputs, windows};
