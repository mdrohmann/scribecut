//! Dragging out a screen region in scribecut's own full-screen overlay, for
//! desktops with no external picker like sway's `slurp`.
//!
//! The overlay is a borderless, transparent window placed exactly over one
//! display, so a pointer position inside it is a desktop coordinate once the
//! display's origin is added. egui works in logical points, which is also what
//! [`Output::area`] is measured in, so no scaling is involved.

use egui::{Color32, Key, Pos2, Rect, Stroke, StrokeKind, pos2};

use crate::geom::{Area, Output};

/// How the drag ended.
pub enum Picked {
    /// Still dragging, or not started yet.
    Pending,
    Cancelled,
    Done(Area),
}

/// Smallest region worth recording, in points.
const MIN_SIZE: f32 = 16.0;

const ACCENT: Color32 = Color32::from_rgb(235, 90, 90);

#[derive(Default)]
pub struct Picker {
    /// Where the drag began, in overlay-local points.
    start: Option<Pos2>,
    /// Where the pointer is now.
    end: Option<Pos2>,
}

impl Picker {
    /// Shows the overlay over `display` for one frame and reports the result.
    ///
    /// Call this from `eframe::App::logic`, not from the main window's `ui`:
    /// an immediate viewport runs its own layout pass and must not be opened
    /// from inside another window's.
    pub fn show(&mut self, ctx: &egui::Context, display: &Output) -> Picked {
        let area = display.area;
        let builder = egui::ViewportBuilder::default()
            .with_title("Select a region to record")
            .with_position([area.x as f32, area.y as f32])
            .with_inner_size([area.w as f32, area.h as f32])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false);
        let viewport = egui::ViewportId::from_hash_of("scribecut-region");
        ctx.show_viewport_immediate(viewport, builder, |ctx, _class| self.overlay(ctx, area))
    }

    fn overlay(&mut self, ctx: &egui::Context, area: Area) -> Picked {
        // A borderless window has no close button, so Esc is the way out --
        // and the window manager's own close request still has to be honoured.
        if ctx.input(|i| i.key_pressed(Key::Escape) || i.viewport().close_requested()) {
            return Picked::Cancelled;
        }
        ctx.set_cursor_icon(egui::CursorIcon::Crosshair);

        // Painted straight onto the viewport's background layer: an overlay
        // has no widgets, and a panel would need a `Ui` this closure has not got.
        let full = ctx.content_rect();
        let (pressed, released, pointer) = ctx.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.interact_pos().or_else(|| i.pointer.latest_pos()),
            )
        });
        if pressed {
            self.start = pointer;
            self.end = pointer;
        } else if self.start.is_some() && pointer.is_some() {
            self.end = pointer;
        }

        let painter = ctx.layer_painter(egui::LayerId::background());
        let dim = Color32::from_black_alpha(110);
        match self.rect().map(|r| r.intersect(full)) {
            Some(sel) => {
                // Leave the selection clear so the user sees exactly what will
                // be recorded. Painting cannot subtract, so the dimming goes
                // on as the four bands around it.
                for band in [
                    Rect::from_min_max(full.left_top(), pos2(full.right(), sel.top())),
                    Rect::from_min_max(pos2(full.left(), sel.bottom()), full.right_bottom()),
                    Rect::from_min_max(pos2(full.left(), sel.top()), pos2(sel.left(), sel.bottom())),
                    Rect::from_min_max(pos2(sel.right(), sel.top()), pos2(full.right(), sel.bottom())),
                ] {
                    painter.rect_filled(band, 0.0, dim);
                }
                painter.rect_stroke(sel, 0.0, Stroke::new(1.5, ACCENT), StrokeKind::Outside);
                painter.text(
                    sel.left_top() - egui::vec2(0.0, 6.0),
                    egui::Align2::LEFT_BOTTOM,
                    format!("{} × {}", sel.width().round(), sel.height().round()),
                    egui::FontId::monospace(13.0),
                    Color32::WHITE,
                );
            }
            None => {
                painter.rect_filled(full, 0.0, dim);
                painter.text(
                    full.center(),
                    egui::Align2::CENTER_CENTER,
                    "Drag out the area to record  ·  Esc to cancel",
                    egui::FontId::proportional(18.0),
                    Color32::from_white_alpha(220),
                );
            }
        }

        if released && self.start.is_some() {
            return match self.rect() {
                // A stray click or a sliver means "never mind".
                Some(r) if r.width() >= MIN_SIZE && r.height() >= MIN_SIZE => Picked::Done(Area {
                    x: area.x + r.left().round() as i32,
                    y: area.y + r.top().round() as i32,
                    w: r.width().round() as i32,
                    h: r.height().round() as i32,
                }),
                _ => Picked::Cancelled,
            };
        }
        Picked::Pending
    }

    /// The selection so far, normalised so dragging in any direction works.
    fn rect(&self) -> Option<Rect> {
        Some(Rect::from_two_pos(self.start?, self.end?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drag is normalised whichever way it was drawn.
    #[test]
    fn normalises_the_drag() {
        let mut p = Picker { start: Some(pos2(300.0, 220.0)), end: Some(pos2(100.0, 20.0)) };
        let r = p.rect().unwrap();
        assert_eq!((r.left(), r.top(), r.width(), r.height()), (100.0, 20.0, 200.0, 200.0));

        p.start = None;
        assert!(p.rect().is_none());
    }
}
