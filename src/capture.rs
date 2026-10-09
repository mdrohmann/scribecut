//! Screen + microphone recording. Screen frames come from the xdg-desktop-portal
//! ScreenCast portal (PipeWire); each record/resume span is written to its own
//! segment file, which are concatenated when recording stops.

use std::os::fd::{IntoRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::enumflags2::BitFlags;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use gst::prelude::*;

use crate::sway;

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
    fn shows(&self, output: &sway::Output) -> bool {
        let a = output.area;
        match (self.position, self.size) {
            (Some(p), _) => p == (a.x, a.y),
            (None, Some(s)) => s == (a.w, a.h),
            (None, None) => true,
        }
    }
}

/// What to record.
pub enum Source {
    /// A whole monitor, chosen in the portal's picker.
    Monitor,
    /// A single window, chosen in the portal's picker (desktops that support it).
    PortalWindow,
    /// A rectangle of the screen (a region, or a window's area on sway),
    /// recorded by cropping the monitor it is on.
    Area(sway::Area),
}

/// Whether the portal can share individual windows (not on sway 1.11).
pub fn portal_supports_windows() -> bool {
    runtime().block_on(async {
        let Ok(proxy) = Screencast::new().await else { return false };
        proxy.available_source_types().await.is_ok_and(|t| t.contains(SourceType::Window))
    })
}

/// Opens the screen stream for `source`. Blocks on the portal dialog.
pub fn connect(source: Source) -> Result<(Portal, Option<sway::Crop>)> {
    match source {
        Source::Monitor => Ok((Portal::open(SourceType::Monitor, None)?.0, None)),
        Source::PortalWindow => Ok((Portal::open(SourceType::Window, None)?.0, None)),
        Source::Area(area) => {
            let outputs = sway::outputs()?;
            let output = sway::output_for(&area, &outputs).context("that area is not on any monitor")?;
            // Reuse the last monitor choice so the user isn't asked again; ask
            // if it was a different monitor than the one the area is on.
            let token_file = dirs::data_dir().map(|d| d.join("scribecut/portal_token"));
            let saved = token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
            let mut attempt = Portal::open(SourceType::Monitor, saved)?;
            if !attempt.0.shows(&output) {
                drop(attempt);
                attempt = Portal::open(SourceType::Monitor, None)?;
                if !attempt.0.shows(&output) {
                    bail!("please pick monitor {} in the dialog - the area is on it", output.name);
                }
            }
            let (portal, token) = attempt;
            if let (Some(file), Some(token)) = (token_file, token) {
                let _ = std::fs::create_dir_all(file.parent().unwrap());
                let _ = std::fs::write(file, token);
            }
            let crop = sway::crop_for(&area, &output);
            let none = sway::Crop { left: 0, top: 0, right: 0, bottom: 0 };
            Ok((portal, (crop != none).then_some(crop)))
        }
    }
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

type Target = Arc<Mutex<Option<gst_app::AppSrc>>>;

pub struct Recorder {
    portal: Portal,
    /// Runs for the whole session, pulling frames from PipeWire.
    screen: gst::Pipeline,
    /// The appsrc of the segment being recorded, if any; frames go there.
    target: Target,
    pub dir: PathBuf,
    pub segments: Vec<PathBuf>,
    current: Option<(gst::Pipeline, Instant)>,
    recorded: Duration,
}

impl Recorder {
    pub fn new(portal: Portal, crop: Option<sway::Crop>, dir: PathBuf) -> Result<Self> {
        // pipewiresrc's own timestamps don't line up with the pipeline clock, so
        // run it unsynchronised and let each segment's appsrc stamp frames on arrival.
        let fd = portal.fd.try_clone()?.into_raw_fd();
        let crop = crop
            .map(|c| format!("! videocrop left={} top={} right={} bottom={} ", c.left, c.top, c.right, c.bottom))
            .unwrap_or_default();
        let screen = gst::parse::launch(&format!(
            "pipewiresrc fd={fd} path={} {crop}! videoconvert ! video/x-raw,format=I420 \
             ! appsink name=frames sync=false max-buffers=4 drop=true",
            portal.node_id
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        screen.use_clock(None::<&gst::Clock>);
        let sink = screen
            .by_name("frames")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("no appsink")?;
        let target: Target = Arc::default();
        let forward = target.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(src) = forward.lock().unwrap().as_ref()
                        && let (Some(caps), Some(buffer)) = (sample.caps(), sample.buffer_owned())
                    {
                        let caps = caps.to_owned();
                        if src.caps().as_ref() != Some(&caps) {
                            src.set_caps(Some(&caps));
                        }
                        let mut buffer = buffer;
                        let b = buffer.make_mut();
                        b.set_pts(gst::ClockTime::NONE);
                        b.set_dts(gst::ClockTime::NONE);
                        b.set_duration(gst::ClockTime::NONE);
                        let _ = src.push_buffer(buffer);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        screen.set_state(gst::State::Playing).context("connecting to the screen stream")?;
        Ok(Self {
            portal,
            screen,
            target,
            dir,
            segments: Vec::new(),
            current: None,
            recorded: Duration::ZERO,
        })
    }

    pub fn is_recording(&self) -> bool {
        self.current.is_some()
    }

    pub fn elapsed(&self) -> Duration {
        self.recorded + self.current.as_ref().map_or(Duration::ZERO, |(_, t)| t.elapsed())
    }

    /// Starts (or resumes into) a new segment file.
    pub fn start_segment(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        let path = self.dir.join(format!("segment_{:03}.mkv", self.segments.len()));
        let desc = format!(
            "appsrc name=screen is-live=true do-timestamp=true format=time ! videoconvert ! {VIDEO_ENCODE} ! queue ! mux. \
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
        pipeline.set_state(gst::State::Playing).context("starting recording")?;
        *self.target.lock().unwrap() = Some(appsrc);
        self.segments.push(path);
        self.current = Some((pipeline, Instant::now()));
        Ok(())
    }

    /// Finishes the current segment (pause). Safe to call when already paused.
    pub fn stop_segment(&mut self) -> Result<()> {
        let Some((pipeline, started)) = self.current.take() else { return Ok(()) };
        self.recorded += started.elapsed();
        self.target.lock().unwrap().take();
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

    /// Checks for errors (e.g. the user stopped sharing the screen).
    pub fn poll_error(&self) -> Option<String> {
        let current = self.current.as_ref().map(|(p, _)| p);
        [Some(&self.screen), current].into_iter().flatten().find_map(|p| {
            let msg = p.bus()?.pop_filtered(&[gst::MessageType::Error])?;
            match msg.view() {
                gst::MessageView::Error(e) => Some(format!(
                    "{} ({})",
                    e.error(),
                    e.debug().map(|d| d.to_string()).unwrap_or_default()
                )),
                _ => None,
            }
        })
    }

    /// Stops recording and joins the pieces. Returns the final file and the
    /// start time of each piece after the first.
    pub fn finish(mut self) -> Result<(PathBuf, Vec<f64>)> {
        let stopped = self.stop_segment();
        let _ = self.screen.set_state(gst::State::Null);
        drop(self.portal);
        stopped?;
        let segments: Vec<PathBuf> = self.segments.into_iter().filter(|s| non_empty(s)).collect();
        if segments.is_empty() {
            bail!("nothing was recorded");
        }
        let mut starts = Vec::new();
        let mut t = 0.0;
        for s in &segments[..segments.len() - 1] {
            t += crate::media::probe(s)?.duration;
            starts.push(t);
        }
        let out = self.dir.join("recording.mkv");
        if segments.len() == 1 {
            std::fs::rename(&segments[0], &out)?;
        } else {
            crate::media::concat(&segments, &out)?;
            for s in &segments {
                let _ = std::fs::remove_file(s);
            }
        }
        Ok((out, starts))
    }
}

fn non_empty(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.len() > 1024)
}

/// A fresh, timestamped folder for a new recording.
pub fn new_recording_dir() -> Result<PathBuf> {
    let base = dirs::video_dir().or_else(dirs::home_dir).context("no home dir")?;
    let dir = base.join("scribecut").join(chrono::Local::now().format("%Y-%m-%d_%H-%M-%S").to_string());
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}
