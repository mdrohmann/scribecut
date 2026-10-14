//! Top-level screens: home, recording, finishing, editing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use egui::{Color32, RichText};

use crate::capture::{Recorder, Source, connect, default_project_name, new_recording_dir};
use crate::settings::Settings;
use crate::editor::{Action, Editor, new_project};
use crate::geom::Output;
use crate::platform::{self, RegionPicker};
use crate::region;

/// What to do once the user has chosen a display.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WithDisplay {
    /// Record all of it.
    Record,
    /// Drag out a region on it.
    PickRegion,
}

enum Screen {
    Home,
    /// Choosing which display to use, where the system does not ask.
    ChooseDisplay { outputs: Vec<Output>, then: WithDisplay },
    /// Choosing which window to record.
    ChooseWindow { windows: Vec<platform::Window>, filter: String },
    /// Dragging out a region in scribecut's own overlay.
    PickRegion { display: Output, picker: region::Picker },
    /// Picking what to record and starting it; `None` means cancelled.
    Picking(mpsc::Receiver<Result<Option<Recorder>>>),
    Recording(Recorder),
    /// Joining segments after Stop.
    Finishing(mpsc::Receiver<Result<crate::project::Project>>),
    Editor(Box<Editor>),
}

pub struct App {
    screen: Screen,
    error: Option<String>,
    /// The system can share single windows itself (the portal on some desktops).
    system_windows: bool,
    settings: Settings,
    /// Folder name for the next recording, asked for on the home screen.
    project_name: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>) -> Self {
        install_overlay_font(&cc.egui_ctx);
        let mut app = Self {
            screen: Screen::Home,
            error: None,
            system_windows: crate::capture::supports_system_windows(),
            settings: Settings::load(),
            project_name: default_project_name(),
        };
        if let Some(path) = open {
            app.open(&path, &cc.egui_ctx);
        }
        app
    }

    /// Shows an error in the UI and also logs it, in case the window is hidden.
    fn report(&mut self, msg: String) {
        eprintln!("scribecut: {msg}");
        self.error = Some(msg);
    }

    fn open(&mut self, path: &std::path::Path, ctx: &egui::Context) {
        match Editor::open(path, ctx) {
            Ok(editor) => self.screen = Screen::Editor(Box::new(editor)),
            Err(e) => self.error = Some(format!("Could not open {}: {e:#}", path.display())),
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.heading(RichText::new("scribecut").size(32.0));
            ui.label("Record your screen, then edit by transcript.");
            ui.add_space(24.0);
            ui.label("Project name");
            let name = ui.add_sized([240.0, 28.0], egui::TextEdit::singleline(&mut self.project_name));
            name.on_hover_text("The recording's folder in Videos/scribecut");
            ui.add_space(12.0);
            let big =
                |text: &str| egui::Button::new(RichText::new(text).color(Color32::from_rgb(235, 90, 90)).size(18.0));
            if ui.add_sized([240.0, 40.0], big("Record full screen")).clicked() {
                self.record_screen(ui.ctx());
            }
            ui.add_space(4.0);
            let can_window = platform::available() || self.system_windows;
            let window = ui
                .add_enabled_ui(can_window, |ui| ui.add_sized([240.0, 40.0], big("Record a window…")))
                .inner
                .on_disabled_hover_text("This desktop cannot list or share single windows");
            if window.clicked() {
                self.record_window(ui.ctx());
            }
            ui.add_space(4.0);
            let can_region = platform::region_picker() != RegionPicker::None;
            let region = ui
                .add_enabled_ui(can_region, |ui| ui.add_sized([240.0, 40.0], big("Record a region…")))
                .inner
                .on_hover_text("Drag out a rectangle on the screen")
                .on_disabled_hover_text("Region recording is not supported on this desktop");
            if region.clicked() {
                self.record_region(ui.ctx());
            }
            ui.add_space(8.0);
            ui.add_space(4.0);
            let before = self.settings.clone();
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 300.0).max(0.0) / 2.0);
                let has_camera = self.settings.camera_available();
                ui.add_enabled(has_camera, egui::Checkbox::new(&mut self.settings.record_camera, "Record camera"))
                    .on_hover_text("Records you as a second video track; choose per part of the video whether to show it")
                    .on_disabled_hover_text(format!("No camera at {}", self.settings.camera_device));
                // Only sway can be told to enlarge the pointer; elsewhere the box would do nothing.
                if platform::CAN_SET_POINTER_SIZE && platform::available() {
                    ui.checkbox(&mut self.settings.big_pointer, "Bigger pointer")
                        .on_hover_text("Doubles the mouse pointer size while recording");
                }
            });
            if self.settings != before {
                self.settings.save();
            }
            ui.add_space(8.0);
            if ui.add_sized([240.0, 32.0], egui::Button::new("Open video or project…")).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Video or project", &["mkv", "mp4", "webm", "mov", "toml", "json"])
                    .pick_file()
            {
                self.open(&path, ui.ctx());
            }
        });
    }

    /// Records a whole display, asking which one if the system will not.
    fn record_screen(&mut self, ctx: &egui::Context) {
        if crate::capture::SYSTEM_MONITOR_PICKER {
            self.start(ctx, || Ok(Some(Source::Monitor(None))));
            return;
        }
        match self.choose_display(ctx, WithDisplay::Record) {
            Ok(screen) => self.screen = screen,
            Err(e) => self.report(format!("Could not list displays: {e:#}")),
        }
    }

    fn record_window(&mut self, ctx: &egui::Context) {
        // Real window capture follows the window across desktops and past
        // anything covering it, so it wins wherever the system offers it (sway
        // >= 1.12, GNOME, KDE); otherwise list the windows and crop the screen.
        if !self.system_windows && platform::available() {
            match platform::windows() {
                Ok(windows) => self.screen = Screen::ChooseWindow { windows, filter: String::new() },
                Err(e) => self.report(format!("Could not list windows: {e:#}")),
            }
        } else {
            self.start(ctx, || Ok(Some(Source::SystemWindow)));
        }
    }

    fn record_region(&mut self, ctx: &egui::Context) {
        match platform::region_picker() {
            // An external helper (sway's slurp) blocks until the user is done.
            RegionPicker::External => self.start(ctx, || Ok(platform::pick_region()?.map(Source::Area))),
            RegionPicker::InApp => match self.choose_display(ctx, WithDisplay::PickRegion) {
                Ok(screen) => self.screen = screen,
                Err(e) => self.report(format!("Could not list displays: {e:#}")),
            },
            RegionPicker::None => {}
        }
    }

    /// The display chooser, skipped when there is only one display.
    fn choose_display(&mut self, ctx: &egui::Context, then: WithDisplay) -> Result<Screen> {
        let outputs = platform::outputs()?;
        Ok(match (outputs.len(), then) {
            (1, WithDisplay::Record) => {
                let output = outputs.into_iter().next().unwrap();
                return Ok(self.picking(ctx, || Ok(Some(Source::Monitor(Some(output))))));
            }
            (1, WithDisplay::PickRegion) => {
                Screen::PickRegion { display: outputs.into_iter().next().unwrap(), picker: region::Picker::default() }
            }
            _ => Screen::ChooseDisplay { outputs, then },
        })
    }

    /// Picks what to record (off the UI thread: the portal and `slurp` block),
    /// then connects to the screen stream.
    fn start(&mut self, ctx: &egui::Context, pick: impl FnOnce() -> Result<Option<Source>> + Send + 'static) {
        self.screen = self.picking(ctx, pick);
    }

    /// As [`Self::start`], but hands the new screen back instead of assigning it.
    fn picking(
        &mut self,
        ctx: &egui::Context,
        pick: impl FnOnce() -> Result<Option<Source>> + Send + 'static,
    ) -> Screen {
        let settings = self.settings.clone();
        let name = self.project_name.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            // Recording starts here rather than on the UI thread: picking a window
            // on another workspace hides scribecut, and a hidden window gets no
            // UI updates until it is shown again.
            let result = pick().and_then(|source| source.map(|s| start_recording(s, &name, &settings)).transpose());
            let _ = tx.send(result);
            // `logic` picks the result up, even while the window is hidden.
            ctx.request_repaint();
        });
        self.error = None;
        Screen::Picking(rx)
    }

    /// The display list. Returns true to go back home.
    fn choose_display_ui(&mut self, ui: &mut egui::Ui, outputs: &[Output], then: WithDisplay) -> bool {
        let mut back = false;
        let mut chosen = None;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading(match then {
                WithDisplay::Record => "Which display?",
                WithDisplay::PickRegion => "Which display is the region on?",
            });
            ui.add_space(12.0);
            for output in outputs {
                let label = format!("{}   {}×{}", output.name, output.pixels.0, output.pixels.1);
                if ui.add_sized([320.0, 36.0], egui::Button::new(label)).clicked() {
                    chosen = Some(output.clone());
                }
                ui.add_space(4.0);
            }
            ui.add_space(8.0);
            if ui.button("Cancel").clicked() {
                back = true;
            }
        });
        if let Some(display) = chosen {
            self.screen = match then {
                WithDisplay::Record => {
                    self.picking(ui.ctx(), move || Ok(Some(Source::Monitor(Some(display)))))
                }
                WithDisplay::PickRegion => Screen::PickRegion { display, picker: region::Picker::default() },
            };
            return false;
        }
        back
    }

    /// The window list. Returns true to go back home.
    fn choose_window(&mut self, ui: &mut egui::Ui, windows: &[platform::Window], filter: &mut String) -> bool {
        let mut back = false;
        let mut chosen = None;
        let mut click = false;
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.heading("Record a window");
            ui.weak(
                "The recording is cut out of the screen: keep the window in place and uncovered while recording.",
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 520.0).max(0.0) / 2.0);
                ui.add(egui::TextEdit::singleline(filter).hint_text("Filter by title or app").desired_width(300.0));
                if platform::CAN_CLICK_WINDOW && ui.button("Click a window on screen…").clicked() {
                    click = true;
                }
                if ui.button("Cancel").clicked() {
                    back = true;
                }
            });
            ui.add_space(8.0);
        });
        let needle = filter.to_lowercase();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.vertical_centered(|ui| {
                for w in windows.iter().filter(|w| {
                    needle.is_empty()
                        || w.title.to_lowercase().contains(&needle)
                        || w.app_id.to_lowercase().contains(&needle)
                }) {
                    let mut text = egui::text::LayoutJob::default();
                    let body = ui.style().text_styles[&egui::TextStyle::Body].clone();
                    let strong = ui.visuals().strong_text_color();
                    let weak = ui.visuals().weak_text_color();
                    let fmt = |c| egui::TextFormat { font_id: body.clone(), color: c, ..Default::default() };
                    if w.is_browser() {
                        text.append("browser  ", 0.0, fmt(Color32::from_rgb(70, 170, 200)));
                    }
                    text.append(&w.title, 0.0, fmt(strong));
                    text.append(&format!("   {}", w.app_id), 0.0, fmt(weak));
                    if !w.visible {
                        text.append("   (other workspace)", 0.0, fmt(weak));
                    }
                    if ui.add_sized([640.0, 30.0], egui::Button::new(text).truncate()).clicked() {
                        chosen = Some((w.id, w.pid));
                    }
                }
            });
        });
        if let Some((id, pid)) = chosen {
            self.start(ui.ctx(), move || Ok(Some(Source::Window { id, pid })));
        } else if click {
            let windows = windows.to_vec();
            self.start(ui.ctx(), move || {
                // The picker reports which window was clicked; its process is
                // what brings the application forward.
                Ok(platform::click_window(&windows)?.and_then(|id| {
                    windows.iter().find(|w| w.id == id).map(|w| Source::Window { id, pid: w.pid })
                }))
            });
        }
        back
    }

    fn recording(ui: &mut egui::Ui, rec: &mut Recorder) -> Result<bool> {
        let mut stop = false;
        let recording = rec.is_recording();
        ui.vertical_centered(|ui| -> Result<()> {
            ui.add_space(ui.available_height() * 0.3);
            let (label, color) = if recording {
                ("Recording", Color32::from_rgb(220, 50, 50))
            } else {
                ("Paused", Color32::from_rgb(230, 180, 40))
            };
            ui.label(RichText::new(label).size(22.0).color(color));
            let secs = rec.elapsed().as_secs();
            ui.label(
                RichText::new(format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60))
                    .size(48.0)
                    .monospace(),
            );
            ui.label(format!("{} piece(s) · {}", rec.pieces(), rec.dir().display()));
            if rec.has_camera() {
                ui.label("Camera: recording");
            } else if let Some(warning) = rec.warning() {
                ui.colored_label(Color32::from_rgb(230, 180, 40), warning);
            } else {
                ui.weak("Camera: off (turn on \"Record camera\" on the home screen)");
            }
            if rec.is_waiting_for_window() {
                ui.colored_label(
                    Color32::from_rgb(230, 180, 40),
                    "Paused while the recorded window is hidden - switch to it to continue",
                );
            }
            ui.add_space(20.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 330.0).max(0.0) / 2.0);
                let toggle = if recording { "Pause" } else { "Resume" };
                if ui.add_sized([160.0, 40.0], egui::Button::new(RichText::new(toggle).size(18.0))).clicked() {
                    if recording { rec.stop_segment() } else { rec.start_segment() }?;
                }
                if ui.add_sized([160.0, 40.0], egui::Button::new(RichText::new("Stop & edit").size(18.0))).clicked() {
                    stop = true;
                }
                Ok::<_, anyhow::Error>(())
            })
            .inner
        })
        .inner?;
        if recording {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        if let Some(err) = rec.poll_error() {
            anyhow::bail!("Recording stopped: {err}");
        }
        Ok(stop)
    }
}

impl eframe::App for App {
    /// A fully transparent base, so the region overlay really is see-through.
    /// The main window's panels paint their own opaque fill over it.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The region overlay is a window of its own, so it is driven from here
        // rather than from `ui`, which runs inside the main window's layout.
        let picked = match &mut self.screen {
            Screen::PickRegion { display, picker } => Some(picker.show(ctx, display)),
            _ => None,
        };
        if let Some(picked) = picked {
            match picked {
                region::Picked::Pending => {}
                region::Picked::Cancelled => self.screen = Screen::Home,
                region::Picked::Done(area) => {
                    self.screen = self.picking(ctx, move || Ok(Some(Source::Area(area))));
                }
            }
            return;
        }
        let Screen::Picking(rx) = &self.screen else { return };
        let next = match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) | Ok(Ok(None)) => Screen::Home,
            Ok(Ok(Some(rec))) => Screen::Recording(rec),
            Ok(Err(e)) => {
                self.report(format!("Recording was not started: {e:#}"));
                Screen::Home
            }
        };
        self.screen = next;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if let Some(err) = self.error.clone() {
            egui::Panel::bottom("error").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(230, 90, 90), err);
                    if ui.small_button("×").clicked() {
                        self.error = None;
                    }
                });
            });
        }
        let screen = std::mem::replace(&mut self.screen, Screen::Home);
        self.screen = match screen {
            Screen::Editor(mut editor) => match editor.ui(ui) {
                Some(Action::Home) => Screen::Home,
                None => Screen::Editor(editor),
            },
            Screen::ChooseDisplay { outputs, then } => {
                let back = egui::CentralPanel::default()
                    .show(ui, |ui| self.choose_display_ui(ui, &outputs, then))
                    .inner;
                match std::mem::replace(&mut self.screen, Screen::Home) {
                    // `choose_display_ui` moved on.
                    Screen::Home if back => Screen::Home,
                    Screen::Home => Screen::ChooseDisplay { outputs, then },
                    next => next,
                }
            }
            Screen::ChooseWindow { windows, mut filter } => {
                let back =
                    egui::CentralPanel::default().show(ui, |ui| self.choose_window(ui, &windows, &mut filter)).inner;
                match std::mem::replace(&mut self.screen, Screen::Home) {
                    // `choose_window` started picking.
                    picking @ Screen::Picking(_) => picking,
                    _ if back => Screen::Home,
                    _ => Screen::ChooseWindow { windows, filter },
                }
            }
            // The overlay itself is driven by `logic`; this is just what the
            // main window says meanwhile.
            Screen::PickRegion { display, picker } => {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.label("Drag out the area to record on the screen…");
                    });
                });
                Screen::PickRegion { display, picker }
            }
            Screen::Picking(rx) => {
                // The result is handled in `logic`, which also runs while this
                // window is hidden (e.g. after switching to the window to record).
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space(ui.available_height() * 0.4);
                        ui.spinner();
                        ui.label("Choose what to record…");
                    });
                });
                Screen::Picking(rx)
            }
            Screen::Recording(mut rec) => {
                let result = egui::CentralPanel::default().show(ui, |ui| Self::recording(ui, &mut rec)).inner;
                match result {
                    Ok(false) => Screen::Recording(rec),
                    Ok(true) | Err(_) => {
                        if let Err(e) = result {
                            self.error = Some(format!("{e:#}"));
                        }
                        // The next recording is a new project.
                        self.project_name = default_project_name();
                        let (tx, rx) = mpsc::channel();
                        let ctx = ctx.clone();
                        std::thread::spawn(move || {
                            let project = rec.finish().and_then(|(file, starts)| new_project(&file, starts));
                            let _ = tx.send(project);
                            ctx.request_repaint();
                        });
                        Screen::Finishing(rx)
                    }
                }
            }
            Screen::Finishing(rx) => match rx.try_recv() {
                Ok(project) => match project.and_then(|p| Editor::new(p, &ctx)) {
                    Ok(editor) => Screen::Editor(Box::new(editor)),
                    Err(e) => {
                        self.error = Some(format!("Could not finish recording: {e:#}"));
                        Screen::Home
                    }
                },
                Err(mpsc::TryRecvError::Disconnected) => Screen::Home,
                Err(mpsc::TryRecvError::Empty) => {
                    egui::CentralPanel::default().show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.4);
                            ui.spinner();
                            ui.label("Joining recorded pieces…");
                        });
                    });
                    Screen::Finishing(rx)
                }
            },
            Screen::Home => {
                // `home` may switch screens by assigning `self.screen`.
                egui::CentralPanel::default().show(ui, |ui| self.home(ui));
                std::mem::replace(&mut self.screen, Screen::Home)
            }
        };
    }
}

/// Connects to the screen stream and starts the first segment.
fn start_recording(source: Source, name: &str, settings: &Settings) -> Result<Recorder> {
    let mut rec = Recorder::new(connect(source)?, new_recording_dir(name)?, settings)?;
    rec.start_segment()?;
    Ok(rec)
}

/// Loads the same font ffmpeg uses for overlays so the preview matches the export.
fn install_overlay_font(ctx: &egui::Context) {
    let Some(data) = crate::media::overlay_font().and_then(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts
        .font_data
        .insert(crate::editor::OVERLAY_FONT.into(), std::sync::Arc::new(egui::FontData::from_owned(data)));
    fonts.families.insert(
        egui::FontFamily::Name(crate::editor::OVERLAY_FONT.into()),
        vec![crate::editor::OVERLAY_FONT.into()],
    );
    ctx.set_fonts(fonts);
}
