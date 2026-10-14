//! Window and monitor listing on macOS via CoreGraphics. There is no portal
//! here: AVFoundation only hands out whole displays, so a window recording is
//! the display cropped to the window's frame, as on sway.
//!
//! Coordinates are CoreGraphics global display coordinates: points, origin at
//! the top-left of the main display, y growing downwards — the same convention
//! [`crate::geom`] uses.

use anyhow::{Context, Result, bail};
use core_foundation::base::{CFType, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::window::{
    copy_window_info, kCGNullWindowID, kCGWindowAlpha, kCGWindowBounds, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
    kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
};

use crate::geom::{Area, Output, Window};

/// CoreGraphics is always there on macOS.
pub fn available() -> bool {
    true
}

/// A dictionary keyed by the `kCGWindow*` strings.
type Dict = CFDictionary<CFString, CFType>;

fn value(dict: &Dict, key: CFStringRef) -> Option<CFType> {
    // The `kCGWindow*` keys are CoreFoundation string constants we only borrow.
    let key = unsafe { CFString::wrap_under_get_rule(key) };
    dict.find(&key).map(|v| v.clone())
}

fn number(dict: &Dict, key: CFStringRef) -> Option<f64> {
    value(dict, key)?.downcast::<CFNumber>()?.to_f64()
}

fn string(dict: &Dict, key: CFStringRef) -> Option<String> {
    Some(value(dict, key)?.downcast::<CFString>()?.to_string())
}

/// Reads a `kCGWindowBounds`-style `{X, Y, Width, Height}` dictionary.
fn area(dict: &Dict, key: CFStringRef) -> Option<Area> {
    let bounds = value(dict, key)?.downcast::<CFDictionary>()?;
    // Checked to be a dictionary by the downcast above; re-typed so the
    // `kCGWindowBounds` members can be looked up by name.
    let bounds: Dict = unsafe { CFDictionary::wrap_under_get_rule(bounds.as_concrete_TypeRef()) };
    let at = |name: &str| -> Option<f64> {
        bounds.find(&CFString::new(name))?.downcast::<CFNumber>()?.to_f64()
    };
    Some(Area {
        x: at("X")? as i32,
        y: at("Y")? as i32,
        w: at("Width")? as i32,
        h: at("Height")? as i32,
    })
}

/// Windows too small to be worth recording (menu extras, tooltips, shadows).
const MIN_SIZE: i32 = 120;

/// On-screen application windows, browsers first.
///
/// Window *titles* need the Screen Recording permission; without it macOS
/// hands out only the owning application's name, which is what we fall back to.
pub fn windows() -> Result<Vec<Window>> {
    let list = copy_window_info(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
        kCGNullWindowID,
    )
    .context("CGWindowListCopyWindowInfo returned nothing")?;

    let own_pid = std::process::id() as i32;
    let mut found: Vec<Window> = Vec::new();
    for item in list.iter() {
        // Each element of the window list is a window description dictionary.
        let dict: Dict = unsafe { CFDictionary::wrap_under_get_rule(*item as _) };
        // Layer 0 is the normal document/window layer; the rest is chrome
        // (menu bar, dock, status items, overlays).
        if number(&dict, unsafe { kCGWindowLayer }).unwrap_or(1.0) != 0.0 {
            continue;
        }
        if number(&dict, unsafe { kCGWindowAlpha }).unwrap_or(1.0) <= 0.0 {
            continue;
        }
        let Some(area) = area(&dict, unsafe { kCGWindowBounds }) else { continue };
        if area.w < MIN_SIZE || area.h < MIN_SIZE {
            continue;
        }
        let pid = number(&dict, unsafe { kCGWindowOwnerPID }).unwrap_or(0.0) as i32;
        if pid == own_pid {
            continue;
        }
        let app_id = string(&dict, unsafe { kCGWindowOwnerName }).unwrap_or_default();
        if app_id.is_empty() {
            continue;
        }
        let title = string(&dict, unsafe { kCGWindowName }).filter(|t| !t.is_empty()).unwrap_or_else(|| app_id.clone());
        found.push(Window {
            id: number(&dict, unsafe { kCGWindowNumber }).unwrap_or(0.0) as i64,
            pid,
            title,
            app_id,
            // `kCGWindowListOptionOnScreenOnly` only reports windows on the
            // current Space, so everything listed is visible.
            visible: true,
            area,
        });
    }
    // An app often has several layer-0 windows stacked exactly on top of each
    // other (a browser's tab-strip and content host, say). They crop to the
    // same pixels, so offering them all is just noise; the list is front to
    // back, so the first one is the one in front.
    found.dedup_by(|a, b| a.app_id == b.app_id && a.area == b.area);
    found.sort_by_key(|w| !w.is_browser());
    Ok(found)
}

/// Brings the window's application to the front and returns the window's
/// current frame. macOS has no API to raise one window of another app without
/// the Accessibility permission, so the whole app is activated instead; if the
/// chosen window is not that app's front window, the user has to raise it.
pub fn focus_window(id: i64, pid: i32) -> Result<Area> {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    if let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) {
        app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows);
    }
    // Give the window server a moment to restack before measuring.
    std::thread::sleep(std::time::Duration::from_millis(250));
    windows()?
        .into_iter()
        .find(|w| w.id == id)
        .map(|w| w.area)
        .context("that window is no longer on screen")
}

/// Whether a window is still on screen; `None` if it no longer exists.
///
/// `kCGWindowListOptionOnScreenOnly` only reports windows on the current Space,
/// so "listed" and "visible" are the same question here. A window that has been
/// closed is gone from the list entirely, which is the `None` case; one that is
/// merely on another Space or minimised reads as not visible.
pub fn window_visible(id: i64) -> Result<Option<bool>> {
    if windows()?.iter().any(|w| w.id == id) {
        return Ok(Some(true));
    }
    // Ask again including off-screen windows, to tell "hidden" from "closed".
    let all = copy_window_info(kCGWindowListExcludeDesktopElements, kCGNullWindowID)
        .context("CGWindowListCopyWindowInfo returned nothing")?;
    for item in all.iter() {
        // Each element of the window list is a window description dictionary.
        let dict: Dict = unsafe { CFDictionary::wrap_under_get_rule(*item as _) };
        if number(&dict, unsafe { kCGWindowNumber }).unwrap_or(-1.0) as i64 == id {
            return Ok(Some(false));
        }
    }
    Ok(None)
}

/// All active displays, with both their logical layout size and their real
/// pixel size, so Retina scaling can be worked out.
pub fn outputs() -> Result<Vec<Output>> {
    let ids = CGDisplay::active_displays().map_err(|e| anyhow::anyhow!("CGGetActiveDisplayList failed ({e})"))?;
    if ids.is_empty() {
        bail!("no active displays");
    }
    let main = CGDisplay::main().id;
    Ok(ids
        .into_iter()
        .enumerate()
        .map(|(i, id)| {
            let display = CGDisplay::new(id);
            let b = display.bounds();
            let area = Area {
                x: b.origin.x as i32,
                y: b.origin.y as i32,
                w: b.size.width as i32,
                h: b.size.height as i32,
            };
            // A Retina display reports more backing pixels than points.
            let pixels = display
                .display_mode()
                .map(|m| (m.pixel_width() as i32, m.pixel_height() as i32))
                .filter(|(w, h)| *w > 0 && *h > 0)
                .unwrap_or((area.w, area.h));
            Output {
                name: if id == main { format!("Display {} (main)", i + 1) } else { format!("Display {}", i + 1) },
                area,
                pixels,
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    /// Enumeration must work without any permission prompt, and the geometry
    /// must be self-consistent: every display has a sane size and scale.
    ///
    /// A sleeping display counts as no display at all, which is an ordinary
    /// state for a laptop, so that is skipped rather than failed.
    #[test]
    fn lists_displays() {
        let Ok(outputs) = super::outputs() else {
            eprintln!("skipping: no awake display");
            return;
        };
        assert!(!outputs.is_empty());
        for o in &outputs {
            assert!(o.area.w > 0 && o.area.h > 0, "{o:?}");
            let scale = o.pixels.0 as f64 / o.area.w as f64;
            assert!((1.0..=4.0).contains(&scale), "odd scale {scale} for {o:?}");
        }
    }

    /// Listing windows must not fail, whatever the permission state.
    #[test]
    fn lists_windows() {
        let windows = super::windows().unwrap();
        for w in &windows {
            assert!(!w.app_id.is_empty());
            assert!(w.area.w >= super::MIN_SIZE && w.area.h >= super::MIN_SIZE);
        }
    }
}
