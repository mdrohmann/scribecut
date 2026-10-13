//! Window and region picking on sway, where the screencast portal can only
//! share whole monitors: we capture the monitor and crop to the chosen area.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// A rectangle in sway's global layout coordinates (logical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Area {
    fn from_json(v: &Value) -> Self {
        let n = |k: &str| v[k].as_i64().unwrap_or(0) as i32;
        Self { x: n("x"), y: n("y"), w: n("width"), h: n("height") }
    }

    fn contains_center_of(&self, o: &Area) -> bool {
        let (cx, cy) = (o.x + o.w / 2, o.y + o.h / 2);
        cx >= self.x && cx < self.x + self.w && cy >= self.y && cy < self.y + self.h
    }
}

#[derive(Clone, Debug)]
pub struct Window {
    pub con_id: i64,
    pub title: String,
    pub app_id: String,
    pub area: Area,
    pub visible: bool,
}

impl Window {
    pub fn is_browser(&self) -> bool {
        const BROWSERS: [&str; 9] =
            ["firefox", "chrom", "brave", "vivaldi", "opera", "epiphany", "librewolf", "zen", "edge"];
        let id = self.app_id.to_lowercase();
        BROWSERS.iter().any(|b| id.contains(b))
    }
}

#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    pub area: Area,
    /// Physical pixel size of the output's current mode.
    pub pixels: (i32, i32),
}

/// Pixels to remove from each edge of a monitor frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Crop {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

pub fn available() -> bool {
    std::env::var_os("SWAYSOCK").is_some()
}

fn swaymsg(kind: &str) -> Result<Value> {
    let out = Command::new("swaymsg")
        .args(["-r", "-t", kind])
        .stdin(Stdio::null())
        .output()
        .context("running swaymsg")?;
    if !out.status.success() {
        bail!("swaymsg -t {kind} failed");
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// All application windows, browsers first.
pub fn windows() -> Result<Vec<Window>> {
    let mut found = Vec::new();
    collect_windows(&swaymsg("get_tree")?, &mut found);
    found.sort_by_key(|w| !w.is_browser());
    Ok(found)
}

fn collect_windows(node: &Value, out: &mut Vec<Window>) {
    if node["pid"].is_number() && node["type"] != "workspace" {
        // `window_rect` is the content area, relative to the container `rect`.
        let rect = Area::from_json(&node["rect"]);
        let inner = Area::from_json(&node["window_rect"]);
        let app_id = node["app_id"]
            .as_str()
            .or_else(|| node["window_properties"]["class"].as_str())
            .unwrap_or("")
            .to_string();
        if app_id != "scribecut" {
            out.push(Window {
                con_id: node["id"].as_i64().unwrap_or(0),
                title: node["name"].as_str().unwrap_or("").to_string(),
                app_id,
                area: Area { x: rect.x + inner.x, y: rect.y + inner.y, w: inner.w, h: inner.h },
                visible: node["visible"].as_bool().unwrap_or(false),
            });
        }
    }
    for key in ["nodes", "floating_nodes"] {
        for child in node[key].as_array().into_iter().flatten() {
            collect_windows(child, out);
        }
    }
}

/// Brings a window to the front (switching workspace if needed) and returns its
/// current content area.
pub fn focus_window(con_id: i64) -> Result<Area> {
    let ok = Command::new("swaymsg")
        .arg(format!("[con_id={con_id}] focus"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?
        .success();
    if !ok {
        bail!("that window no longer exists");
    }
    std::thread::sleep(std::time::Duration::from_millis(150));
    windows()?
        .into_iter()
        .find(|w| w.con_id == con_id)
        .map(|w| w.area)
        .context("that window no longer exists")
}

/// Sets the pointer theme and size on all seats.
pub fn set_cursor(theme: &str, size: u32) -> Result<()> {
    let ok = Command::new("swaymsg")
        .args(["seat", "*", "xcursor_theme", theme, &size.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()?
        .success();
    if !ok {
        bail!("swaymsg could not set the cursor size");
    }
    Ok(())
}

pub fn outputs() -> Result<Vec<Output>> {
    Ok(swaymsg("get_outputs")?
        .as_array()
        .into_iter()
        .flatten()
        .filter(|o| o["active"].as_bool().unwrap_or(false))
        .map(|o| Output {
            name: o["name"].as_str().unwrap_or("").to_string(),
            area: Area::from_json(&o["rect"]),
            pixels: (
                o["current_mode"]["width"].as_i64().unwrap_or(0) as i32,
                o["current_mode"]["height"].as_i64().unwrap_or(0) as i32,
            ),
        })
        .collect())
}

/// Lets the user drag out a rectangle. `None` if cancelled (Esc).
pub fn pick_region() -> Result<Option<Area>> {
    slurp(&["-f", "%x %y %w %h"], None)
}

/// Lets the user click one of the given windows; returns its container id.
/// `None` if cancelled.
pub fn click_window(windows: &[Window]) -> Result<Option<i64>> {
    let visible: Vec<&Window> = windows.iter().filter(|w| w.visible).collect();
    let boxes: String = visible
        .iter()
        .map(|w| format!("{},{} {}x{}\n", w.area.x, w.area.y, w.area.w, w.area.h))
        .collect();
    let picked = slurp(&["-r", "-f", "%x %y %w %h"], Some(boxes))?;
    Ok(picked.and_then(|a| visible.iter().find(|w| w.area == a).map(|w| w.con_id)))
}

/// Whether a window is on a visible workspace; `None` if it no longer exists.
pub fn window_visible(con_id: i64) -> Result<Option<bool>> {
    Ok(windows()?.into_iter().find(|w| w.con_id == con_id).map(|w| w.visible))
}

fn slurp(args: &[&str], input: Option<String>) -> Result<Option<Area>> {
    let mut child = Command::new("slurp")
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running slurp (is it installed?)")?;
    if let Some(input) = input {
        child.stdin.take().unwrap().write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Ok(None);
    }
    let nums: Vec<i32> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    match nums[..] {
        [x, y, w, h] if w > 0 && h > 0 => Ok(Some(Area { x, y, w, h })),
        _ => Ok(None),
    }
}

/// The output an area is (mostly) on.
pub fn output_for(area: &Area, outputs: &[Output]) -> Option<Output> {
    outputs.iter().find(|o| o.area.contains_center_of(area)).cloned()
}

/// Converts an area to a crop of its output's frame, clipped to the output
/// and rounded to even pixels (needed for 4:2:0 video).
pub fn crop_for(area: &Area, output: &Output) -> Crop {
    let scale_x = output.pixels.0 as f64 / output.area.w.max(1) as f64;
    let scale_y = output.pixels.1 as f64 / output.area.h.max(1) as f64;
    let even = |v: f64| ((v / 2.0).round() as i32 * 2).max(0);
    let left = even((area.x - output.area.x).max(0) as f64 * scale_x);
    let top = even((area.y - output.area.y).max(0) as f64 * scale_y);
    let right_edge = ((area.x + area.w).min(output.area.x + output.area.w) - output.area.x) as f64 * scale_x;
    let bottom_edge = ((area.y + area.h).min(output.area.y + output.area.h) - output.area.y) as f64 * scale_y;
    let right = even(output.pixels.0 as f64 - right_edge);
    let bottom = even(output.pixels.1 as f64 - bottom_edge);
    Crop { left, top, right, bottom }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(x: i32, scale: f64) -> Output {
        Output {
            name: "X".into(),
            area: Area { x, y: 0, w: 1000, h: 500 },
            pixels: ((1000.0 * scale) as i32, (500.0 * scale) as i32),
        }
    }

    #[test]
    fn crop_scales_clips_and_rounds() {
        let o = output(1000, 2.0);
        // 101 logical px in from the left at scale 2 = 202 px; clipped at the right edge.
        let c = crop_for(&Area { x: 1101, y: 51, w: 2000, h: 99 }, &o);
        assert_eq!(c, Crop { left: 202, top: 102, right: 0, bottom: 700 });
        let c = crop_for(&Area { x: 1000, y: 0, w: 1000, h: 500 }, &output(1000, 1.0));
        assert_eq!(c, Crop { left: 0, top: 0, right: 0, bottom: 0 });
    }

    #[test]
    fn finds_output_by_center() {
        let outs = [output(0, 1.0), output(1000, 1.0)];
        let picked = output_for(&Area { x: 900, y: 10, w: 400, h: 10 }, &outs).unwrap();
        assert_eq!(picked.area.x, 1000);
    }

    #[test]
    fn parses_sway_tree() {
        let tree: Value = serde_json::from_str(
            r#"{"type":"root","nodes":[{"type":"workspace","nodes":[
                {"id":7,"pid":1,"type":"con","name":"GitHub — Mozilla Firefox","app_id":"firefox","visible":true,
                 "rect":{"x":10,"y":20,"width":500,"height":400},
                 "window_rect":{"x":2,"y":25,"width":496,"height":373},"nodes":[],"floating_nodes":[]},
                {"id":8,"pid":2,"type":"con","name":"scribecut","app_id":"scribecut","visible":true,
                 "rect":{"x":0,"y":0,"width":1,"height":1},"window_rect":{"x":0,"y":0,"width":1,"height":1},
                 "nodes":[],"floating_nodes":[]}],"floating_nodes":[]}]}"#,
        )
        .unwrap();
        let mut found = Vec::new();
        collect_windows(&tree, &mut found);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_browser());
        assert_eq!(found[0].area, Area { x: 12, y: 45, w: 496, h: 373 });
    }
}
