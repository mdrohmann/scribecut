//! Top-level screens: home, recording, finishing, editing.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use egui::{Color32, RichText};

use crate::capture::{Portal, Recorder, new_recording_dir};
use crate::editor::{Action, Editor, new_project};

enum Screen {
    Home,
    /// Waiting for the user to pick a screen in the portal dialog.
    Picking(mpsc::Receiver<Result<Portal>>),
    Recording(Recorder),
    /// Joining segments after Stop.
    Finishing(mpsc::Receiver<Result<crate::project::Project>>),
    Editor(Box<Editor>),
}

pub struct App {
    screen: Screen,
    error: Option<String>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, open: Option<PathBuf>) -> Self {
        install_overlay_font(&cc.egui_ctx);
        let mut app = Self { screen: Screen::Home, error: None };
        if let Some(path) = open {
            app.open(&path, &cc.egui_ctx);
        }
        app
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
            if ui.add_sized([220.0, 40.0], egui::Button::new(RichText::new("Record screen").color(Color32::from_rgb(235, 90, 90)).size(18.0))).clicked() {
                let (tx, rx) = mpsc::channel();
                let ctx = ui.ctx().clone();
                std::thread::spawn(move || {
                    let _ = tx.send(Portal::open());
                    ctx.request_repaint();
                });
                self.screen = Screen::Picking(rx);
                self.error = None;
            }
            ui.add_space(8.0);
            if ui.add_sized([220.0, 32.0], egui::Button::new("Open video or project…")).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Video or project", &["mkv", "mp4", "webm", "mov", "json"])
                    .pick_file()
            {
                self.open(&path, ui.ctx());
            }
        });
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
            Screen::Picking(rx) => match rx.try_recv() {
                Ok(Ok(portal)) => {
                    let started = new_recording_dir().and_then(|dir| {
                        let mut rec = Recorder::new(portal, dir)?;
                        rec.start_segment()?;
                        Ok(rec)
                    });
                    match started {
                        Ok(rec) => Screen::Recording(rec),
                        Err(e) => {
                            self.error = Some(format!("Could not start recording: {e:#}"));
                            Screen::Home
                        }
                    }
                }
                Ok(Err(e)) => {
                    self.error = Some(format!("Screen sharing was not started: {e:#}"));
                    Screen::Home
                }
                Err(mpsc::TryRecvError::Disconnected) => Screen::Home,
                Err(mpsc::TryRecvError::Empty) => {
                    egui::CentralPanel::default().show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.4);
                            ui.spinner();
                            ui.label("Choose a screen or window in the sharing dialog…");
                        });
                    });
                    Screen::Picking(rx)
                }
            },
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
