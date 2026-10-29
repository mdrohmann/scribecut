//! macOS capture. AVFoundation only hands out whole displays, so there is no
//! portal session to hold open: each segment is one ffmpeg process grabbing the
//! display (and optionally the camera) and encoding H.264 in hardware with
//! VideoToolbox. Cropping to a window or region happens in that same process.
//!
//! The microphone does **not** come from ffmpeg. Its AVFoundation audio input
//! drops 8 to 22 per cent of its samples even with nothing else running --
//! measured at 4 per cent of one core, writing uncompressed, with no screen
//! capture in the process -- and the demuxer exposes nothing to tune. Padding
//! the gaps afterwards only buys the right duration around missing speech. So
//! the microphone is recorded through GStreamer's CoreAudio source, which
//! captured everything in the same test, and muxed with the picture when the
//! segment ends.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;

use super::{Source, join_pieces};
use crate::geom::{Crop, Output};
use crate::platform;
use crate::settings::Settings;

/// The display to record, how to crop it, and what to watch.
pub struct Connection {
    /// ffmpeg's AVFoundation video device index for this display.
    screen: usize,
    /// Whether there is a microphone to record at all.
    has_audio: bool,
    /// The display's full frame size in pixels, which a crop is measured against.
    pixels: (i32, i32),
    crop: Option<Crop>,
    name: String,
    /// Window whose visibility pauses and resumes the recording.
    watch: Option<i64>,
}

/// macOS has no system-provided single-window picker: scribecut lists the
/// windows itself and crops the display.
pub fn supports_system_windows() -> bool {
    false
}

/// The AVFoundation devices ffmpeg can see.
#[derive(Default, Debug)]
struct Devices {
    /// Device indices named "Capture screen N", in display order.
    screens: Vec<usize>,
    /// Video devices that are not screens, i.e. cameras, with their names.
    cameras: Vec<(usize, String)>,
    /// Audio input device indices. Only their presence matters: the microphone
    /// is captured by GStreamer, which uses the system's default input.
    audio: Vec<usize>,
}

/// Asks ffmpeg which AVFoundation devices exist.
///
/// This is also the first call that touches the capture APIs, so it is where
/// macOS puts up its Screen Recording permission prompt.
fn devices() -> Result<Devices> {
    let out = crate::media::ffmpeg()
        .stdin(Stdio::null())
        .args(["-hide_banner", "-f", "avfoundation", "-list_devices", "true", "-i", ""])
        .output()
        .context("running ffmpeg (is it installed?)")?;
    // Listing devices always "fails": there is no input to open afterwards, so
    // only the text it printed matters.
    let text = String::from_utf8_lossy(&out.stderr);
    let devices = parse_devices(&text);
    if devices.screens.is_empty() {
        bail!(
            "ffmpeg found no screen capture device. Grant Screen Recording permission \
             in System Settings > Privacy & Security > Screen & System Audio Recording, \
             and check the display is awake, then restart scribecut.\n{}",
            text.trim()
        );
    }
    Ok(devices)
}

/// Pulls the device indices out of ffmpeg's `-list_devices` output, which looks
/// like `[AVFoundation indev @ 0x...] [1] Capture screen 0`.
fn parse_devices(text: &str) -> Devices {
    #[derive(PartialEq)]
    enum Section {
        None,
        Video,
        Audio,
    }
    let mut section = Section::None;
    let mut devices = Devices::default();
    for line in text.lines() {
        // Drop ffmpeg's `[AVFoundation indev @ 0x...] ` log prefix.
        let line = line.split_once("] ").map_or(line, |(_, rest)| rest).trim();
        if line.ends_with("video devices:") || line.ends_with("screen capture devices:") {
            section = Section::Video;
            continue;
        }
        if line.ends_with("audio devices:") {
            section = Section::Audio;
            continue;
        }
        // `[<index>] <name>`
        let Some((index, name)) = line.strip_prefix('[').and_then(|l| l.split_once(']')) else { continue };
        let Ok(index) = index.trim().parse::<usize>() else { continue };
        let name = name.trim();
        match section {
            Section::Video if name.starts_with("Capture screen") => devices.screens.push(index),
            Section::Video => devices.cameras.push((index, name.to_string())),
            Section::Audio => devices.audio.push(index),
            Section::None => {}
        }
    }
    devices
}

/// AVFoundation lists its screen devices in the same order as CoreGraphics
/// lists active displays, so position in [`platform::outputs`] is the index.
fn screen_device(devices: &Devices, outputs: &[Output], output: &Output) -> Result<usize> {
    let position = outputs.iter().position(|o| o.name == output.name).unwrap_or(0);
    devices
        .screens
        .get(position)
        .copied()
        .or_else(|| devices.screens.first().copied())
        .context("no screen capture device")
}

/// The camera a fresh install should use: the first one AVFoundation lists.
///
/// Stored by name rather than by number, because AVFoundation's indices shift
/// as devices come and go while the name stays put.
pub fn default_camera() -> String {
    devices().ok().and_then(|d| d.cameras.first().map(|(_, name)| name.clone())).unwrap_or_default()
}

/// Whether `device` is a camera ffmpeg can currently see.
pub fn camera_available(device: &str) -> bool {
    !device.trim().is_empty() && devices().is_ok_and(|d| find_camera(&d, device).is_some())
}

/// Resolves a configured camera to an AVFoundation index: an exact name, a bare
/// index, or part of a name.
fn find_camera(devices: &Devices, device: &str) -> Option<usize> {
    let wanted = device.trim();
    if wanted.is_empty() {
        return None;
    }
    if let Some((index, _)) = devices.cameras.iter().find(|(_, name)| name == wanted) {
        return Some(*index);
    }
    if let Ok(index) = wanted.parse::<usize>()
        && devices.cameras.iter().any(|(i, _)| *i == index)
    {
        return Some(index);
    }
    devices
        .cameras
        .iter()
        .find(|(_, name)| name.to_lowercase().contains(&wanted.to_lowercase()))
        .map(|(index, _)| *index)
}

/// Picks the display to record and works out the crop. Unlike Linux there is no
/// system dialog here, so nothing blocks on the user.
pub fn connect(source: Source) -> Result<Connection> {
    let devices = devices()?;
    let has_audio = !devices.audio.is_empty();
    let outputs = platform::outputs()?;
    let (output, crop, watch) = match source {
        Source::Monitor(chosen) => {
            let output = chosen.or_else(|| outputs.first().cloned()).context("no displays")?;
            (output, None, None)
        }
        Source::SystemWindow => bail!("macOS has no system window picker; pick the window from the list"),
        Source::Window { id, pid, .. } => {
            // Bring it to the front, then record its area, watching it so the
            // recording pauses whenever it stops being visible.
            let area = platform::focus_window(id, pid)?;
            let output = platform::output_for(&area, &outputs).context("that window is not on any display")?;
            let crop = platform::crop_for(&area, &output);
            (output, (!crop.is_none()).then_some(crop), Some(id))
        }
        Source::Area(area) => {
            let output = platform::output_for(&area, &outputs).context("that area is not on any display")?;
            let crop = platform::crop_for(&area, &output);
            (output, (!crop.is_none()).then_some(crop), None)
        }
    };
    let screen = screen_device(&devices, &outputs, &output)?;
    Ok(Connection { screen, has_audio, pixels: output.pixels, crop, name: output.name, watch })
}

/// H.264 encoder for the recording. VideoToolbox encodes in hardware, which
/// keeps the CPU free while the screen is being captured; set
/// `SCRIBECUT_ENCODER=libx264` (and a lower bitrate) for software encoding.
fn encoder() -> String {
    std::env::var("SCRIBECUT_ENCODER").unwrap_or_else(|_| "h264_videotoolbox".into())
}

/// Recording bitrate for the screen. Hardware H.264 needs a generous one to
/// keep screen text crisp; override with `SCRIBECUT_BITRATE`.
fn bitrate() -> String {
    std::env::var("SCRIBECUT_BITRATE").unwrap_or_else(|_| "12M".into())
}

/// How long a segment may stay empty before it counts as stalled. Generous:
/// the device takes a moment to warm up and the first keyframe to be written.
const STALL_AFTER: Duration = Duration::from_secs(8);

/// Microphone capture through GStreamer's CoreAudio source, written to its own
/// uncompressed file and muxed with the picture when the segment ends.
struct Microphone {
    pipeline: gst::Pipeline,
    path: PathBuf,
}

impl Microphone {
    fn start(path: &Path) -> Result<Self> {
        let pipeline = gst::parse::launch(&format!(
            "osxaudiosrc name=mic ! audioconvert ! audioresample \
             ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! wavenc \
             ! filesink location=\"{}\"",
            path.display()
        ))
        .context("building the microphone pipeline")?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        pipeline.set_state(gst::State::Playing).context("starting the microphone")?;
        Ok(Self { pipeline, path: path.to_path_buf() })
    }

    /// Any error the pipeline has reported.
    fn error(&self) -> Option<String> {
        let msg = self.pipeline.bus()?.pop_filtered(&[gst::MessageType::Error])?;
        match msg.view() {
            gst::MessageView::Error(e) => Some(format!("microphone: {}", e.error())),
            _ => None,
        }
    }

    /// Finishes the file. `wavenc` has to go back and write the data length, so
    /// this waits for the pipeline to see the end of the stream.
    fn stop(self) -> Result<PathBuf> {
        self.pipeline.send_event(gst::event::Eos::new());
        if let Some(bus) = self.pipeline.bus() {
            bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            );
        }
        let _ = self.pipeline.set_state(gst::State::Null);
        Ok(self.path)
    }
}

/// The processes recording the current segment.
struct Running {
    /// ffmpeg, recording the screen and optionally the camera.
    child: Child,
    /// Where ffmpeg writes; without a microphone this is the segment itself.
    video: PathBuf,
    /// The microphone, recorded separately.
    mic: Option<Microphone>,
    /// Where the finished segment goes once the two are muxed.
    segment: PathBuf,
    stderr: Arc<Mutex<String>>,
    /// When ffmpeg was spawned, to notice a segment that never grows.
    started: Instant,
}

/// Recording state shared between the UI and the window watcher.
struct Segments {
    screen: usize,
    has_audio: bool,
    pixels: (i32, i32),
    crop: Option<Crop>,
    display: String,
    dir: PathBuf,
    files: Vec<PathBuf>,
    current: Option<Running>,
    recorded: Duration,
    /// Paused with the Pause button (the watcher must not resume then).
    user_paused: bool,
    /// Paused because the watched window isn't visible.
    auto_paused: bool,
    error: Option<String>,
    /// Shown to the user without stopping the recording.
    warning: Option<String>,
    /// Camera device index recorded as a second video track, if any.
    camera: Option<usize>,
}

impl Segments {
    /// Starts (or resumes into) a new segment file.
    fn start(&mut self) -> Result<()> {
        if self.current.is_some() {
            return Ok(());
        }
        let index = self.files.len();
        let segment = self.dir.join(format!("segment_{index:03}.mkv"));
        let running = match self.spawn(index, &segment, self.camera) {
            Ok(running) => running,
            Err(e) if self.camera.is_some() => {
                // Busy or missing camera: keep recording the screen.
                self.warning = Some(format!("Camera unavailable, recording without it: {e:#}"));
                self.camera = None;
                self.spawn(index, &segment, None)?
            }
            Err(e) => return Err(e),
        };
        self.files.push(segment);
        self.current = Some(running);
        Ok(())
    }

    /// Starts both halves of a segment: ffmpeg for the picture, GStreamer for
    /// the microphone.
    fn spawn(&self, index: usize, segment: &Path, camera: Option<usize>) -> Result<Running> {
        // With a microphone the picture goes to its own file first, because the
        // two are muxed together afterwards.
        let video =
            if self.has_audio { self.dir.join(format!("segment_{index:03}.video.mkv")) } else { segment.to_path_buf() };

        let mut cmd = crate::media::ffmpeg();
        cmd.args(["-hide_banner", "-v", "error", "-nostats"]);
        // The display. Two things are deliberate here. No input `-framerate`:
        // AVFoundation screen devices advertise no frame rates, so asking for
        // one makes ffmpeg refuse the device, and the output `-r` below makes
        // the result constant rate anyway. And `nv12`, which is both what the
        // screen device offers and what VideoToolbox encodes natively, so
        // nothing converts pixels in between; left to itself ffmpeg asks for
        // yuv420p, which the device rejects before falling back.
        cmd.args(["-f", "avfoundation", "-capture_cursor", "1", "-pixel_format", "nv12"])
            .args(["-i", &self.screen.to_string()]);
        let camera_input = camera.map(|device| {
            cmd.args(["-f", "avfoundation", "-framerate", "30"]).args(["-i", &device.to_string()]);
            1usize
        });

        // The screen track comes first so it is the file's first video track,
        // which is what the editor and the export expect.
        match self.crop {
            Some(crop) => {
                let (w, h) = crop.size(self.pixels);
                cmd.args(["-filter_complex", &format!("[0:v]crop={w}:{h}:{}:{}[screen]", crop.left, crop.top)])
                    .args(["-map", "[screen]"]);
            }
            None => {
                cmd.args(["-map", "0:v"]);
            }
        }
        if let Some(input) = camera_input {
            cmd.args(["-map", &format!("{input}:v")]);
        }
        // No `-pix_fmt`: ffmpeg converts if the encoder needs it, and asking
        // for one here would override the native nv12 path above.
        cmd.args(["-r", "30"]).args(["-c:v", &encoder(), "-g", "60"]);
        cmd.args(["-b:v:0", &bitrate()]);
        if camera_input.is_some() {
            // The camera ends up as a small inset, so it needs far less than
            // the screen does.
            cmd.args(["-b:v:1", "3M"]);
        }
        cmd.args(["-f", "matroska"]).arg(&video);
        // stdin stays open: `q` on it is how ffmpeg is asked to finish the file.
        // It is a pipe, never the terminal, so a backgrounded scribecut is not
        // stopped by SIGTTOU.
        cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());

        let mut child = cmd.spawn().context("running ffmpeg (is it installed?)")?;
        // Drain stderr so a chatty ffmpeg can never block on a full pipe.
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let collected = stderr.clone();
            std::thread::spawn(move || {
                let mut text = String::new();
                let _ = pipe.read_to_string(&mut text);
                collected.lock().unwrap().push_str(&text);
            });
        }

        let mic = if self.has_audio {
            match Microphone::start(&self.dir.join(format!("segment_{index:03}.wav"))) {
                Ok(mic) => Some(mic),
                Err(e) => {
                    // Better a silent recording than none.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(e);
                }
            }
        } else {
            None
        };

        let mut running =
            Running { child, video, mic, segment: segment.to_path_buf(), stderr, started: Instant::now() };
        // A camera another app is holding makes ffmpeg exit almost at once, so
        // give it a moment and report that as the failure the caller falls back
        // on. Without the camera there is nothing to fall back to, so the
        // ordinary error paths handle it instead of delaying every start.
        if camera.is_some() {
            std::thread::sleep(Duration::from_millis(400));
            if matches!(running.child.try_wait(), Ok(Some(_))) {
                let message = tail(&running.stderr);
                if let Some(mic) = running.mic.take() {
                    let _ = mic.stop();
                }
                bail!("{}", if message.is_empty() { "ffmpeg stopped".to_string() } else { message });
            }
        }
        Ok(running)
    }

    /// Finishes the current segment. Safe to call when already stopped.
    fn stop(&mut self) -> Result<()> {
        let Some(mut run) = self.current.take() else { return Ok(()) };
        self.recorded += run.started.elapsed();
        // `q` makes ffmpeg finalise the Matroska file; killing it would leave
        // the file without its index, unseekable and often unreadable.
        if let Some(stdin) = run.child.stdin.as_mut() {
            let _ = stdin.write_all(b"q");
            let _ = stdin.flush();
        }
        drop(run.child.stdin.take());
        let exited = wait_for(&mut run.child, Duration::from_secs(10))?;
        if !exited {
            let _ = run.child.kill();
            let _ = run.child.wait();
        }

        let mic = match run.mic.take() {
            Some(mic) => Some(mic.stop()?),
            None => None,
        };

        if !exited {
            bail!("timed out finalising segment");
        }
        // ffmpeg's exit code after `q` is not meaningful, so judge the segment
        // by whether it actually wrote a file.
        if file_len(&run.video) <= 1024 {
            bail!("recording failed: {}", tail(&run.stderr));
        }
        if let Some(mic) = mic {
            // Both halves are stopped together, so however much more sound
            // there is than picture was captured before the first frame: the
            // microphone opens in a moment while the screen device takes a
            // second or two. Measuring the two finished files is steadier than
            // timing the starts, because Matroska buffers and the picture's
            // file does not grow the instant frames begin arriving.
            let video_len = crate::media::probe(&run.video).map(|i| i.duration).unwrap_or(0.0);
            let audio_len = crate::media::audio_duration(&mic).unwrap_or(0.0);
            let lead = (audio_len - video_len).clamp(0.0, 5.0);
            mux(&run.video, &mic, lead, &run.segment)?;
            let _ = std::fs::remove_file(&run.video);
            let _ = std::fs::remove_file(&mic);
        }
        Ok(())
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

    /// Whichever problem the recording has run into, if any.
    fn poll_error(&mut self) -> Option<String> {
        if let Some(e) = self.error.take() {
            return Some(e);
        }
        let display = self.display.clone();
        let run = self.current.as_mut()?;
        if let Some(e) = run.mic.as_ref().and_then(Microphone::error) {
            return Some(e);
        }
        // Still running is the normal case; anything else means ffmpeg gave up
        // (screen sharing revoked, disk full, device taken away).
        if let Ok(Some(status)) = run.child.try_wait() {
            let message = tail(&run.stderr);
            self.current = None;
            return Some(if message.is_empty() { format!("ffmpeg stopped ({status})") } else { message });
        }
        // A missing permission makes AVFoundation block instead of fail, which
        // would otherwise leave a running timer above a file that never grows.
        if run.started.elapsed() > STALL_AFTER && file_len(&run.video) == 0 {
            return Some(format!(
                "no frames have arrived from {display}. Grant Screen Recording (and Microphone) \
                 permission in System Settings > Privacy & Security, then restart scribecut - \
                 see the macOS notes in the README"
            ));
        }
        None
    }
}

/// Puts the picture and the microphone into one file.
///
/// `lead` is how many seconds of sound were captured before the first frame;
/// that much is dropped from the front so the two line up.
fn mux(video: &Path, audio: &Path, lead: f64, out: &Path) -> Result<()> {
    let mut cmd = crate::media::ffmpeg();
    cmd.stdin(Stdio::null()).args(["-nostdin", "-y", "-v", "error", "-i"]).arg(video);
    if lead > 0.01 {
        cmd.args(["-ss", &format!("{lead:.3}")]);
    }
    cmd.args(["-i"])
        .arg(audio)
        // `0:v` takes every video track, so a camera track comes along too.
        .args(["-map", "0:v", "-map", "1:a", "-c:v", "copy", "-c:a", "aac", "-b:a", "160k"])
        .args(["-f", "matroska"])
        .arg(out);
    let out_result = cmd.output().context("running ffmpeg to mux the segment")?;
    if !out_result.status.success() {
        bail!("could not mux the segment: {}", String::from_utf8_lossy(&out_result.stderr));
    }
    Ok(())
}

/// Polls the recorded window's visibility until dropped.
struct Watcher {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn spawn(id: i64, segments: Arc<Mutex<Segments>>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                // A closed window counts as hidden; errors are ignored.
                if let Ok(visible) = platform::window_visible(id) {
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
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Recorder {
    // Field order matters for drop: stop watching before tearing down.
    watcher: Option<Watcher>,
    segments: Arc<Mutex<Segments>>,
}

impl Recorder {
    pub fn new(conn: Connection, dir: PathBuf, settings: &Settings) -> Result<Self> {
        let Connection { screen, has_audio, pixels, crop, name, watch } = conn;
        let camera = settings
            .record_camera
            .then(|| devices().ok().and_then(|d| find_camera(&d, &settings.camera_device)))
            .flatten();
        let segments = Arc::new(Mutex::new(Segments {
            screen,
            has_audio,
            pixels,
            crop,
            display: name,
            dir,
            files: Vec::new(),
            current: None,
            recorded: Duration::ZERO,
            user_paused: false,
            auto_paused: false,
            error: None,
            warning: None,
            camera,
        }));
        Ok(Self { watcher: watch.map(|id| Watcher::spawn(id, segments.clone())), segments })
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
        s.recorded + s.current.as_ref().map_or(Duration::ZERO, |r| r.started.elapsed())
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
        self.segments.lock().unwrap().warning.clone()
    }

    /// Checks for errors (e.g. the user revoked screen recording).
    pub fn poll_error(&self) -> Option<String> {
        self.segments.lock().unwrap().poll_error()
    }

    /// Stops recording and joins the pieces. Returns the final file and the
    /// start time of each piece after the first.
    pub fn finish(mut self) -> Result<(PathBuf, Vec<f64>)> {
        drop(self.watcher.take());
        let (stopped, dir, files) = {
            let mut s = self.segments.lock().unwrap();
            let stopped = s.stop();
            (stopped, s.dir.clone(), s.files.clone())
        };
        stopped?;
        join_pieces(&dir, files)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        drop(self.watcher.take());
        // Never leave an ffmpeg holding the camera or the screen, or a
        // microphone pipeline running.
        if let Ok(mut s) = self.segments.lock()
            && let Some(mut run) = s.current.take()
        {
            let _ = run.child.kill();
            let _ = run.child.wait();
            if let Some(mic) = run.mic.take() {
                let _ = mic.stop();
            }
        }
    }
}

/// The segment's size so far, treating "not created yet" as empty.
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Waits up to `limit` for the child to exit. `false` if it is still running.
fn wait_for(child: &mut Child, limit: Duration) -> Result<bool> {
    let deadline = Instant::now() + limit;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last few lines of ffmpeg's complaints, for an error message.
fn tail(stderr: &Arc<Mutex<String>>) -> String {
    let text = stderr.lock().unwrap();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        // AVFoundation's `objc[123]: class ... not linked` notes are printed by
        // the ObjC runtime on every screen capture and mean nothing here.
        .filter(|l| !l.is_empty() && !l.starts_with("objc["))
        .collect();
    lines[lines.len().saturating_sub(3)..].join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Area;

    /// The screen and the microphone are single-instance devices: two
    /// recordings at once leave ffmpeg fighting over them and neither
    /// finalises in time. The tests that record take turns.
    static DEVICE: Mutex<()> = Mutex::new(());

    /// Takes the device turn, ignoring poisoning from an unrelated failure.
    ///
    /// The pause is for the camera: the previous test's ffmpeg has exited by
    /// the time the lock is free, but AVFoundation needs a moment longer to
    /// hand the device to the next one, and without the wait the camera
    /// occasionally reads as busy.
    fn device_turn() -> std::sync::MutexGuard<'static, ()> {
        // The microphone is a GStreamer pipeline, and only `main` initialises
        // GStreamer in the real app. Doing it again is harmless.
        gst::init().expect("GStreamer");
        let guard = DEVICE.lock().unwrap_or_else(|e| e.into_inner());
        std::thread::sleep(Duration::from_millis(750));
        guard
    }

    /// Recording settings for the tests that do not want the camera: it is not
    /// on every machine, and one test covers it where it is.
    fn no_camera() -> Settings {
        Settings { record_camera: false, ..Settings::default() }
    }

    const LISTING: &str = "\
[AVFoundation indev @ 0x7f8e1] AVFoundation video devices:
[AVFoundation indev @ 0x7f8e1] [0] FaceTime HD Camera
[AVFoundation indev @ 0x7f8e1] [1] Capture screen 0
[AVFoundation indev @ 0x7f8e1] [2] Capture screen 1
[AVFoundation indev @ 0x7f8e1] AVFoundation audio devices:
[AVFoundation indev @ 0x7f8e1] [0] MacBook Pro Microphone
[AVFoundation indev @ 0x7f8e1] [1] External Mic
";

    #[test]
    fn parses_device_listing() {
        let d = parse_devices(LISTING);
        // The camera is not a screen and must not be offered as one.
        assert_eq!(d.screens, vec![1, 2]);
        assert_eq!(d.cameras, vec![(0, "FaceTime HD Camera".to_string())]);
        assert_eq!(d.audio, vec![0, 1]);
    }

    #[test]
    fn ignores_unrelated_output() {
        let d = parse_devices("ffmpeg version 9.0.1\nsomething [3] else\n");
        assert!(d.screens.is_empty() && d.audio.is_empty() && d.cameras.is_empty());
    }

    #[test]
    fn finds_a_camera_by_name_index_or_fragment() {
        let d = parse_devices(LISTING);
        assert_eq!(find_camera(&d, "FaceTime HD Camera"), Some(0));
        assert_eq!(find_camera(&d, "0"), Some(0));
        assert_eq!(find_camera(&d, "facetime"), Some(0));
        // A screen is not a camera, and nor is an unknown name, nor an index
        // that belongs to a screen.
        assert_eq!(find_camera(&d, "1"), None);
        assert_eq!(find_camera(&d, "Capture screen 0"), None);
        assert_eq!(find_camera(&d, "Logitech"), None);
        assert_eq!(find_camera(&d, ""), None);
        assert_eq!(find_camera(&d, "  "), None);
    }

    /// Checks the parser against what the installed ffmpeg really prints:
    /// listing devices needs no Screen Recording permission, so this is safe
    /// to run anywhere ffmpeg exists.
    #[test]
    fn finds_a_real_screen_device() {
        if !crate::media::tool("ffmpeg").is_absolute() {
            eprintln!("skipping: no ffmpeg installed");
            return;
        }
        // AVFoundation stops offering a screen while the display sleeps, which
        // is ordinary on a laptop and not something to fail over.
        let Ok(found) = devices() else {
            eprintln!("skipping: no screen capture device (display asleep?)");
            return;
        };
        assert!(!found.screens.is_empty());
        // One display here, but the camera must never be mistaken for a screen.
        assert!(found.screens.len() <= 8, "{found:?}");
    }

    #[test]
    fn maps_display_order_to_device_index() {
        let d = parse_devices(LISTING);
        let outputs: Vec<Output> = (0..2)
            .map(|i| Output {
                name: format!("Display {}", i + 1),
                area: Area { x: i * 1000, y: 0, w: 1000, h: 500 },
                pixels: (2000, 1000),
            })
            .collect();
        // The second display is "Capture screen 1", device index 2.
        assert_eq!(screen_device(&d, &outputs, &outputs[1]).unwrap(), 2);
        assert_eq!(screen_device(&d, &outputs, &outputs[0]).unwrap(), 1);
    }

    /// Records the screen for real: two segments with a pause between them,
    /// joined into one file. Exercises device selection, the `q` shutdown that
    /// finalises each Matroska file, and the concatenation.
    ///
    /// Needs Screen Recording permission, so it runs with
    /// `cargo test -- --ignored`. Without it AVFoundation hands out no frames
    /// and the stall guard trips instead.
    #[test]
    #[ignore]
    fn records_and_joins_two_segments() {
        let _turn = device_turn();
        let conn = connect(Source::Monitor(None)).expect("no display to record");
        assert!(conn.crop.is_none(), "a whole display needs no crop");
        let dir = std::env::temp_dir().join(format!("scribecut-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut rec = Recorder::new(conn, dir.clone(), &no_camera()).unwrap();
        // Several seconds per segment: opening the capture devices costs one to
        // three of them before any frame arrives, so a short segment would be
        // mostly start-up and tell us little.
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(6));
        assert!(rec.is_recording());
        assert!(!rec.has_camera());
        // Pause, then resume into a second segment.
        rec.stop_segment().unwrap();
        assert!(!rec.is_recording());
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(5));
        assert_eq!(rec.pieces(), 2);
        assert!(rec.poll_error().is_none(), "recording reported an error");

        let (file, starts) = rec.finish().unwrap();
        assert!(file.ends_with("recording.mkv"));
        // One resume point, somewhere inside the first segment's run.
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert!(starts[0] > 0.5, "resume point at {}", starts[0]);

        let info = crate::media::probe(&file).unwrap();
        assert!(info.width > 100 && info.height > 100, "{}x{}", info.width, info.height);
        // Only that something was recorded: how much of a 5 s capture actually
        // arrives depends on how busy the machine is, and a full-resolution
        // Retina screen is a lot of pixels to move. Frame loss is the host's
        // problem, not this code's, so it is not what this test pins down.
        assert!(info.duration > 1.0, "duration {}", info.duration);
        // What this test does pin down: the sound must run as long as the
        // picture. Without asynchronous resampling the microphone's gaps get
        // packed together instead of padded, and the track ends early -- which
        // is what made the waveform and the transcript cover only part of the
        // recording, and what made it sound chopped up.
        // The sound must now run as long as the picture, within a fraction of
        // a second. Two separate failures used to show up here: ffmpeg's
        // AVFoundation audio input silently dropping samples, which left these
        // recordings with 60 to 97 per cent of the sound missing, and the
        // microphone starting before the screen device, which shifted what was
        // left. The microphone now comes from GStreamer and is lined up against
        // the picture when the segment is muxed.
        let audio = crate::media::audio_duration(&file).unwrap();
        assert!(
            (info.duration - audio).abs() < 0.8,
            "audio is {audio:.2}s against {:.2}s of video",
            info.duration
        );
        // The segments must have been joined, not left lying around.
        assert!(!dir.join("segment_000.mkv").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records a rectangle of the screen, which is how window and region
    /// recordings work here. Checks the crop maths all the way through: a
    /// region given in logical points must come out at the matching pixel
    /// size, which on a Retina display is twice as large.
    #[test]
    #[ignore]
    fn records_a_cropped_region() {
        let _turn = device_turn();
        let outputs = platform::outputs().unwrap();
        let display = outputs.first().expect("no display").clone();
        // Well inside the display, so nothing is clipped at an edge.
        let area = Area { x: display.area.x + 100, y: display.area.y + 80, w: 400, h: 300 };
        let conn = connect(Source::Area(area)).unwrap();
        let crop = conn.crop.expect("a region must crop the display");
        let (want_w, want_h) = crop.size(display.pixels);

        let dir = std::env::temp_dir().join(format!("scribecut-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = Recorder::new(conn, dir.clone(), &no_camera()).unwrap();
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert!(rec.poll_error().is_none(), "cropped recording reported an error");
        let (file, _) = rec.finish().unwrap();

        let info = crate::media::probe(&file).unwrap();
        assert_eq!((info.width as i32, info.height as i32), (want_w, want_h));
        // The region was 400x300 points; a 2x display must give 800x600 pixels.
        let scale = display.pixels.0 / display.area.w.max(1);
        assert_eq!((info.width as i32, info.height as i32), (400 * scale, 300 * scale));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records the screen and the camera into one file, which must come out
    /// with two video tracks, the screen first.
    #[test]
    #[ignore]
    fn records_with_a_camera_track() {
        let _turn = device_turn();
        let camera = default_camera();
        if camera.is_empty() {
            eprintln!("skipping: no camera on this machine");
            return;
        }
        assert!(camera_available(&camera), "the camera it just named must exist");
        let dir = std::env::temp_dir().join(format!("scribecut-cam-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let settings = Settings { record_camera: true, camera_device: camera, ..Settings::default() };
        let conn = connect(Source::Monitor(None)).unwrap();
        let mut rec = Recorder::new(conn, dir.clone(), &settings).unwrap();
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(4));
        if !rec.has_camera() {
            // Another app may hold the camera; that is a warning, not a failure.
            eprintln!("skipping: camera unavailable ({:?})", rec.warning());
            return;
        }
        assert!(rec.poll_error().is_none(), "camera recording reported an error");
        let (file, _) = rec.finish().unwrap();

        // The screen stays the first video track and the camera becomes the
        // second, which is what `probe` reports as `camera`.
        let info = crate::media::probe(&file).unwrap();
        let (cam_w, cam_h) = info.camera.expect("expected a camera track too");
        assert!(cam_w > 0 && cam_h > 0, "camera track is {cam_w}x{cam_h}");
        assert!(info.width >= cam_w, "the screen should be the larger, first track");
        // Three capture devices in one process is where the audio gaps were
        // worst, so this is the case most worth checking.
        let audio = crate::media::audio_duration(&file).unwrap();
        assert!(
            (info.duration - audio).abs() < 0.8,
            "audio is {audio:.2}s against {:.2}s of video",
            info.duration
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
