//! Top-level screens: home, recording, finishing, editing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use egui::{Color32, RichText};

use crate::capture::{Portal, Recorder, Source, connect, new_recording_dir};
use crate::sway;
use crate::editor::{Action, Editor, new_project};

/// An open screen stream, plus the crop to apply to it.
type Connected = (Portal, Option<sway::Crop>);

enum Screen {
    Home,
    /// Choosing which window to record (sway).
    ChooseWindow { windows: Vec<sway::Window>, filter: String },
    /// Waiting for the user to pick what to share; `None` means cancelled.
    Picking(mpsc::Receiver<Result<Option<Connected>>>),
    Recording(Recorder),
    /// Joining segments after Stop.
    Finishing(mpsc::Receiver<Result<crate::project::Project>>),
    Editor(Box<Editor>),
}

pub struct App {
    screen: Screen,
    error: Option<String>,
    /// The portal can share single windows itself (not on sway 1.11).
    portal_windows: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>) -> Self {
        install_overlay_font(&cc.egui_ctx);
        let mut app = Self { screen: Screen::Home, error: None, portal_windows: crate::capture::portal_supports_windows() };
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
            let big = |text: &str| egui::Button::new(RichText::new(text).color(Color32::from_rgb(235, 90, 90)).size(18.0));
            if ui.add_sized([240.0, 40.0], big("Record full screen")).clicked() {
                self.start(ui.ctx(), || Ok(Some(Source::Monitor)));
            }
            ui.add_space(4.0);
            let can_window = sway::available() || self.portal_windows;
            let window = ui
                .add_enabled_ui(can_window, |ui| ui.add_sized([240.0, 40.0], big("Record a window…")))
                .inner
                .on_disabled_hover_text("Your desktop's screen-sharing portal can't share single windows");
            if window.clicked() {
                if sway::available() {
                    match sway::windows() {
                        Ok(windows) => self.screen = Screen::ChooseWindow { windows, filter: String::new() },
                        Err(e) => self.error = Some(format!("Could not list windows: {e:#}")),
                    }
                } else {
                    self.start(ui.ctx(), || Ok(Some(Source::PortalWindow)));
                }
            }
            ui.add_space(4.0);
            let region = ui
                .add_enabled_ui(sway::available(), |ui| ui.add_sized([240.0, 40.0], big("Record a region…")))
                .inner
                .on_hover_text("Drag out a rectangle on the screen")
                .on_disabled_hover_text("Region recording needs sway + slurp");
            if region.clicked() {
                self.start(ui.ctx(), || Ok(sway::pick_region()?.map(Source::Area)));
            }
            ui.add_space(8.0);
            if ui.add_sized([240.0, 32.0], egui::Button::new("Open video or project…")).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Video or project", &["mkv", "mp4", "webm", "mov", "json"])
                    .pick_file()
            {
                self.open(&path, ui.ctx());
            }
        });
    }

    /// Picks what to record (off the UI thread: slurp and the portal block),
    /// then connects to the screen stream.
    fn start(&mut self, ctx: &egui::Context, pick: impl FnOnce() -> Result<Option<Source>> + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = pick().and_then(|source| source.map(connect).transpose());
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.screen = Screen::Picking(rx);
        self.error = None;
    }

    /// The window list. Returns true to go back home.
    fn choose_window(&mut self, ui: &mut egui::Ui, windows: &[sway::Window], filter: &mut String) -> bool {
        let mut back = false;
        let mut chosen = None;
        let mut click = false;
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.heading("Record a window");
            ui.weak("The recording is cut out of the screen: keep the window in place and uncovered while recording.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.add_space((ui.available_width() - 520.0).max(0.0) / 2.0);
                ui.add(egui::TextEdit::singleline(filter).hint_text("Filter by title or app").desired_width(300.0));
                if ui.button("Click a window on screen…").clicked() {
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
                    needle.is_empty() || w.title.to_lowercase().contains(&needle) || w.app_id.to_lowercase().contains(&needle)
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
                        chosen = Some(w.con_id);
                    }
                }
            });
        });
        if let Some(id) = chosen {
            self.start(ui.ctx(), move || Ok(Some(Source::Area(sway::focus_window(id)?))));
        } else if click {
            let windows = windows.to_vec();
            self.start(ui.ctx(), move || Ok(sway::click_window(&windows)?.map(Source::Area)));
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
            ui.label(RichText::new(format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)).size(48.0).monospace());
            ui.label(format!("{} piece(s) · {}", rec.segments.len(), rec.dir.display()));
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
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let Screen::Picking(rx) = &self.screen else { return };
        let next = match rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) | Ok(Ok(None)) => Screen::Home,
            Ok(Ok(Some((portal, crop)))) => {
                let started = new_recording_dir().and_then(|dir| {
                    let mut rec = Recorder::new(portal, crop, dir)?;
                    rec.start_segment()?;
                    Ok(rec)
                });
                match started {
                    Ok(rec) => Screen::Recording(rec),
                    Err(e) => {
                        self.report(format!("Could not start recording: {e:#}"));
                        Screen::Home
                    }
                }
            }
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
            Screen::ChooseWindow { windows, mut filter } => {
                let back = egui::CentralPanel::default().show(ui, |ui| self.choose_window(ui, &windows, &mut filter)).inner;
                match std::mem::replace(&mut self.screen, Screen::Home) {
                    // `choose_window` started picking.
                    picking @ Screen::Picking(_) => picking,
                    _ if back => Screen::Home,
                    _ => Screen::ChooseWindow { windows, filter },
                }
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

/// Loads the same font ffmpeg uses for overlays so the preview matches the export.
fn install_overlay_font(ctx: &egui::Context) {
    let Some(data) = crate::media::overlay_font().and_then(|p| std::fs::read(p).ok()) else { return };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        crate::editor::OVERLAY_FONT.into(),
        std::sync::Arc::new(egui::FontData::from_owned(data)),
    );
    fonts.families.insert(
        egui::FontFamily::Name(crate::editor::OVERLAY_FONT.into()),
        vec![crate::editor::OVERLAY_FONT.into()],
    );
    ctx.set_fonts(fonts);
}
