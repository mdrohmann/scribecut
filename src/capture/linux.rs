//! Linux capture: screen frames come from the xdg-desktop-portal ScreenCast
//! portal over PipeWire, encoded with x264 into Matroska segments alongside the
//! microphone and, optionally, the camera.

use std::os::fd::{IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use gst::prelude::*;

use super::chooser::{self, Choice};
use super::{Source, join_pieces};
use crate::geom::{Crop, Output};
use crate::platform;
use crate::settings::Settings;

/// One runtime for all portal calls. ashpd caches its D-Bus connection
/// globally, and that connection's background tasks live on the runtime that
/// created it: with a fresh runtime per call, later calls hang forever.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> = std::sync::LazyLock::new(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("portal")
            .enable_all()
            .build()
            .expect("failed to start the portal runtime")
    });
    &RUNTIME
}

/// An open ScreenCast portal session. Dropping it closes the session.
pub struct Portal {
    node_id: u32,
    fd: OwnedFd,
    /// Where the shared monitor sits in the compositor layout, if reported.
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    close: Option<tokio::sync::oneshot::Sender<()>>,
}

struct Opened {
    node_id: u32,
    fd: OwnedFd,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
    restore_token: Option<String>,
}

impl Portal {
    /// Asks the portal for a monitor or a window. Blocks until the user chooses
    /// (call off the UI thread). With a `restore_token` from an earlier session
    /// the portal may skip its picker; the returned session's new token is saved.
    fn open(source: SourceType, restore_token: Option<String>) -> Result<(Self, Option<String>)> {
        let (tx, rx) = mpsc::channel::<Result<Opened>>();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::spawn(move || {
            runtime().block_on(async move {
                let result: Result<_> = async {
                    let proxy = Screencast::new().await?;
                    let session = proxy.create_session(Default::default()).await?;
                    let persist = if source == SourceType::Monitor { PersistMode::ExplicitlyRevoked } else { PersistMode::DoNot };
                    proxy
                        .select_sources(
                            &session,
                            SelectSourcesOptions::default()
                                .set_cursor_mode(CursorMode::Embedded)
                                .set_sources(BitFlags::from(source))
                                .set_multiple(false)
                                .set_persist_mode(persist)
                                .set_restore_token(restore_token.as_deref()),
                        )
                        .await?;
                    let streams = proxy.start(&session, None, Default::default()).await?.response()?;
                    let stream = streams.streams().first().context("nothing selected")?.to_owned();
                    let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
                    let opened = Opened {
                        node_id: stream.pipe_wire_node_id(),
                        fd,
                        position: stream.position(),
                        size: stream.size(),
                        restore_token: streams.restore_token().map(str::to_owned),
                    };
                    Ok((proxy, session, opened))
                }
                .await;
                match result {
                    Ok((_proxy, session, opened)) => {
                        if tx.send(Ok(opened)).is_ok() {
                            let _ = close_rx.await;
                        }
                        let _ = session.close().await;
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
        });
        let o = rx.recv().map_err(|_| anyhow!("portal thread died"))??;
        let portal = Self { node_id: o.node_id, fd: o.fd, position: o.position, size: o.size, close: Some(close_tx) };
        Ok((portal, o.restore_token))
    }

    /// Whether this monitor stream shows `output`.
    fn shows(&self, output: &Output) -> bool {
        let a = output.area;
        match (self.position, self.size) {
            (Some(p), _) => p == (a.x, a.y),
            (None, Some(s)) => s == (a.w, a.h),
            (None, None) => true,
        }
    }
}

/// A connected screen stream, ready to record.
pub struct Connection {
    portal: Portal,
    crop: Option<Crop>,
    /// The window being shared on its own (sway container id and pid): its
    /// frame size is kept from the start, since it can be resized while
    /// recording and the file needs one size throughout, and if the portal
    /// dies the window is recorded cut out of the screen instead.
    window: Option<(i64, i32)>,
    /// Sway window whose visibility pauses/resumes the recording.
    watch: Option<i64>,
}

/// Whether the portal can share individual windows (sway >= 1.12).
pub fn supports_system_windows() -> bool {
    runtime().block_on(async {
        let Ok(proxy) = Screencast::new().await else { return false };
        proxy.available_source_types().await.is_ok_and(|t| t.contains(SourceType::Window))
    })
}

/// The camera a fresh install should use. V4L2 numbers its devices, and the
/// first one is the built-in camera on almost every laptop.
pub fn default_camera() -> String {
    "/dev/video0".to_string()
}

/// Whether the configured camera is present.
pub fn camera_available(device: &str) -> bool {
    !device.is_empty() && Path::new(device).exists()
}

/// Opens the screen stream for `source`. Blocks on the portal dialog, unless
/// scribecut's chooser answers it.
pub fn connect(source: Source) -> Result<Connection> {
    let plain = |portal| Connection { portal, crop: None, window: None, watch: None };
    match source {
        Source::Monitor(output) => {
            // With a single monitor there is nothing to choose.
            let output = output.or_else(|| match platform::outputs() {
                Ok(outputs) if outputs.len() == 1 => outputs.into_iter().next(),
                _ => None,
            });
            match output {
                Some(output) => Ok(plain(open_monitor(&output)?)),
                None => Ok(plain(Portal::open(SourceType::Monitor, None)?.0)),
            }
        }
        Source::SystemWindow => Ok(plain(Portal::open(SourceType::Window, None)?.0)),
        Source::Window { id, pid, toplevel } => {
            if let Some(toplevel) = toplevel.filter(|_| chooser::installed() && supports_system_windows()) {
                // Shared on its own: other windows on top don't show, and it
                // keeps recording on another workspace.
                let _choice = chooser::choose(Choice::Window(&toplevel))?;
                let (portal, _) = Portal::open(SourceType::Window, None)?;
                return Ok(Connection { portal, crop: None, window: Some((id, pid)), watch: None });
            }
            // Bring it to the front, record its area, and let the watcher pause
            // the recording whenever it stops being visible.
            let area = platform::focus_window(id, pid)?;
            Ok(Connection { watch: Some(id), ..connect(Source::Area(area))? })
        }
        Source::Area(area) => {
            let outputs = platform::outputs()?;
            let output = platform::output_for(&area, &outputs).context("that area is not on any monitor")?;
            let portal = open_monitor(&output)?;
            let crop = platform::crop_for(&area, &output);
            Ok(Connection { portal, crop: (!crop.is_none()).then_some(crop), window: None, watch: None })
        }
    }
}

/// Shares `output` without asking where possible: scribecut's chooser names
/// it, or the portal reuses the choice saved from the last time. Asks if it
/// was a different monitor.
fn open_monitor(output: &Output) -> Result<Portal> {
    if chooser::installed() {
        let _choice = chooser::choose(Choice::Monitor(&output.name))?;
        return Ok(Portal::open(SourceType::Monitor, None)?.0);
    }
    let token_file = dirs::data_dir().map(|d| d.join("scribecut/portal_token"));
    let saved = token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
    let mut attempt = Portal::open(SourceType::Monitor, saved)?;
    if !attempt.0.shows(output) {
        drop(attempt);
        attempt = Portal::open(SourceType::Monitor, None)?;
        if !attempt.0.shows(output) {
            bail!("please pick monitor {} in the dialog", output.name);
        }
    }
    let (portal, token) = attempt;
    if let (Some(file), Some(token)) = (token_file, token) {
        let _ = std::fs::create_dir_all(file.parent().unwrap());
        let _ = std::fs::write(file, token);
    }
    Ok(portal)
}

impl Drop for Portal {
    fn drop(&mut self) {
        if let Some(tx) = self.close.take() {
            let _ = tx.send(());
        }
    }
}

/// Screen frames → H.264 (frame rate made constant, since the compositor only
/// sends frames when something on screen changes).
const VIDEO_ENCODE: &str = "videorate ! video/x-raw,framerate=30/1 \
    ! queue max-size-time=2000000000 max-size-buffers=0 max-size-bytes=0 \
    ! x264enc tune=zerolatency speed-preset=veryfast bitrate=8000 key-int-max=60 ! h264parse";

/// Where screen frames go: the current segment's appsrc, plus the latest
/// frame so a new segment can start with a picture even if nothing on screen
/// changes (the compositor only sends frames on change).
#[derive(Default)]
struct FrameSink {
    src: Option<gst_app::AppSrc>,
    last: Option<gst::Sample>,
    /// Whether the current segment has had a frame yet.
    delivered: bool,
}

impl FrameSink {
    fn push(&mut self, sample: &gst::Sample) {
        let (Some(src), Some(caps), Some(buffer)) = (&self.src, sample.caps(), sample.buffer_owned()) else { return };
        let caps = caps.to_owned();
        if src.caps().as_ref() != Some(&caps) {
            src.set_caps(Some(&caps));
        }
        // Each segment's appsrc stamps frames on arrival.
        let mut buffer = buffer;
        let b = buffer.make_mut();
        b.set_pts(gst::ClockTime::NONE);
        b.set_dts(gst::ClockTime::NONE);
        b.set_duration(gst::ClockTime::NONE);
        self.delivered |= src.push_buffer(buffer).is_ok();
    }

    fn start(&mut self, src: gst_app::AppSrc) {
        self.src = Some(src);
        self.delivered = false;
        if let Some(last) = self.last.clone() {
            self.push(&last);
        }
    }
}

type Target = Arc<Mutex<FrameSink>>;

/// Recording state shared between the UI and the window watcher.
struct Segments {
    /// The appsrc of the segment being recorded, if any; frames go there.
    target: Target,
    dir: PathBuf,
    files: Vec<PathBuf>,
    current: Option<(gst::Pipeline, Instant)>,
    recorded: Duration,
    /// Paused with the Pause button (the watcher must not resume then).
    user_paused: bool,
    /// Paused because the watched window isn't visible.
    auto_paused: bool,
    error: Option<String>,
    /// Shown to the user without stopping the recording.
    warning: Option<String>,
    /// Camera device recorded as a second video track, if any.
    camera: Option<String>,
}

impl Segments {
    /// Starts (or resumes into) a new segment file.
    fn start(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        let path = self.dir.join(format!("segment_{:03}.mkv", self.files.len()));
        let (pipeline, appsrc) = match Self::launch(&path, self.camera.as_deref()) {
            Ok(started) => started,
            Err(e) if self.camera.is_some() => {
                // Busy or missing camera: keep recording the screen.
                self.warning = Some(format!("Camera unavailable, recording without it: {e:#}"));
                self.camera = None;
                Self::launch(&path, None)?
            }
            Err(e) => return Err(e),
        };
        self.target.lock().unwrap().start(appsrc);
        self.files.push(path);
        self.current = Some((pipeline, Instant::now()));
        Ok(())
    }

    /// Builds and starts one segment's pipeline: screen frames (pushed into the
    /// appsrc), the microphone and optionally the camera, all into one file so
    /// they share a clock.
    fn launch(path: &Path, camera: Option<&str>) -> Result<(gst::Pipeline, gst_app::AppSrc)> {
        let camera = camera
            .map(|dev| {
                format!(
                    "v4l2src device={dev} ! image/jpeg,width=1280,height=720,framerate=30/1 ! jpegdec \
                       ! videoconvert ! videorate ! video/x-raw,framerate=30/1 \
                       ! queue max-size-time=2000000000 max-size-buffers=0 max-size-bytes=0 \
                       ! x264enc tune=zerolatency speed-preset=veryfast bitrate=3000 key-int-max=60 \
                       ! h264parse ! queue ! mux. "
                )
            })
            .unwrap_or_default();
        // The screen branch comes first so it becomes the file's first video track.
        let desc = format!(
            "appsrc name=screen is-live=true do-timestamp=true format=time ! videoconvert ! {VIDEO_ENCODE} ! queue ! mux. \
             {camera}\
             pulsesrc do-timestamp=true ! audioconvert ! audioresample ! queue \
               ! opusenc bitrate=128000 ! queue ! mux. \
             matroskamux name=mux ! filesink location=\"{}\"",
            path.display(),
        );
        let pipeline = gst::parse::launch(&desc)
            .context("building recording pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        let appsrc = pipeline
            .by_name("screen")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .context("no appsrc")?;
        if let Err(e) = pipeline.set_state(gst::State::Playing) {
            let detail = pipeline
                .bus()
                .and_then(|b| b.pop_filtered(&[gst::MessageType::Error]))
                .and_then(|m| match m.view() {
                    gst::MessageView::Error(err) => Some(err.error().to_string()),
                    _ => None,
                });
            let _ = pipeline.set_state(gst::State::Null);
            return Err(anyhow!("{}", detail.unwrap_or_else(|| e.to_string())));
        }
        Ok((pipeline, appsrc))
    }

    /// Finishes the current segment. Safe to call when already stopped.
    fn stop(&mut self) -> Result<()> {
        let Some((pipeline, started)) = self.current.take() else { return Ok(()) };
        self.recorded += started.elapsed();
        let delivered = {
            let mut target = self.target.lock().unwrap();
            target.src = None;
            target.delivered
        };
        if !delivered {
            // No screen frame yet (a shared window that hasn't drawn since):
            // the muxer never started, the microphone is stuck pushing into a
            // full queue, and an EOS would wait on it forever. Nothing worth
            // keeping was recorded, so drop the piece.
            pipeline.set_state(gst::State::Null)?;
            if let Some(file) = self.files.pop() {
                let _ = std::fs::remove_file(file);
            }
            return Ok(());
        }
        pipeline.send_event(gst::event::Eos::new());
        let bus = pipeline.bus().context("no bus")?;
        let result = match bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        ) {
            Some(msg) => match msg.view() {
                gst::MessageView::Error(e) => Err(anyhow!("recording error: {}", e.error())),
                _ => Ok(()),
            },
            None => Err(anyhow!("timed out finalising segment")),
        };
        pipeline.set_state(gst::State::Null)?;
        result
    }

    /// Follows the watched window: pause while hidden, resume when shown.
    fn window_visible(&mut self, visible: bool) {
        let result = if !visible && self.current.is_some() {
            self.auto_paused = true;
            self.stop()
        } else if visible && self.auto_paused {
            self.auto_paused = false;
            if self.user_paused { Ok(()) } else { self.start() }
        } else {
            Ok(())
        };
        if let Err(e) = result {
            self.error = Some(format!("{e:#}"));
        }
    }
}

/// Polls sway for the recorded window's visibility until dropped.
struct Watcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn spawn(con_id: i64, segments: Arc<Mutex<Segments>>) -> Self {
        use std::sync::atomic::Ordering;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                // A closed window counts as hidden; errors talking to sway are ignored.
                if let Ok(visible) = platform::window_visible(con_id) {
                    segments.lock().unwrap().window_visible(visible.unwrap_or(false));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Pauses dunst notifications while alive, then restores the previous state.
struct QuietNotifications {
    was_paused: bool,
}

impl QuietNotifications {
    fn new() -> Option<Self> {
        let out = Command::new("dunstctl").arg("is-paused").stdin(Stdio::null()).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let was_paused = String::from_utf8_lossy(&out.stdout).trim() == "true";
        dunstctl_set_paused(true);
        Some(Self { was_paused })
    }
}

impl Drop for QuietNotifications {
    fn drop(&mut self) {
        dunstctl_set_paused(self.was_paused);
    }
}

fn dunstctl_set_paused(paused: bool) {
    let _ = Command::new("dunstctl")
        .args(["set-paused", if paused { "true" } else { "false" }])
        .stdin(Stdio::null())
        .status();
}

/// Enlarges sway's mouse pointer while alive, then restores it.
struct BigPointer {
    theme: String,
    size: u32,
}

impl BigPointer {
    fn new(scale: f32) -> Option<Self> {
        if !platform::available() {
            return None;
        }
        let theme = std::env::var("XCURSOR_THEME").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| "default".into());
        let size = std::env::var("XCURSOR_SIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(24);
        platform::set_cursor(&theme, (size as f32 * scale).round() as u32).ok()?;
        Some(Self { theme, size })
    }
}

impl Drop for BigPointer {
    fn drop(&mut self) {
        let _ = platform::set_cursor(&self.theme, self.size);
    }
}

/// A frame size shared between screen pipelines: set by the first frame, then
/// kept by every pipeline that replaces it.
type FixedSize = Arc<Mutex<Option<(i32, i32)>>>;

/// The screen stream, cropped if asked, as I420 frames handed to `on_frame`.
/// With `fixed` frames keep that size, or the first frame's if it is not set
/// yet: frames of another size are scaled to fit, with borders.
fn screen_pipeline(
    portal: &Portal,
    crop: Option<Crop>,
    fixed: Option<FixedSize>,
    on_frame: impl Fn(gst::Sample) + Send + Sync + 'static,
) -> Result<gst::Pipeline> {
    // pipewiresrc's own timestamps don't line up with the pipeline clock, so
    // run it unsynchronised and let each segment's appsrc stamp frames on arrival.
    let fd = portal.fd.try_clone()?.into_raw_fd();
    let crop = crop
        .map(|c| format!("! videocrop left={} top={} right={} bottom={} ", c.left, c.top, c.right, c.bottom))
        .unwrap_or_default();
    let screen = gst::parse::launch(&format!(
        "pipewiresrc fd={fd} path={} keepalive-time=500 {crop}! videoconvert ! videoscale add-borders=true \
         ! capsfilter name=fit ! video/x-raw,format=I420 ! appsink name=frames sync=false max-buffers=4 drop=true",
        portal.node_id
    ))?
    .downcast::<gst::Pipeline>()
    .map_err(|_| anyhow!("not a pipeline"))?;
    screen.use_clock(None::<&gst::Clock>);
    let sink = screen
        .by_name("frames")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .context("no appsink")?;
    let fit = screen.by_name("fit").context("no capsfilter")?;
    let set_size = move |(w, h): (i32, i32)| {
        let caps = gst::Caps::builder("video/x-raw")
            .field("width", w)
            .field("height", h)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        fit.set_property("caps", &caps);
    };
    let preset = fixed.as_ref().and_then(|f| *f.lock().unwrap());
    if let Some(size) = preset {
        set_size(size);
    }
    let mut to_fix = fixed.filter(|_| preset.is_none());
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |sink| {
                let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(fixed) = &to_fix {
                    // Fix the size (even, for the encoder) and drop this frame:
                    // the next one arrives at that size.
                    let info = sample.caps().and_then(|c| gst_video::VideoInfo::from_caps(c).ok());
                    if let Some(info) = info {
                        let size = ((info.width() & !1) as i32, (info.height() & !1) as i32);
                        *fixed.lock().unwrap() = Some(size);
                        set_size(size);
                        to_fix = None;
                        return Ok(gst::FlowSuccess::Ok);
                    }
                }
                on_frame(sample);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(screen)
}

/// Hands screen frames to the current segment, keeping the latest.
fn forward(target: Target) -> impl Fn(gst::Sample) + Send + Sync + 'static {
    move |sample| {
        let mut sink = target.lock().unwrap();
        sink.push(&sample);
        sink.last = Some(sample);
    }
}

pub struct Recorder {
    // Field order matters for drop: stop watching before tearing down.
    watcher: Option<Watcher>,
    segments: Arc<Mutex<Segments>>,
    /// Runs for the whole session, pulling frames from PipeWire.
    screen: gst::Pipeline,
    /// Kept open for the stream's lifetime; dropping it ends screen sharing.
    portal: Portal,
    /// The window shared on its own, to fall back to cutting out of the
    /// screen if the portal dies; and the frame size kept meanwhile.
    window: Option<(i64, i32)>,
    size: FixedSize,
    _quiet: Option<QuietNotifications>,
    _pointer: Option<BigPointer>,
}

impl Recorder {
    pub fn new(conn: Connection, dir: PathBuf, settings: &Settings) -> Result<Self> {
        let Connection { portal, crop, window, watch } = conn;
        let target: Target = Arc::default();
        let size = FixedSize::default();
        let screen = screen_pipeline(&portal, crop, window.map(|_| size.clone()), forward(target.clone()))?;
        screen.set_state(gst::State::Playing).context("connecting to the screen stream")?;
        let segments = Arc::new(Mutex::new(Segments {
            target,
            dir,
            files: Vec::new(),
            current: None,
            recorded: Duration::ZERO,
            user_paused: false,
            auto_paused: false,
            error: None,
            warning: None,
            camera: settings.record_camera.then(|| settings.camera_device.clone()),
        }));
        Ok(Self {
            watcher: watch.map(|id| Watcher::spawn(id, segments.clone())),
            segments,
            screen,
            portal,
            window,
            size,
            _quiet: QuietNotifications::new(),
            _pointer: settings.big_pointer.then(|| BigPointer::new(settings.pointer_scale)).flatten(),
        })
    }

    pub fn is_recording(&self) -> bool {
        self.segments.lock().unwrap().current.is_some()
    }

    /// Paused because the recorded window is not visible (switch to it to resume).
    pub fn is_waiting_for_window(&self) -> bool {
        let s = self.segments.lock().unwrap();
        s.auto_paused && !s.user_paused
    }

    pub fn elapsed(&self) -> Duration {
        let s = self.segments.lock().unwrap();
        s.recorded + s.current.as_ref().map_or(Duration::ZERO, |(_, t)| t.elapsed())
    }

    /// Whether the camera is being recorded (it may have failed to start).
    pub fn has_camera(&self) -> bool {
        self.segments.lock().unwrap().camera.is_some()
    }

    pub fn pieces(&self) -> usize {
        self.segments.lock().unwrap().files.len()
    }

    pub fn dir(&self) -> PathBuf {
        self.segments.lock().unwrap().dir.clone()
    }

    /// Starts recording, or resumes after Pause.
    pub fn start_segment(&mut self) -> Result<()> {
        let mut s = self.segments.lock().unwrap();
        s.user_paused = false;
        // While the watched window is hidden, recording resumes once it shows.
        if s.auto_paused { Ok(()) } else { s.start() }
    }

    /// Pause.
    pub fn stop_segment(&mut self) -> Result<()> {
        let mut s = self.segments.lock().unwrap();
        s.user_paused = true;
        s.stop()
    }

    /// A problem worth showing that doesn't stop the recording.
    pub fn warning(&self) -> Option<String> {
        let s = self.segments.lock().unwrap();
        let waiting = s.current.as_ref().is_some_and(|(_, started)| started.elapsed() > Duration::from_secs(2));
        if waiting && !s.target.lock().unwrap().delivered {
            return Some(
                "No picture yet: the window hasn't drawn anything since recording started. Apps like Chrome \
                 stop drawing windows you can't see, so bring it into view."
                    .into(),
            );
        }
        s.warning.clone()
    }

    /// Checks for errors (e.g. the user stopped sharing the screen). If the
    /// stream of a window shared on its own fails, the window is recorded cut
    /// out of the screen from then on, with a warning instead of an error.
    pub fn poll_error(&mut self) -> Option<String> {
        let error = |p: &gst::Pipeline| {
            let msg = p.bus()?.pop_filtered(&[gst::MessageType::Error])?;
            match msg.view() {
                gst::MessageView::Error(e) => Some(format!(
                    "{} ({})",
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                )),
                _ => None,
            }
        };
        {
            let mut s = self.segments.lock().unwrap();
            if let Some(e) = s.error.take() {
                return Some(e);
            }
            if let Some(e) = s.current.as_ref().and_then(|(p, _)| error(p)) {
                return Some(e);
            }
        }
        let e = error(&self.screen)?;
        let Some((id, pid)) = self.window.take() else { return Some(e) };
        match self.cut_out_window(id, pid) {
            Ok(()) => {
                self.segments.lock().unwrap().warning = Some(
                    "The screen-share portal stopped (a known portal bug with some apps, e.g. Chrome), so the \
                     window is now recorded cut out of the screen: keep it in place and uncovered."
                        .into(),
                );
                None
            }
            Err(cut) => Some(format!("{e}; recording the window cut out of the screen failed too: {cut:#}")),
        }
    }

    /// Switches from sharing a window on its own to recording it cut out of
    /// the screen, keeping the frame size. The current piece ends and a new
    /// one starts, as after Pause / Resume.
    fn cut_out_window(&mut self, id: i64, pid: i32) -> Result<()> {
        let _ = self.screen.set_state(gst::State::Null);
        let was_recording = self.is_recording();
        self.segments.lock().unwrap().stop()?;
        let conn = connect(Source::Window { id, pid, toplevel: None })?;
        let target = self.segments.lock().unwrap().target.clone();
        let screen = screen_pipeline(&conn.portal, conn.crop, Some(self.size.clone()), forward(target))?;
        screen.set_state(gst::State::Playing).context("connecting to the screen stream")?;
        self.screen = screen;
        self.portal = conn.portal;
        self.watcher = conn.watch.map(|id| Watcher::spawn(id, self.segments.clone()));
        if was_recording {
            self.segments.lock().unwrap().start()?;
        }
        Ok(())
    }

    /// Stops recording and joins the pieces. Returns the final file and the
    /// start time of each piece after the first.
    pub fn finish(mut self) -> Result<(PathBuf, Vec<f64>)> {
        drop(self.watcher.take());
        let stopped = self.segments.lock().unwrap().stop();
        let _ = self.screen.set_state(gst::State::Null);
        let (dir, files) = {
            let s = self.segments.lock().unwrap();
            (s.dir.clone(), s.files.clone())
        };
        drop(self);
        stopped?;
        join_pieces(&dir, files)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Shares one window through scribecut's portal chooser and checks that
    /// every frame keeps the first frame's size while the window is resized.
    /// Needs sway >= 1.12 with the chooser installed. Set
    /// `SCRIBECUT_TEST_TOPLEVEL` to the window's `foreign_toplevel_identifier`,
    /// `SCRIBECUT_TEST_RESIZE` to a sway command that resizes it, and
    /// `SCRIBECUT_TEST_OUT` to a file for the last frame (raw I420). Run with
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn shares_a_single_window() {
        gst::init().unwrap();
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let conn = connect(Source::Window { id: 0, pid: 0, toplevel: Some(var("SCRIBECUT_TEST_TOPLEVEL")) }).unwrap();
        assert!(conn.crop.is_none() && conn.watch.is_none() && conn.window.is_some(), "shared on its own, not cropped");
        let frames: Arc<Mutex<Vec<gst::Sample>>> = Arc::default();
        let seen = frames.clone();
        let fixed = Some(FixedSize::default());
        let screen = screen_pipeline(&conn.portal, None, fixed, move |s| seen.lock().unwrap().push(s)).unwrap();
        screen.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let before = frames.lock().unwrap().len();
        assert!(Command::new("swaymsg").arg(var("SCRIBECUT_TEST_RESIZE")).status().unwrap().success());
        std::thread::sleep(Duration::from_millis(1500));
        screen.set_state(gst::State::Null).unwrap();
        let frames = frames.lock().unwrap();
        let size = |s: &gst::Sample| {
            let info = gst_video::VideoInfo::from_caps(s.caps().unwrap()).unwrap();
            (info.width(), info.height())
        };
        let first = size(&frames[0]);
        eprintln!("{} frames ({before} before the resize), all {first:?}", frames.len());
        assert!(before > 10 && frames.len() > before + 10, "frames keep coming");
        assert!(frames.iter().all(|f| size(f) == first), "one size throughout");
        let last = frames.last().unwrap().buffer().unwrap().map_readable().unwrap();
        std::fs::write(var("SCRIBECUT_TEST_OUT"), last.as_slice()).unwrap();
    }

    /// Shares the whole screen without the portal asking which monitor (one
    /// monitor connected, or scribecut's chooser installed). Only counts
    /// frames. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn shares_the_monitor_without_asking() {
        gst::init().unwrap();
        let started = Instant::now();
        let conn = connect(Source::Monitor(None)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "the portal asked");
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let screen = screen_pipeline(&conn.portal, None, None, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        })
        .unwrap();
        screen.set_state(gst::State::Playing).unwrap();
        std::thread::sleep(Duration::from_secs(1));
        screen.set_state(gst::State::Null).unwrap();
        assert!(count.load(std::sync::atomic::Ordering::Relaxed) > 0, "no frames");
    }

    /// Records a window shared on its own for a few seconds and kills the
    /// portal two seconds in: the recording must carry on cut out of the
    /// screen, with a warning, and come out at one size. Set
    /// `SCRIBECUT_TEST_TOPLEVEL`, `SCRIBECUT_TEST_CON` (its sway container id)
    /// and `SCRIBECUT_TEST_OUT` (an empty folder). Records the microphone and
    /// restarts xdg-desktop-portal-wlr. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn carries_on_when_the_portal_dies() {
        gst::init().unwrap();
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let id: i64 = var("SCRIBECUT_TEST_CON").parse().unwrap();
        let pid = platform::windows().unwrap().into_iter().find(|w| w.id == id).unwrap().pid;
        let conn = connect(Source::Window { id, pid, toplevel: Some(var("SCRIBECUT_TEST_TOPLEVEL")) }).unwrap();
        let settings = Settings { record_camera: false, big_pointer: false, ..Settings::default() };
        let mut rec = Recorder::new(conn, PathBuf::from(var("SCRIBECUT_TEST_OUT")), &settings).unwrap();
        rec.start_segment().unwrap();
        for i in 0..80 {
            if i == 20 && std::env::var_os("SCRIBECUT_TEST_KEEP_PORTAL").is_none() {
                let kill = ["--user", "kill", "-s", "KILL", "xdg-desktop-portal-wlr"];
                assert!(Command::new("systemctl").args(kill).status().unwrap().success());
            }
            assert_eq!(rec.poll_error(), None);
            std::thread::sleep(Duration::from_millis(100));
        }
        let warning = rec.warning();
        let pieces = rec.pieces();
        let (file, _) = rec.finish().unwrap();
        eprintln!("{pieces} pieces, warning: {warning:?}");
        assert!(warning.is_some_and(|w| w.contains("cut out of the screen")), "fell back");
        let info = crate::media::probe(&file).unwrap();
        eprintln!("{:.1}s at {}x{}", info.duration, info.width, info.height);
        assert!(info.duration > 6.0, "kept recording: {}", info.duration);
    }
}
