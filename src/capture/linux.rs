//! Linux capture: screen frames come from the xdg-desktop-portal ScreenCast
//! portal over PipeWire, encoded with x264 and recorded to Matroska segments.

use std::os::fd::{IntoRawFd, OwnedFd};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use anyhow::{Context, Result, anyhow, bail};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::enumflags2::BitFlags;
use gst::prelude::*;

use super::Source;
use crate::geom::{Crop, Output};
use crate::platform;

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
pub struct Target {
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

impl Target {
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
                    let persist = if source == SourceType::Monitor {
                        PersistMode::ExplicitlyRevoked
                    } else {
                        PersistMode::DoNot
                    };
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
        let target =
            Self { node_id: o.node_id, fd: o.fd, position: o.position, size: o.size, close: Some(close_tx) };
        Ok((target, o.restore_token))
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

impl Drop for Target {
    fn drop(&mut self) {
        if let Some(tx) = self.close.take() {
            let _ = tx.send(());
        }
    }
}

/// Whether the portal can share individual windows (not on sway 1.11).
pub fn supports_system_windows() -> bool {
    runtime().block_on(async {
        let Ok(proxy) = Screencast::new().await else { return false };
        proxy.available_source_types().await.is_ok_and(|t| t.contains(SourceType::Window))
    })
}

/// Opens the screen stream for `source`. Blocks on the portal dialog.
pub fn connect(source: Source) -> Result<(Target, Option<Crop>)> {
    match source {
        // The portal runs its own monitor picker, so a pre-chosen output is ignored.
        Source::Monitor(_) => Ok((Target::open(SourceType::Monitor, None)?.0, None)),
        Source::SystemWindow => Ok((Target::open(SourceType::Window, None)?.0, None)),
        Source::Area(area) => {
            let outputs = platform::outputs()?;
            let output = platform::output_for(&area, &outputs).context("that area is not on any monitor")?;
            // Reuse the last monitor choice so the user isn't asked again; ask
            // if it was a different monitor than the one the area is on.
            let token_file = dirs::data_dir().map(|d| d.join("scribecut/portal_token"));
            let saved = token_file.as_ref().and_then(|f| std::fs::read_to_string(f).ok());
            let mut attempt = Target::open(SourceType::Monitor, saved)?;
            if !attempt.0.shows(&output) {
                drop(attempt);
                attempt = Target::open(SourceType::Monitor, None)?;
                if !attempt.0.shows(&output) {
                    bail!("please pick monitor {} in the dialog - the area is on it", output.name);
                }
            }
            let (target, token) = attempt;
            if let (Some(file), Some(token)) = (token_file, token) {
                let _ = std::fs::create_dir_all(file.parent().unwrap());
                let _ = std::fs::write(file, token);
            }
            let crop = platform::crop_for(&area, &output);
            Ok((target, (!crop.is_none()).then_some(crop)))
        }
    }
}

/// Screen frames → H.264 (frame rate made constant, since the compositor only
/// sends frames when something on screen changes).
const VIDEO_ENCODE: &str = "videorate ! video/x-raw,framerate=30/1 \
    ! queue max-size-time=2000000000 max-size-buffers=0 max-size-bytes=0 \
    ! x264enc tune=zerolatency speed-preset=veryfast bitrate=8000 key-int-max=60 ! h264parse";

type Sink = Arc<Mutex<Option<gst_app::AppSrc>>>;

pub struct Capture {
    _target: Target,
    /// Runs for the whole session, pulling frames from PipeWire.
    screen: gst::Pipeline,
    /// The appsrc of the segment being recorded, if any; frames go there.
    sink: Sink,
    current: Option<gst::Pipeline>,
}

impl Capture {
    pub fn new(target: Target, crop: Option<Crop>) -> Result<Self> {
        // pipewiresrc's own timestamps don't line up with the pipeline clock, so
        // run it unsynchronised and let each segment's appsrc stamp frames on arrival.
        let fd = target.fd.try_clone()?.into_raw_fd();
        let crop = crop
            .map(|c| {
                format!("! videocrop left={} top={} right={} bottom={} ", c.left, c.top, c.right, c.bottom)
            })
            .unwrap_or_default();
        let screen = gst::parse::launch(&format!(
            "pipewiresrc fd={fd} path={} {crop}! videoconvert ! video/x-raw,format=I420 \
             ! appsink name=frames sync=false max-buffers=4 drop=true",
            target.node_id
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        screen.use_clock(None::<&gst::Clock>);
        let appsink = screen
            .by_name("frames")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .context("no appsink")?;
        let sink: Sink = Arc::default();
        let forward = sink.clone();
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
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
        Ok(Self { _target: target, screen, sink, current: None })
    }

    pub fn start(&mut self, path: &Path) -> Result<()> {
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
        *self.sink.lock().unwrap() = Some(appsrc);
        self.current = Some(pipeline);
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let Some(pipeline) = self.current.take() else { return Ok(()) };
        self.sink.lock().unwrap().take();
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

    pub fn poll_error(&mut self) -> Option<String> {
        [Some(&self.screen), self.current.as_ref()].into_iter().flatten().find_map(|p| {
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

    pub fn shutdown(&mut self) {
        let _ = self.screen.set_state(gst::State::Null);
    }
}
