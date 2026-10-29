//! Screen geometry shared by the platform backends: rectangles in the
//! desktop's logical coordinate space, monitors, and the crop that turns a
//! monitor's frame into one of those rectangles.

/// A rectangle in the desktop's global layout coordinates (logical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Area {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Area {
    pub fn contains_center_of(&self, o: &Area) -> bool {
        let (cx, cy) = (o.x + o.w / 2, o.y + o.h / 2);
        cx >= self.x && cx < self.x + self.w && cy >= self.y && cy < self.y + self.h
    }
}

/// An application window that can be recorded.
#[derive(Clone, Debug)]
pub struct Window {
    /// Backend-specific handle (sway container id, macOS window number).
    pub id: i64,
    /// Owning process, used to bring the window to the front on macOS.
    pub pid: i32,
    pub title: String,
    pub app_id: String,
    pub area: Area,
    pub visible: bool,
    /// The compositor's handle for sharing just this window through the
    /// portal (sway >= 1.12), if it has one.
    pub toplevel: Option<String>,
}

impl Window {
    pub fn is_browser(&self) -> bool {
        const BROWSERS: [&str; 10] =
            ["firefox", "chrom", "safari", "brave", "vivaldi", "opera", "epiphany", "librewolf", "zen", "edge"];
        let id = self.app_id.to_lowercase();
        BROWSERS.iter().any(|b| id.contains(b))
    }
}

/// A monitor.
#[derive(Clone, Debug)]
pub struct Output {
    pub name: String,
    /// Where the monitor sits in the layout, in logical pixels.
    pub area: Area,
    /// Physical pixel size of the monitor's current mode.
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

impl Crop {
    pub const NONE: Crop = Crop { left: 0, top: 0, right: 0, bottom: 0 };

    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// The cropped size, given the full frame size it applies to.
    ///
    /// Only backends that have to state an output size need this: GStreamer's
    /// `videocrop` is told the edges instead, so the Linux build never calls it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn size(&self, frame: (i32, i32)) -> (i32, i32) {
        ((frame.0 - self.left - self.right).max(2), (frame.1 - self.top - self.bottom).max(2))
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
        assert_eq!(c, Crop::NONE);
    }

    #[test]
    fn crop_size_subtracts_both_edges() {
        let c = Crop { left: 10, top: 20, right: 30, bottom: 40 };
        assert_eq!(c.size((1000, 500)), (960, 440));
        // Never degenerate, even if the crop swallows the frame.
        assert_eq!(Crop { left: 900, top: 0, right: 900, bottom: 0 }.size((1000, 500)), (2, 500));
    }

    #[test]
    fn finds_output_by_center() {
        let outs = [output(0, 1.0), output(1000, 1.0)];
        let picked = output_for(&Area { x: 900, y: 10, w: 400, h: 10 }, &outs).unwrap();
        assert_eq!(picked.area.x, 1000);
    }
}
