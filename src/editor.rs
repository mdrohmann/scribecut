//! The editing screen: video preview, word-linked transcript, timeline with
//! cuts, and text overlays.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result};
use egui::{Align, Align2, Color32, FontFamily, FontId, Key, Pos2, Rect, RichText, Sense, Stroke, Vec2, pos2, vec2};

use crate::player::{Player, preview_size};
use crate::project::{CameraMode, Corner, Overlay, Project, Range, Slide, Word};
use crate::transcribe;

pub const OVERLAY_FONT: &str = "overlay";

const SELECTION: Color32 = Color32::from_rgba_premultiplied(40, 90, 160, 90);
const CUT: Color32 = Color32::from_rgba_premultiplied(150, 30, 30, 150);
const CURRENT_WORD: Color32 = Color32::from_rgb(120, 95, 10);
const SELECTED_WORD: Color32 = Color32::from_rgb(35, 70, 125);
const SEGMENT: Color32 = Color32::from_rgb(230, 180, 40);
const OVERLAY_BAR: Color32 = Color32::from_rgb(60, 140, 90);
const WAVE: Color32 = Color32::from_rgb(70, 170, 200);
const SLIDE_BAR: Color32 = Color32::from_rgb(120, 80, 170);
const CAMERA_FULL: Color32 = Color32::from_rgb(220, 120, 40);
const CAMERA_PIP: Color32 = Color32::from_rgb(170, 110, 60);

/// A background task reporting progress and a final result.
struct Job<T> {
    rx: mpsc::Receiver<JobMsg<T>>,
    label: String,
    progress: f32,
}

enum JobMsg<T> {
    Progress(String, f32),
    Done(Result<T>),
}

impl<T: Send + 'static> Job<T> {
    fn spawn(
        ctx: &egui::Context,
        label: &str,
        work: impl FnOnce(Box<dyn Fn(String, f32) + Send>) -> Result<T> + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let progress_ctx = ctx.clone();
            let report = Box::new(move |label: String, p: f32| {
                let _ = progress_tx.send(JobMsg::Progress(label, p));
                progress_ctx.request_repaint();
            });
            let _ = tx.send(JobMsg::Done(work(report)));
            ctx.request_repaint();
        });
        Self { rx, label: label.to_string(), progress: 0.0 }
    }

    /// Drains progress messages; returns the result once finished.
    fn poll(&mut self) -> Option<Result<T>> {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                JobMsg::Progress(label, p) => (self.label, self.progress) = (label, p),
                JobMsg::Done(r) => return Some(r),
            }
        }
        None
    }

    fn ui(&self, ui: &mut egui::Ui) {
        ui.add(
            egui::ProgressBar::new(self.progress)
                .desired_width(160.0)
                .text(format!("{} {:.0}%", self.label, self.progress * 100.0)),
        );
    }
}

/// Identifies a slide picture: resolved file, page and clip position.
type SlideKey = (PathBuf, u32, u64);

enum SlideImage {
    Loading(Job<egui::ColorImage>),
    Ready(egui::TextureHandle),
    Failed(String),
}

/// A slide being created or edited in the dialog.
struct SlideDraft {
    index: Option<usize>,
    slide: Slide,
}

/// Loads an image file for display, at most 1600 pixels wide.
fn load_image(path: &Path) -> Result<egui::ColorImage> {
    let img = image::open(path).with_context(|| format!("reading {}", path.display()))?;
    let img = if img.width() > 1600 { img.resize(1600, 1600 * img.height() / img.width(), image::imageops::FilterType::Triangle) } else { img };
    let rgba = img.to_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// The largest rect with `aspect` (w/h) centred in `outer`.
fn fit_rect(outer: Rect, aspect: f32) -> Rect {
    let mut size = vec2(outer.width(), outer.width() / aspect);
    if size.y > outer.height() {
        size = vec2(outer.height() * aspect, outer.height());
    }
    Rect::from_center_size(outer.center(), size)
}

/// An overlay being created or edited in the dialog.
struct Draft {
    index: Option<usize>,
    overlay: Overlay,
}

pub enum Action {
    Home,
}

pub struct Editor {
    pub project: Project,
    player: Player,
    texture: Option<egui::TextureHandle>,
    pos: f64,
    selection: Option<Range>,
    anchor_word: Option<usize>,
    /// Visible timeline window: start and length in seconds.
    view: (f64, f64),
    drag_from: Option<f64>,
    draft: Option<Draft>,
    placing: bool,
    slide_draft: Option<SlideDraft>,
    /// Earlier states of the edits (words excluded) for undo.
    undo: Vec<Project>,
    /// Camera track player, kept in step with `player`.
    camera: Option<Player>,
    camera_texture: Option<egui::TextureHandle>,
    /// Slide pictures for the preview, by slide file/page.
    slide_images: HashMap<SlideKey, SlideImage>,
    /// Modification time of the edit file as we last wrote or read it.
    edit_mtime: Option<SystemTime>,
    last_reload_check: Instant,
    transcription: Option<Job<Vec<Word>>>,
    /// Loudness envelope (see `media::audio_peaks`), empty until loaded.
    waveform: Vec<f32>,
    /// Background-noise level in dB; the waveform is drawn from here up to 0 dB.
    waveform_floor_db: f32,
    waveform_job: Option<Job<Vec<f32>>>,
    /// Renders the cleaned-up narration; playback switches to it when done.
    clean_audio_job: Option<Job<Option<PathBuf>>>,
    ctx: egui::Context,
    export: Option<Job<PathBuf>>,
    message: Option<String>,
    last_scrolled_word: Option<usize>,
}

impl Editor {
    /// Opens a video file (with its edit file, if any) or an edit file.
    pub fn open(path: &Path, ctx: &egui::Context) -> Result<Self> {
        let is_video = !path.extension().is_some_and(|e| e == "toml" || e == "json");
        let project = if is_video && !Project::path_for(path).is_file() && !path.with_extension("scribecut.json").is_file() {
            new_project(path, Vec::new())?
        } else {
            Project::load(path)?
        };
        Self::new(project, ctx)
    }

    pub fn new(project: Project, ctx: &egui::Context) -> Result<Self> {
        let preview = preview_size(project.width, project.height, 1600);
        let player = Player::new(&project.media, preview, ctx.clone())
            .with_context(|| format!("opening {}", project.media.display()))?;
        let duration = project.duration.max(0.1);
        let camera = match project.camera_size {
            Some((w, h)) => Player::silent_stream(&project.media, 1, preview_size(w, h, 640), ctx.clone()).ok(),
            None => None,
        };
        let mut editor = Self {
            project,
            player,
            texture: None,
            pos: 0.0,
            selection: None,
            anchor_word: None,
            view: (0.0, duration),
            drag_from: None,
            draft: None,
            placing: false,
            slide_draft: None,
            undo: Vec::new(),
            camera,
            camera_texture: None,
            slide_images: HashMap::new(),
            edit_mtime: None,
            last_reload_check: Instant::now(),
            transcription: None,
            waveform: Vec::new(),
            waveform_floor_db: -50.0,
            waveform_job: None,
            clean_audio_job: None,
            ctx: ctx.clone(),
            export: None,
            message: None,
            last_scrolled_word: None,
        };
        if editor.project.has_audio {
            let media = editor.project.media.clone();
            editor.waveform_job = Some(Job::spawn(ctx, "Waveform", move |_| crate::media::audio_peaks(&media)));
            let media = editor.project.media.clone();
            editor.clean_audio_job = Some(Job::spawn(ctx, "Cleaning audio", move |_| crate::audio::render_clean(&media)));
            if editor.project.words.is_empty() {
                editor.start_transcription(ctx);
            }
        }
        editor.save();
        Ok(editor)
    }

    fn start_transcription(&mut self, ctx: &egui::Context) {
        let media = self.project.media.clone();
        self.transcription = Some(Job::spawn(ctx, "Transcribing", move |report| {
            transcribe::transcribe(&media, move |s| match s {
                transcribe::Status::Downloading(p) => report("Downloading model".into(), p),
                transcribe::Status::Transcribing(p) => report("Transcribing".into(), p),
            })
        }));
    }

    fn edit_path(&self) -> PathBuf {
        Project::path_for(&self.project.media)
    }

    fn save(&mut self) {
        match self.project.save() {
            Ok(()) => self.edit_mtime = std::fs::metadata(self.edit_path()).and_then(|m| m.modified()).ok(),
            Err(e) => self.message = Some(format!("Saving failed: {e:#}")),
        }
    }

    /// Picks up changes to the edit file made outside the app (by hand or an agent).
    fn reload_if_changed(&mut self) {
        if self.last_reload_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_reload_check = Instant::now();
        let path = self.edit_path();
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return };
        if Some(mtime) == self.edit_mtime {
            return;
        }
        self.edit_mtime = Some(mtime);
        match Project::load(&path) {
            Ok(mut fresh) => {
                fresh.words = std::mem::take(&mut self.project.words);
                self.undo.push(self.snapshot());
                self.project = fresh;
                self.draft = None;
                self.slide_draft = None;
                self.placing = false;
                self.message = Some(format!("{} changed - reloaded", path.file_name().unwrap_or_default().to_string_lossy()));
            }
            Err(e) => self.message = Some(format!("Not reloaded, {e:#}")),
        }
    }

    /// The edits without the (large, unchanging) word list.
    fn snapshot(&self) -> Project {
        Project { words: Vec::new(), ..self.project.clone() }
    }

    /// Records the current edits so they can be undone, then applies `f`.
    fn edit(&mut self, f: impl FnOnce(&mut Project)) {
        self.undo.push(self.snapshot());
        f(&mut self.project);
        self.save();
    }

    fn undo(&mut self) {
        if let Some(mut previous) = self.undo.pop() {
            previous.words = std::mem::take(&mut self.project.words);
            self.project = previous;
            self.draft = None;
            self.slide_draft = None;
            self.placing = false;
            self.save();
        }
    }

    fn seek(&mut self, t: f64) {
        let t = t.clamp(0.0, self.project.duration);
        self.player.seek(t);
        if let Some(cam) = &mut self.camera {
            cam.seek(t);
        }
        self.pos = t;
    }

    fn cut_selection(&mut self) {
        if let Some(sel) = self.selection.take() {
            self.edit(|p| p.add_cut(sel));
        }
    }

    fn new_overlay_draft(&mut self) {
        let range = self
            .selection
            .unwrap_or_else(|| Range::new(self.pos, (self.pos + 3.0).min(self.project.duration)));
        self.draft = Some(Draft {
            index: None,
            overlay: Overlay {
                start: range.start,
                end: range.end,
                text: String::new(),
                x: 0.5,
                y: 0.85,
                size: 0.06,
                color: [255, 255, 255],
            },
        });
        self.placing = false;
    }

    fn export_script(&mut self) {
        let media = &self.project.media;
        let stem = media.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
        let mut dialog = rfd::FileDialog::new()
            .set_file_name(format!("{stem}_script.md"))
            .add_filter("Markdown", &["md"]);
        if let Some(dir) = media.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(out) = dialog.save_file() else { return };
        self.message = Some(match std::fs::write(&out, crate::script::script(&self.project)) {
            Ok(()) => format!("Script saved to {}", out.display()),
            Err(e) => format!("Saving script failed: {e}"),
        });
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        let media = &self.project.media;
        let stem = media.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
        let mut dialog = rfd::FileDialog::new()
            .set_file_name(format!("{stem}_edited.mp4"))
            .add_filter("MP4 video", &["mp4"]);
        if let Some(dir) = media.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(out) = dialog.save_file() else { return };
        let project = self.project.clone();
        self.export = Some(Job::spawn(ctx, "Exporting", move |report| {
            crate::export::export(&project, &out, |p| report("Exporting".into(), p))?;
            Ok(out)
        }));
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let ctx = ui.ctx().clone();
        self.reload_if_changed();
        self.poll_jobs();
        if let Some(err) = self.player.poll() {
            self.message = Some(format!("Playback error: {err}"));
        }
        self.pos = self.player.position();
        self.skip_cuts();
        self.sync_camera();
        if self.player.playing {
            self.follow_playhead();
            ctx.request_repaint();
        } else {
            // Keep checking the edit file for outside changes.
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.upload_frame(&ctx);
        self.upload_camera_frame(&ctx);
        self.keyboard(&ctx);

        let mut action = None;
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| action = self.toolbar(ui, &ctx));
            ui.add_space(4.0);
        });
        egui::Panel::bottom("timeline").resizable(false).show(ui, |ui| {
            ui.add_space(6.0);
            self.timeline(ui);
            ui.add_space(6.0);
        });
        egui::Panel::right("side").resizable(true).default_size(400.0).show(ui, |ui| self.side_panel(ui, &ctx));
        egui::CentralPanel::default().show(ui, |ui| self.video(ui));
        self.overlay_dialog(&ctx);
        self.slide_dialog(&ctx);
        action
    }

    /// Swaps playback over to the cleaned-up narration, where it was.
    fn play_with_audio(&mut self, audio: &Path) {
        let preview = preview_size(self.project.width, self.project.height, 1600);
        let ctx = self.ctx.clone();
        match Player::with_audio(&self.project.media, audio, preview, ctx) {
            Ok(mut player) => {
                let (pos, playing) = (self.pos, self.player.playing);
                player.seek(pos);
                if playing {
                    player.play();
                }
                self.player = player;
            }
            Err(e) => self.message = Some(format!("Playing the original audio: {e:#}")),
        }
    }

    /// Keeps the camera player playing in step with the main one.
    fn sync_camera(&mut self) {
        let Some(cam) = &mut self.camera else { return };
        let _ = cam.poll();
        if cam.playing != self.player.playing {
            if self.player.playing { cam.play() } else { cam.pause() }
        }
        if self.player.playing && (cam.position() - self.pos).abs() > 0.15 {
            cam.seek(self.pos);
        }
    }

    /// The preview picture for slide `i`, loading it in the background on first use.
    fn slide_texture(&mut self, i: usize, ctx: &egui::Context) -> Result<Option<egui::TextureHandle>, String> {
        let slide = self.project.slides[i].clone();
        let key: SlideKey = (self.project.resolve(&slide.file), slide.page, slide.clip_start.to_bits());
        if let Some(SlideImage::Loading(job)) = self.slide_images.get_mut(&key)
            && let Some(result) = job.poll()
        {
            let state = match result {
                Ok(img) => SlideImage::Ready(ctx.load_texture("slide", img, egui::TextureOptions::LINEAR)),
                Err(e) => SlideImage::Failed(format!("{e:#}")),
            };
            self.slide_images.insert(key.clone(), state);
        }
        match self.slide_images.get(&key) {
            Some(SlideImage::Ready(tex)) => Ok(Some(tex.clone())),
            Some(SlideImage::Failed(e)) => Err(e.clone()),
            Some(SlideImage::Loading(_)) => Ok(None),
            None => {
                let project = self.snapshot();
                let job = Job::spawn(ctx, "Slide", move |_| load_image(&crate::sources::still(&project, &slide, 720)?));
                self.slide_images.insert(key, SlideImage::Loading(job));
                Ok(None)
            }
        }
    }

    fn poll_jobs(&mut self) {
        if let Some(job) = &mut self.clean_audio_job
            && let Some(result) = job.poll()
        {
            self.clean_audio_job = None;
            match result {
                Ok(Some(audio)) => self.play_with_audio(&audio),
                Ok(None) => {}
                Err(e) => self.message = Some(format!("Playing the original audio, cleanup failed: {e:#}")),
            }
        }
        if let Some(job) = &mut self.waveform_job
            && let Some(result) = job.poll()
        {
            self.waveform_job = None;
            match result {
                Ok(peaks) => {
                    self.waveform_floor_db = noise_floor_db(&peaks);
                    self.waveform = peaks;
                }
                Err(e) => self.message = Some(format!("Waveform failed: {e:#}")),
            }
        }
        if let Some(job) = &mut self.transcription
            && let Some(result) = job.poll()
        {
            self.transcription = None;
            match result {
                Ok(words) => {
                    self.project.words = words;
                    if let Err(e) = self.project.save_transcript() {
                        self.message = Some(format!("Saving the transcript failed: {e:#}"));
                    }
                }
                Err(e) => self.message = Some(format!("Transcription failed: {e:#}")),
            }
        }
        if let Some(job) = &mut self.export
            && let Some(result) = job.poll()
        {
            self.export = None;
            self.message = Some(match result {
                Ok(out) => format!("Exported to {}", out.display()),
                Err(e) => format!("Export failed: {e:#}"),
            });
        }
    }

    /// During playback, jumps over cut ranges so the preview matches the export.
    fn skip_cuts(&mut self) {
        if !self.player.playing {
            return;
        }
        if let Some(i) = self.project.cut_at(self.pos) {
            let end = self.project.cuts[i].end;
            if end >= self.project.duration - 0.05 {
                self.player.pause();
            } else {
                self.seek(end);
            }
        }
    }

    fn follow_playhead(&mut self) {
        let (start, len) = self.view;
        if self.pos < start || self.pos > start + len {
            self.view.0 = (self.pos - len * 0.1).clamp(0.0, (self.project.duration - len).max(0.0));
        }
    }

    fn upload_frame(&mut self, ctx: &egui::Context) {
        let Some(frame) = self.player.frame.lock().unwrap().take() else { return };
        let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
        match &mut self.texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => self.texture = Some(ctx.load_texture("video", image, egui::TextureOptions::LINEAR)),
        }
    }

    fn upload_camera_frame(&mut self, ctx: &egui::Context) {
        let Some(cam) = &self.camera else { return };
        let Some(frame) = cam.frame.lock().unwrap().take() else { return };
        let image = egui::ColorImage::from_rgba_unmultiplied([frame.width, frame.height], &frame.rgba);
        match &mut self.camera_texture {
            Some(t) => t.set(image, egui::TextureOptions::LINEAR),
            None => self.camera_texture = Some(ctx.load_texture("camera", image, egui::TextureOptions::LINEAR)),
        }
    }

    fn frame_step(&self) -> f64 {
        1.0 / self.project.fps.max(1.0)
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        // Consume the keys before widgets run, so e.g. Space doesn't also press a focused button.
        // Shift variants first: consume_key ignores extra Shift when matching.
        let (shift_left, shift_right, space, i, o, del, esc, left, right, undo) = ctx.input_mut(|inp| {
            use egui::Modifiers as M;
            (
                inp.consume_key(M::SHIFT, Key::ArrowLeft),
                inp.consume_key(M::SHIFT, Key::ArrowRight),
                inp.consume_key(M::NONE, Key::Space),
                inp.consume_key(M::NONE, Key::I),
                inp.consume_key(M::NONE, Key::O),
                inp.consume_key(M::NONE, Key::Delete) || inp.consume_key(M::NONE, Key::X),
                inp.consume_key(M::NONE, Key::Escape),
                inp.consume_key(M::NONE, Key::ArrowLeft),
                inp.consume_key(M::NONE, Key::ArrowRight),
                inp.consume_key(M::COMMAND, Key::Z),
            )
        });
        if space {
            self.player.toggle();
        }
        if i {
            self.mark_in();
        }
        if o {
            self.mark_out();
        }
        if del {
            self.cut_selection();
        }
        if esc {
            self.selection = None;
            self.placing = false;
        }
        let frame = self.frame_step();
        for (pressed, delta) in [(left, -frame), (right, frame), (shift_left, -1.0), (shift_right, 1.0)] {
            if pressed {
                self.seek(self.pos + delta);
            }
        }
        if undo {
            self.undo();
        }
    }

    fn mark_in(&mut self) {
        let end = self.selection.map_or(self.project.duration, |s| s.end.max(self.pos));
        self.selection = Some(Range::new(self.pos, end));
    }

    fn mark_out(&mut self) {
        let start = self.selection.map_or(0.0, |s| s.start.min(self.pos));
        self.selection = Some(Range::new(start, self.pos));
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) -> Option<Action> {
        let mut action = None;
        if ui.button("⏴ Home").clicked() {
            action = Some(Action::Home);
        }
        ui.separator();
        let label = if self.player.playing { "Pause" } else { "▶ Play" };
        if ui.button(label).on_hover_text("Space").clicked() {
            self.player.toggle();
        }
        ui.monospace(format!("{} / {}", fmt_time(self.pos), fmt_time(self.project.duration)));
        ui.separator();
        if ui.button("[ In").on_hover_text("Set selection start at playhead (I)").clicked() {
            self.mark_in();
        }
        if ui.button("Out ]").on_hover_text("Set selection end at playhead (O)").clicked() {
            self.mark_out();
        }
        ui.separator();
        if ui
            .add_enabled(self.selection.is_some(), egui::Button::new("✂ Cut selection"))
            .on_hover_text("Delete / X")
            .clicked()
        {
            self.cut_selection();
        }
        if ui.button("+ Add overlay").on_hover_text("Text over the selected time range").clicked() {
            self.new_overlay_draft();
        }
        if ui.button("+ Slide…").on_hover_text("Show an image, PDF/presentation page or video clip instead of the screen for the selected time range").clicked() {
            self.add_slide();
        }
        let has_camera = self.project.camera_size.is_some();
        ui.add_enabled_ui(has_camera, |ui| {
            ui.menu_button("Camera", |ui| {
                ui.label("For the selection:");
                for mode in CameraMode::ALL {
                    if ui.add_enabled(self.selection.is_some(), egui::Button::new(mode.label())).clicked()
                        && let Some(sel) = self.selection
                    {
                        self.edit(|p| p.set_camera(sel, mode));
                        ui.close();
                    }
                }
                ui.separator();
                ui.label("Everywhere else:");
                for mode in CameraMode::ALL {
                    if ui.radio(self.project.camera.default == mode, mode.label()).clicked() {
                        self.edit(|p| p.camera.default = mode);
                    }
                }
                ui.separator();
                ui.label("Small window corner:");
                for corner in Corner::ALL {
                    if ui.radio(self.project.camera.corner == corner, corner.name()).clicked() {
                        self.edit(|p| p.camera.corner = corner);
                    }
                }
            })
        })
        .response
        .on_disabled_hover_text("This recording has no camera track");
        if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("Undo")).on_hover_text("Ctrl+Z").clicked() {
            self.undo();
        }
        ui.separator();
        match &self.export {
            Some(job) => job.ui(ui),
            None => {
                if ui.button("Export…").clicked() {
                    self.start_export(ctx);
                }
                if ui
                    .add_enabled(!self.project.words.is_empty(), egui::Button::new("Export script…"))
                    .on_hover_text("The transcript after cuts, as Markdown, to re-record from")
                    .clicked()
                {
                    self.export_script();
                }
            }
        }
        ui.monospace(format!("→ {}", fmt_time(self.project.output_duration())))
            .on_hover_text("Length after cuts");
        // Selection info goes last so the buttons above never shift around.
        if let Some(sel) = self.selection {
            ui.separator();
            ui.monospace(format!("sel {} – {} ({:.2}s)", fmt_time(sel.start), fmt_time(sel.end), sel.len()));
            if ui.small_button("×").on_hover_text("Clear selection (Esc)").clicked() {
                self.selection = None;
            }
        }
        if let Some(msg) = &self.message {
            ui.separator();
            ui.label(msg);
            if ui.small_button("×").clicked() {
                self.message = None;
            }
        }
        action
    }

    // ---------------------------------------------------------------- video

    fn video(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let aspect = self.project.width.max(1) as f32 / self.project.height.max(1) as f32;
        let mut size = vec2(avail.width(), avail.width() / aspect);
        if size.y > avail.height() {
            size = vec2(avail.height() * aspect, avail.height());
        }
        let rect = Rect::from_center_size(avail.center(), size);
        let response = ui.allocate_rect(rect, Sense::click());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        if let Some(tex) = &self.texture {
            painter.image(tex.id(), rect, uv, Color32::WHITE);
        }

        // Same layering as the export: screen < slide < camera < text.
        if let Some(i) = self.project.slide_at(self.pos) {
            painter.rect_filled(rect, 0.0, Color32::BLACK);
            match self.slide_texture(i, ui.ctx()) {
                Ok(Some(tex)) => {
                    let [w, h] = tex.size();
                    painter.image(tex.id(), fit_rect(rect, w as f32 / h.max(1) as f32), uv, Color32::WHITE);
                }
                Ok(None) => {
                    painter.text(rect.center(), Align2::CENTER_CENTER, "loading slide…", FontId::proportional(16.0), Color32::GRAY);
                }
                Err(e) => {
                    painter.text(rect.center(), Align2::CENTER_CENTER, format!("slide {}: {e}", i + 1), FontId::proportional(14.0), Color32::from_rgb(230, 90, 90));
                }
            }
        }
        if let (Some(tex), Some((cw, ch))) = (&self.camera_texture, self.project.camera_size) {
            let aspect = cw as f32 / ch.max(1) as f32;
            match self.project.camera_mode_at(self.pos) {
                CameraMode::Full => {
                    painter.rect_filled(rect, 0.0, Color32::BLACK);
                    painter.image(tex.id(), fit_rect(rect, aspect), uv, Color32::WHITE);
                }
                CameraMode::Pip => {
                    let c = self.project.camera;
                    let w = rect.width() * c.width;
                    let m = rect.width() * c.margin;
                    let size = vec2(w, w / aspect);
                    let x = if matches!(c.corner, Corner::TopLeft | Corner::BottomLeft) { rect.left() + m } else { rect.right() - m - size.x };
                    let y = if matches!(c.corner, Corner::TopLeft | Corner::TopRight) { rect.top() + m } else { rect.bottom() - m - size.y };
                    painter.image(tex.id(), Rect::from_min_size(pos2(x, y), size), uv, Color32::WHITE);
                }
                CameraMode::Off => {}
            }
        }

        let editing = self.draft.as_ref().and_then(|d| d.index);
        for (i, o) in self.project.overlays.iter().enumerate() {
            if Some(i) != editing && o.start <= self.pos && self.pos < o.end {
                paint_overlay(&painter, rect, o, None);
            }
        }

        if let Some(draft) = &mut self.draft {
            if self.placing {
                if let Some(p) = response.hover_pos() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
                    paint_overlay(&painter, rect, &draft.overlay, Some(p));
                }
                let hint = painter.layout_no_wrap(
                    "Click where the text should go  (Esc to cancel)".into(),
                    FontId::proportional(16.0),
                    Color32::YELLOW,
                );
                let at = rect.left_top() + vec2(10.0, 10.0);
                painter.rect_filled(Rect::from_min_size(at, hint.size()).expand(6.0), 4.0, Color32::from_black_alpha(200));
                painter.galley(at, hint, Color32::YELLOW);
                if response.clicked()
                    && let Some(p) = response.interact_pointer_pos()
                {
                    draft.overlay.x = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
                    draft.overlay.y = ((p.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
                    self.placing = false;
                    self.commit_draft();
                }
            } else if draft.index.is_some() || !draft.overlay.text.is_empty() {
                paint_overlay(&painter, rect, &draft.overlay, None);
            }
        } else if response.clicked() {
            self.player.toggle();
        }
    }

    // ------------------------------------------------------------- overlays

    fn commit_draft(&mut self) {
        let Some(draft) = self.draft.take() else { return };
        self.edit(|p| match draft.index {
            Some(i) if i < p.overlays.len() => p.overlays[i] = draft.overlay,
            _ => p.overlays.push(draft.overlay),
        });
    }

    fn overlay_dialog(&mut self, ctx: &egui::Context) {
        // Hidden while placing so the whole video is clickable.
        if self.placing {
            return;
        }
        let Some(draft) = &mut self.draft else { return };
        let mut open = true;
        let mut place = false;
        let mut save = false;
        let mut delete = false;
        let duration = self.project.duration;
        let (selection, pos) = (self.selection, self.pos);
        let title = if draft.index.is_some() { "Edit overlay" } else { "Add overlay" };
        egui::Window::new(title)
            .open(&mut open)
            .resizable(false)
            .default_pos(ctx.content_rect().center() - vec2(160.0, 120.0))
            .show(ctx, |ui| {
                ui.label("Text");
                ui.add(egui::TextEdit::multiline(&mut draft.overlay.text).desired_rows(2).desired_width(300.0));
                egui::Grid::new("overlay_grid").num_columns(2).show(ui, |ui| {
                    let o = &mut draft.overlay;
                    ui.label("From");
                    ui.add(egui::DragValue::new(&mut o.start).speed(0.05).range(0.0..=duration).custom_formatter(|v, _| fmt_time(v)));
                    ui.end_row();
                    ui.label("To");
                    ui.add(egui::DragValue::new(&mut o.end).speed(0.05).range(0.0..=duration).custom_formatter(|v, _| fmt_time(v)));
                    ui.end_row();
                    ui.label("");
                    ui.horizontal(|ui| {
                        if ui.add_enabled(selection.is_some(), egui::Button::new("Use selection")).clicked()
                            && let Some(s) = selection
                        {
                            (o.start, o.end) = (s.start, s.end);
                        }
                        if ui.button("Playhead +3s").clicked() {
                            (o.start, o.end) = (pos, (pos + 3.0).min(duration));
                        }
                    });
                    ui.end_row();
                    ui.label("Size");
                    let mut pct = o.size * 100.0;
                    if ui.add(egui::Slider::new(&mut pct, 2.0..=20.0).suffix("% of height")).changed() {
                        o.size = pct / 100.0;
                    }
                    ui.end_row();
                    ui.label("Colour");
                    ui.color_edit_button_srgb(&mut o.color);
                    ui.end_row();
                    if draft.index.is_some() {
                        ui.label("Position");
                        ui.label(format!("{:.0}%, {:.0}%", o.x * 100.0, o.y * 100.0));
                        ui.end_row();
                    }
                });
                if draft.overlay.end < draft.overlay.start {
                    std::mem::swap(&mut draft.overlay.start, &mut draft.overlay.end);
                }
                ui.separator();
                let has_text = !draft.overlay.text.trim().is_empty();
                ui.horizontal(|ui| {
                    let label = if draft.index.is_some() { "Move on video…" } else { "Place on video…" };
                    if ui.add_enabled(has_text, egui::Button::new(label)).clicked() {
                        place = true;
                    }
                    if draft.index.is_some() {
                        if ui.add_enabled(has_text, egui::Button::new("Save")).clicked() {
                            save = true;
                        }
                        if ui.button("Delete").clicked() {
                            delete = true;
                        }
                    }
                });
            });
        if place {
            self.placing = true;
            let start = self.draft.as_ref().map(|d| d.overlay.start).unwrap_or(0.0);
            if !(self.draft.as_ref().is_some_and(|d| d.overlay.start <= self.pos && self.pos < d.overlay.end)) {
                self.seek(start);
            }
        }
        if save {
            self.commit_draft();
        } else if delete {
            if let Some(i) = self.draft.take().and_then(|d| d.index) {
                self.edit(|p| {
                    p.overlays.remove(i);
                });
            }
        } else if !open {
            self.draft = None;
            self.placing = false;
        }
    }

    /// Picks a file and opens the slide dialog for the selection (or 5 s from the playhead).
    fn add_slide(&mut self) {
        let mut dialog = rfd::FileDialog::new().add_filter("Image, PDF, presentation or video", crate::sources::EXTENSIONS);
        dialog = dialog.set_directory(self.project.base_dir());
        let Some(file) = dialog.pick_file() else { return };
        let base = self.project.base_dir();
        let file = file.strip_prefix(&base).map(Path::to_path_buf).unwrap_or(file);
        let range = self
            .selection
            .unwrap_or_else(|| Range::new(self.pos, (self.pos + 5.0).min(self.project.duration)));
        self.slide_draft = Some(SlideDraft {
            index: None,
            slide: Slide { start: range.start, end: range.end, file, page: 1, clip_start: 0.0 },
        });
    }

    fn edit_slide(&mut self, i: usize) {
        self.slide_draft = Some(SlideDraft { index: Some(i), slide: self.project.slides[i].clone() });
    }

    fn slide_dialog(&mut self, ctx: &egui::Context) {
        let Some(draft) = &mut self.slide_draft else { return };
        let (duration, selection, pos) = (self.project.duration, self.selection, self.pos);
        let kind = crate::sources::kind(&draft.slide.file);
        let (mut open, mut save, mut delete, mut change_file) = (true, false, false, false);
        let title = if draft.index.is_some() { "Edit slide" } else { "Add slide" };
        egui::Window::new(title).open(&mut open).resizable(false).show(ctx, |ui| {
            let s = &mut draft.slide;
            egui::Grid::new("slide_grid").num_columns(2).show(ui, |ui| {
                ui.label("File");
                ui.horizontal(|ui| {
                    ui.monospace(s.file.display().to_string());
                    if ui.small_button("Change…").clicked() {
                        change_file = true;
                    }
                });
                ui.end_row();
                ui.label("From");
                ui.add(egui::DragValue::new(&mut s.start).speed(0.05).range(0.0..=duration).custom_formatter(|v, _| fmt_time(v)));
                ui.end_row();
                ui.label("To");
                ui.add(egui::DragValue::new(&mut s.end).speed(0.05).range(0.0..=duration).custom_formatter(|v, _| fmt_time(v)));
                ui.end_row();
                ui.label("");
                ui.horizontal(|ui| {
                    if ui.add_enabled(selection.is_some(), egui::Button::new("Use selection")).clicked()
                        && let Some(sel) = selection
                    {
                        (s.start, s.end) = (sel.start, sel.end);
                    }
                    if ui.button("Playhead +5s").clicked() {
                        (s.start, s.end) = (pos, (pos + 5.0).min(duration));
                    }
                });
                ui.end_row();
                match kind {
                    Some(crate::sources::Kind::Pdf | crate::sources::Kind::Presentation) => {
                        ui.label("Page");
                        ui.add(egui::DragValue::new(&mut s.page).range(1..=9999));
                        ui.end_row();
                    }
                    Some(crate::sources::Kind::Video) => {
                        ui.label("Start clip at");
                        ui.add(egui::DragValue::new(&mut s.clip_start).speed(0.05).range(0.0..=f64::MAX).custom_formatter(|v, _| fmt_time(v)));
                        ui.end_row();
                    }
                    _ => {}
                }
            });
            if s.end < s.start {
                std::mem::swap(&mut s.start, &mut s.end);
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(if draft.index.is_some() { "Save" } else { "Add" }).clicked() {
                    save = true;
                }
                if draft.index.is_some() && ui.button("Delete").clicked() {
                    delete = true;
                }
            });
        });
        if change_file
            && let Some(file) = rfd::FileDialog::new()
                .add_filter("Image, PDF, presentation or video", crate::sources::EXTENSIONS)
                .set_directory(self.project.base_dir())
                .pick_file()
        {
            let base = self.project.base_dir();
            draft.slide.file = file.strip_prefix(&base).map(Path::to_path_buf).unwrap_or(file);
        }
        if save {
            let draft = self.slide_draft.take().unwrap();
            self.edit(|p| match draft.index {
                Some(i) if i < p.slides.len() => p.slides[i] = draft.slide,
                _ => p.slides.push(draft.slide),
            });
        } else if delete {
            if let Some(i) = self.slide_draft.take().and_then(|d| d.index) {
                self.edit(|p| {
                    p.slides.remove(i);
                });
            }
        } else if !open {
            self.slide_draft = None;
        }
    }

    fn edit_overlay(&mut self, i: usize) {
        self.draft = Some(Draft { index: Some(i), overlay: self.project.overlays[i].clone() });
        self.placing = false;
        let start = self.project.overlays[i].start;
        self.seek(start);
    }

    // ------------------------------------------------------------- timeline

    fn timeline(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(vec2(width, 184.0), Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let duration = self.project.duration.max(0.1);

        // Zoom with the wheel around the pointer; pan with shift+wheel / horizontal scroll.
        if response.hovered() {
            let (scroll, pointer) = ui.input(|i| (i.smooth_scroll_delta, i.pointer.hover_pos()));
            let (mut start, mut len) = self.view;
            if scroll.y != 0.0 {
                let anchor = pointer.map_or(start + len / 2.0, |p| self.x_to_t(rect, p.x));
                let new_len = (len * (-scroll.y as f64 * 0.003).exp()).clamp(1.0, duration);
                start = anchor - (anchor - start) * new_len / len;
                len = new_len;
            }
            if scroll.x != 0.0 {
                start -= scroll.x as f64 / rect.width() as f64 * len;
            }
            self.view = (start.clamp(0.0, (duration - len).max(0.0)), len);
        }

        let ruler = Rect::from_min_size(rect.min, vec2(width, 18.0));
        let track = Rect::from_min_size(pos2(rect.left(), ruler.bottom() + 2.0), vec2(width, 44.0));
        let wave = Rect::from_min_size(pos2(rect.left(), track.bottom() + 2.0), vec2(width, 56.0));
        let visual = Rect::from_min_size(pos2(rect.left(), wave.bottom() + 4.0), vec2(width, 22.0));
        let cam = Rect::from_min_size(pos2(rect.left(), visual.bottom() + 2.0), vec2(width, 8.0));
        let lane = Rect::from_min_size(pos2(rect.left(), cam.bottom() + 2.0), vec2(width, 22.0));
        let visuals = ui.visuals();
        painter.rect_filled(track, 3.0, visuals.extreme_bg_color);
        painter.rect_filled(wave, 3.0, visuals.extreme_bg_color);
        painter.rect_filled(lane, 3.0, visuals.faint_bg_color);
        painter.rect_filled(visual, 3.0, visuals.faint_bg_color);

        // Ruler ticks.
        let (start, len) = self.view;
        let px_per_s = width as f64 / len;
        let step = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0]
            .into_iter()
            .find(|s| s * px_per_s >= 80.0)
            .unwrap_or(3600.0);
        let mut t = (start / step).floor() * step;
        while t <= start + len {
            let x = self.t_to_x(rect, t);
            painter.line_segment([pos2(x, ruler.top() + 10.0), pos2(x, ruler.bottom())], Stroke::new(1.0, visuals.weak_text_color()));
            painter.text(pos2(x + 3.0, ruler.top()), Align2::LEFT_TOP, fmt_time(t), FontId::monospace(10.0), visuals.weak_text_color());
            t += step;
        }

        // Words as alternating blocks, labelled when zoomed in far enough.
        let shades = [Color32::from_gray(55), Color32::from_gray(75)];
        let word_font = FontId::proportional(11.0);
        for (i, w) in self.project.words.iter().enumerate() {
            if w.end < start || w.start > start + len {
                continue;
            }
            let r = self.span(track, w.start, w.end).shrink2(vec2(0.5, 8.0));
            painter.rect_filled(r, 2.0, shades[i % 2]);
            if r.width() > 6.0 * w.text.len() as f32 {
                painter.with_clip_rect(r).text(r.center(), Align2::CENTER_CENTER, &w.text, word_font.clone(), Color32::from_gray(210));
            }
        }

        self.paint_waveform(&painter, wave);

        // Cuts, selection and resume points span the word track and the waveform.
        let edit_area = track.union(wave);
        for c in &self.project.cuts {
            painter.rect_filled(self.span(edit_area, c.start, c.end), 0.0, CUT);
        }
        if let Some(s) = self.selection {
            painter.rect_filled(self.span(edit_area, s.start, s.end), 0.0, SELECTION);
        }
        for &b in &self.project.segment_starts {
            let x = self.t_to_x(rect, b);
            painter.line_segment([pos2(x, edit_area.top()), pos2(x, edit_area.bottom())], Stroke::new(2.0, SEGMENT));
        }
        for s in &self.project.slides {
            let r = self.span(visual, s.start, s.end).shrink2(vec2(0.0, 2.0));
            painter.rect_filled(r, 3.0, SLIDE_BAR);
            let name = s.file.file_name().unwrap_or_default().to_string_lossy();
            let label = match crate::sources::kind(&s.file) {
                Some(crate::sources::Kind::Pdf | crate::sources::Kind::Presentation) => format!("{name} p{}", s.page),
                _ => name.to_string(),
            };
            painter.with_clip_rect(r).text(r.left_center() + vec2(4.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(12.0), Color32::WHITE);
        }
        // Camera: effective layout as fill, explicit ranges outlined.
        if self.project.camera_size.is_some() {
            for (mode, color) in [(CameraMode::Full, CAMERA_FULL), (CameraMode::Pip, CAMERA_PIP)] {
                for r in self.project.camera_intervals(mode) {
                    painter.rect_filled(self.span(cam, r.start, r.end), 2.0, color);
                }
            }
            for r in &self.project.camera_ranges {
                painter.rect_stroke(self.span(cam, r.start, r.end), 2.0, Stroke::new(1.0, Color32::WHITE), egui::StrokeKind::Inside);
            }
        }
        for o in &self.project.overlays {
            let r = self.span(lane, o.start, o.end).shrink2(vec2(0.0, 2.0));
            painter.rect_filled(r, 3.0, OVERLAY_BAR);
            painter.with_clip_rect(r).text(
                r.left_center() + vec2(4.0, 0.0),
                Align2::LEFT_CENTER,
                o.text.lines().next().unwrap_or(""),
                FontId::proportional(12.0),
                Color32::WHITE,
            );
        }
        let x = self.t_to_x(rect, self.pos);
        painter.line_segment([pos2(x, ruler.top()), pos2(x, lane.bottom())], Stroke::new(2.0, Color32::WHITE));

        // Interaction: click = seek, drag = select, right-click = remove cut / edit overlay.
        if response.drag_started()
            && let Some(p) = response.interact_pointer_pos()
        {
            self.drag_from = Some(self.x_to_t(rect, p.x));
        }
        if response.dragged()
            && let (Some(from), Some(p)) = (self.drag_from, response.interact_pointer_pos())
        {
            let to = self.x_to_t(rect, p.x);
            self.selection = Some(Range::new(from, to));
            self.seek(to);
        }
        if response.drag_stopped() {
            self.drag_from = None;
            if self.selection.is_some_and(|s| s.len() < 0.02) {
                self.selection = None;
            }
        }
        if response.clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            let t = self.x_to_t(rect, p.x);
            if let Some(i) = self.overlay_at(lane, p, t) {
                self.edit_overlay(i);
            } else if let Some(i) = self.slide_at_lane(visual, p, t) {
                self.edit_slide(i);
            } else {
                self.seek(t);
            }
        }
        if response.secondary_clicked()
            && let Some(p) = response.interact_pointer_pos()
        {
            let t = self.x_to_t(rect, p.x);
            if let Some(i) = self.overlay_at(lane, p, t) {
                self.edit_overlay(i);
            } else if let Some(i) = self.slide_at_lane(visual, p, t) {
                self.edit_slide(i);
            } else if cam.expand2(vec2(0.0, 2.0)).contains(p)
                && let Some(i) = self.project.camera_ranges.iter().rposition(|r| r.start <= t && t < r.end)
            {
                self.edit(|p| {
                    p.camera_ranges.remove(i);
                });
            } else if let Some(i) = self.project.cut_at(t) {
                self.edit(|p| {
                    p.cuts.remove(i);
                });
            }
        }
        response.on_hover_text_at_pointer(
            "Click: seek · Drag: select · Wheel: zoom · Shift+wheel: pan\nRight-click a red cut to restore it · Click a slide (purple) or overlay (green) bar to edit it\nCamera strip: orange = only me, brown = small window; right-click an outlined range to remove it",
        );
    }

    /// Draws the loudness envelope on a dB scale, so pauses between words show
    /// up as clear gaps. One bar per pixel column, using the column's peak.
    fn paint_waveform(&self, painter: &egui::Painter, wave: Rect) {
        if self.waveform.is_empty() {
            let text = if self.waveform_job.is_some() { "loading waveform…" } else { "no audio" };
            painter.text(wave.center(), Align2::CENTER_CENTER, text, FontId::proportional(11.0), Color32::from_gray(110));
            return;
        }
        let rate = crate::media::PEAKS_PER_SEC as f64;
        let mid = wave.center().y;
        let half = wave.height() / 2.0 - 2.0;
        let columns = wave.width().ceil() as usize;
        for c in 0..columns {
            let x = wave.left() + c as f32;
            let (t0, t1) = (self.x_to_t(wave, x), self.x_to_t(wave, x + 1.0));
            let (i0, i1) = ((t0 * rate) as usize, ((t1 * rate).ceil() as usize).max((t0 * rate) as usize + 1));
            let Some(bins) = self.waveform.get(i0.min(self.waveform.len())..i1.min(self.waveform.len())) else { continue };
            let peak = bins.iter().copied().fold(0.0f32, f32::max);
            if peak <= 0.0 {
                continue;
            }
            let floor = self.waveform_floor_db;
            let level = ((20.0 * peak.log10() - floor) / -floor).clamp(0.0, 1.0);
            let h = (level * half).max(0.5);
            painter.line_segment([pos2(x + 0.5, mid - h), pos2(x + 0.5, mid + h)], Stroke::new(1.0, WAVE));
        }
        // Word starts as faint guides, to relate the gaps to the transcript.
        let guide = Stroke::new(1.0, Color32::from_white_alpha(28));
        let (start, len) = self.view;
        for w in self.project.words.iter().filter(|w| w.start >= start && w.start <= start + len) {
            let x = self.t_to_x(wave, w.start);
            painter.line_segment([pos2(x, wave.top()), pos2(x, wave.bottom())], guide);
        }
    }

    fn slide_at_lane(&self, lane: Rect, p: Pos2, t: f64) -> Option<usize> {
        if !lane.contains(p) {
            return None;
        }
        self.project.slide_at(t)
    }

    fn overlay_at(&self, lane: Rect, p: Pos2, t: f64) -> Option<usize> {
        if !lane.contains(p) {
            return None;
        }
        self.project.overlays.iter().position(|o| o.start <= t && t < o.end)
    }

    fn t_to_x(&self, rect: Rect, t: f64) -> f32 {
        rect.left() + ((t - self.view.0) / self.view.1) as f32 * rect.width()
    }

    fn x_to_t(&self, rect: Rect, x: f32) -> f64 {
        (self.view.0 + ((x - rect.left()) / rect.width()) as f64 * self.view.1).clamp(0.0, self.project.duration)
    }

    fn span(&self, row: Rect, a: f64, b: f64) -> Rect {
        let (xa, xb) = (self.t_to_x(row, a), self.t_to_x(row, b));
        Rect::from_min_max(pos2(xa, row.top()), pos2(xb.max(xa + 1.0), row.bottom())).intersect(row)
    }

    // ----------------------------------------------------------- side panel

    fn side_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::Panel::bottom("lists").resizable(true).default_size(200.0).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.lists(ui));
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading("Transcript");
            match &self.transcription {
                Some(job) => job.ui(ui),
                None if self.project.has_audio => {
                    if ui.small_button("Re-transcribe").clicked() {
                        self.start_transcription(ctx);
                    }
                }
                None => {
                    ui.weak("(no audio)");
                }
            }
        });
        ui.weak("Click a word to jump there · Shift+click to select a range");
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.transcript(ui));
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        let current = self.project.word_at(self.pos);
        let scroll_to_current = self.player.playing && current != self.last_scrolled_word;
        let shift = ui.input(|i| i.modifiers.shift);
        let mut clicked = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = vec2(4.0, 6.0);
            let mut boundaries = self.project.segment_starts.iter().peekable();
            for (i, w) in self.project.words.iter().enumerate() {
                if let Some(&&b) = boundaries.peek()
                    && w.start >= b
                {
                    while boundaries.next_if(|&&b| w.start >= b).is_some() {}
                    ui.end_row();
                    ui.label(RichText::new("— resumed —").small().color(SEGMENT));
                    ui.end_row();
                } else if i > 0 && w.start - self.project.words[i - 1].end > 1.5 {
                    ui.end_row();
                }
                let mut text = RichText::new(&w.text).size(15.0);
                if self.project.cut_at((w.start + w.end) / 2.0).is_some() {
                    text = text.strikethrough().color(ui.visuals().weak_text_color());
                }
                if Some(i) == current {
                    text = text.background_color(CURRENT_WORD).color(Color32::WHITE);
                } else if self.selection.is_some_and(|s| w.start >= s.start - 0.01 && w.end <= s.end + 0.01) {
                    text = text.background_color(SELECTED_WORD);
                }
                let r = ui
                    .add(egui::Label::new(text).sense(Sense::click()).selectable(false))
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if r.clicked() {
                    clicked = Some(i);
                }
                if scroll_to_current && Some(i) == current {
                    r.scroll_to_me(Some(Align::Center));
                }
            }
        });
        if scroll_to_current {
            self.last_scrolled_word = current;
        }
        if self.project.words.is_empty() && self.transcription.is_none() && self.project.has_audio {
            ui.weak("No speech found.");
        }

        if let Some(i) = clicked {
            let w = &self.project.words[i];
            match self.anchor_word.filter(|_| shift) {
                Some(a) => {
                    let a = &self.project.words[a.min(self.project.words.len() - 1)];
                    self.selection = Some(Range::new(a.start.min(w.start), a.end.max(w.end)));
                }
                None => {
                    self.anchor_word = Some(i);
                    let t = w.start;
                    self.seek(t);
                }
            }
        }
    }

    fn lists(&mut self, ui: &mut egui::Ui) {
        let mut seek = None;
        let mut restore = None;
        let mut edit = None;
        egui::CollapsingHeader::new(format!("Cuts ({})", self.project.cuts.len()))
            .default_open(true)
            .show(ui, |ui| {
                for (i, c) in self.project.cuts.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.link(format!("{} – {}", fmt_time(c.start), fmt_time(c.end))).clicked() {
                            seek = Some(c.start);
                        }
                        ui.weak(format!("{:.1}s", c.len()));
                        if !c.note.is_empty() {
                            ui.weak(&c.note);
                        }
                        if ui.small_button("Restore").clicked() {
                            restore = Some(i);
                        }
                    });
                }
            });
        egui::CollapsingHeader::new(format!("Overlays ({})", self.project.overlays.len()))
            .default_open(true)
            .show(ui, |ui| {
                for (i, o) in self.project.overlays.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.link(format!("{} – {}", fmt_time(o.start), fmt_time(o.end))).clicked() {
                            seek = Some(o.start);
                        }
                        ui.label(o.text.lines().next().unwrap_or(""));
                        if ui.small_button("Edit").clicked() {
                            edit = Some(i);
                        }
                    });
                }
            });
        let mut edit_slide = None;
        egui::CollapsingHeader::new(format!("Slides ({})", self.project.slides.len()))
            .default_open(true)
            .show(ui, |ui| {
                for (i, s) in self.project.slides.iter().enumerate() {
                    ui.horizontal(|ui| {
                        if ui.link(format!("{} – {}", fmt_time(s.start), fmt_time(s.end))).clicked() {
                            seek = Some(s.start);
                        }
                        ui.label(s.file.file_name().unwrap_or_default().to_string_lossy());
                        if ui.small_button("Edit").clicked() {
                            edit_slide = Some(i);
                        }
                    });
                }
            });
        let mut remove_camera = None;
        if self.project.camera_size.is_some() {
            egui::CollapsingHeader::new(format!("Camera ({}, otherwise {})", self.project.camera_ranges.len(), self.project.camera.default.label().to_lowercase()))
                .default_open(true)
                .show(ui, |ui| {
                    for (i, r) in self.project.camera_ranges.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.link(format!("{} – {}", fmt_time(r.start), fmt_time(r.end))).clicked() {
                                seek = Some(r.start);
                            }
                            ui.label(r.mode.label());
                            if ui.small_button("Remove").clicked() {
                                remove_camera = Some(i);
                            }
                        });
                    }
                });
        }
        if let Some(t) = seek {
            self.seek(t);
        }
        if let Some(i) = edit_slide {
            self.edit_slide(i);
        }
        if let Some(i) = remove_camera {
            self.edit(|p| {
                p.camera_ranges.remove(i);
            });
        }
        if let Some(i) = restore {
            self.edit(|p| {
                p.cuts.remove(i);
            });
        }
        if let Some(i) = edit {
            self.edit_overlay(i);
        }
    }
}

/// Estimates the background noise as the 15th percentile of the peaks, so
/// pauses sit at the bottom of the waveform even in a noisy recording.
fn noise_floor_db(peaks: &[f32]) -> f32 {
    let mut sorted: Vec<f32> = peaks.iter().copied().filter(|p| *p > 0.0).collect();
    if sorted.is_empty() {
        return -50.0;
    }
    sorted.sort_by(f32::total_cmp);
    let noise = sorted[sorted.len() * 15 / 100];
    // Keep at least 12 dB of range so a constantly loud track still shows shape.
    (20.0 * noise.log10() + 3.0).clamp(-60.0, -12.0)
}

/// Creates a project for a media file by probing it.
pub fn new_project(media: &Path, segment_starts: Vec<f64>) -> Result<Project> {
    let info = crate::media::probe(media)?;
    Ok(Project {
        media: std::fs::canonicalize(media)?,
        duration: info.duration,
        width: info.width,
        height: info.height,
        fps: info.fps,
        has_audio: info.has_audio,
        camera_size: info.camera,
        segment_starts,
        ..Default::default()
    })
}

/// Draws an overlay the way ffmpeg's drawtext will: centred on (x, y), sized
/// relative to the frame height, with a drop shadow. `at` overrides the position.
fn paint_overlay(painter: &egui::Painter, frame: Rect, o: &Overlay, at: Option<Pos2>) {
    let center = at.unwrap_or_else(|| pos2(frame.left() + o.x * frame.width(), frame.top() + o.y * frame.height()));
    let family = if painter.fonts(|f| f.families().iter().any(|f| *f == FontFamily::Name(OVERLAY_FONT.into()))) {
        FontFamily::Name(OVERLAY_FONT.into())
    } else {
        FontFamily::Proportional
    };
    let font = FontId::new((o.size * frame.height()).max(4.0), family);
    let [r, g, b] = o.color;
    let shadow = (2.0 * frame.height() / 1080.0).max(1.0);
    painter.text(center + Vec2::splat(shadow), Align2::CENTER_CENTER, &o.text, font.clone(), Color32::from_black_alpha(150));
    painter.text(center, Align2::CENTER_CENTER, &o.text, font, Color32::from_rgb(r, g, b));
}

pub fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let cs = (t * 100.0).round() as u64;
    let (h, m, s, c) = (cs / 360_000, cs / 6000 % 60, cs / 100 % 60, cs % 100);
    if h > 0 { format!("{h}:{m:02}:{s:02}.{c:02}") } else { format!("{m:02}:{s:02}.{c:02}") }
}

#[cfg(test)]
mod tests {
    use super::{fmt_time, noise_floor_db};

    #[test]
    fn noise_floor_tracks_quiet_parts() {
        let mut peaks = vec![0.01; 20];
        peaks.extend([1.0; 80]);
        assert!((noise_floor_db(&peaks) - -37.0).abs() < 0.01);
        assert_eq!(noise_floor_db(&[1.0; 10]), -12.0);
        assert_eq!(noise_floor_db(&[]), -50.0);
    }

    #[test]
    fn formats_time() {
        assert_eq!(fmt_time(0.0), "00:00.00");
        assert_eq!(fmt_time(61.234), "01:01.23");
        assert_eq!(fmt_time(3723.5), "1:02:03.50");
    }
}
