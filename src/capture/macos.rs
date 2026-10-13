//! macOS capture. AVFoundation only hands out whole displays, so there is no
//! portal session to hold open: each segment is one ffmpeg process grabbing the
//! display (and the microphone) and encoding H.264 in hardware with
//! VideoToolbox. Cropping to a window or region happens in that same process.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Source;
use crate::geom::{Crop, Output};
use crate::platform;

/// The display to record, as an AVFoundation device.
pub struct Target {
    /// ffmpeg's AVFoundation video device index for this display.
    screen: usize,
    /// AVFoundation audio device index, if there is a microphone.
    audio: Option<usize>,
    /// The display's full frame size in pixels, which a crop is measured against.
    pixels: (i32, i32),
    pub name: String,
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
    /// Audio input device indices.
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
             in System Settings › Privacy & Security › Screen & System Audio Recording, \
             then restart scribecut.\n{}",
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
            Section::Audio => devices.audio.push(index),
            _ => {}
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

/// Picks the display to record and works out the crop. Unlike Linux there is no
/// system dialog here, so nothing blocks on the user.
pub fn connect(source: Source) -> Result<(Target, Option<Crop>)> {
    let devices = devices()?;
    let audio = devices.audio.first().copied();
    let outputs = platform::outputs()?;
    let (output, crop) = match source {
        Source::Monitor(chosen) => {
            let output = chosen
                .or_else(|| outputs.first().cloned())
                .context("no displays")?;
            (output, None)
        }
        Source::SystemWindow => bail!("macOS has no system window picker; pick the window from the list"),
        Source::Area(area) => {
            let output = platform::output_for(&area, &outputs).context("that area is not on any display")?;
            let crop = platform::crop_for(&area, &output);
            (output, (!crop.is_none()).then_some(crop))
        }
    };
    let screen = screen_device(&devices, &outputs, &output)?;
    let target = Target { screen, audio, pixels: output.pixels, name: output.name };
    Ok((target, crop))
}

/// H.264 encoder for the recording. VideoToolbox encodes in hardware, which
/// keeps the CPU free while the screen is being captured; set
/// `SCRIBECUT_ENCODER=libx264` (and a lower bitrate) for software encoding.
fn encoder() -> String {
    std::env::var("SCRIBECUT_ENCODER").unwrap_or_else(|_| "h264_videotoolbox".into())
}

/// Recording bitrate. Hardware H.264 needs a generous one to keep screen text
/// crisp; override with `SCRIBECUT_BITRATE`.
fn bitrate() -> String {
    std::env::var("SCRIBECUT_BITRATE").unwrap_or_else(|_| "12M".into())
}

/// The ffmpeg process recording the current segment.
struct Running {
    child: Child,
    path: PathBuf,
    stderr: Arc<Mutex<String>>,
    /// When ffmpeg was spawned, to notice a segment that never grows.
    started: Instant,
}

pub struct Capture {
    target: Target,
    crop: Option<Crop>,
    current: Option<Running>,
}

impl Capture {
    pub fn new(target: Target, crop: Option<Crop>) -> Result<Self> {
        Ok(Self { target, crop, current: None })
    }

    pub fn start(&mut self, path: &Path) -> Result<()> {
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
            .args(["-i", &self.target.screen.to_string()]);
        // The microphone, as its own input: opening a display and a microphone
        // in one AVFoundation session often fails to start.
        if let Some(audio) = self.target.audio {
            cmd.args(["-f", "avfoundation", "-i", &format!(":{audio}")]);
        }
        if let Some(crop) = self.crop {
            let (w, h) = crop.size(self.target.pixels);
            cmd.args(["-filter:v", &format!("crop={w}:{h}:{}:{}", crop.left, crop.top)]);
        }
        // No `-pix_fmt`: ffmpeg converts if the encoder needs it, and asking
        // for one here would override the native nv12 path above.
        cmd.args(["-r", "30"]).args(["-c:v", &encoder(), "-b:v", &bitrate(), "-g", "60"]);
        if self.target.audio.is_some() {
            cmd.args(["-c:a", "aac", "-b:a", "160k"]);
        }
        cmd.args(["-f", "matroska"]).arg(path);
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
        self.current = Some(Running { child, path: path.to_path_buf(), stderr, started: Instant::now() });
        Ok(())
    }

    pub fn stop(&mut self) -> Result<()> {
        let Some(mut run) = self.current.take() else { return Ok(()) };
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
            bail!("timed out finalising segment");
        }
        // ffmpeg's exit code after `q` is not meaningful, so judge the segment
        // by whether it actually wrote a file.
        if std::fs::metadata(&run.path).is_ok_and(|m| m.len() > 1024) {
            return Ok(());
        }
        bail!("recording failed: {}", tail(&run.stderr));
    }

    pub fn poll_error(&mut self) -> Option<String> {
        let run = self.current.as_mut()?;
        // Still running is the normal case; anything else means ffmpeg gave up
        // (screen sharing revoked, disk full, device taken away).
        if let Ok(Some(status)) = run.child.try_wait() {
            let message = tail(&run.stderr);
            self.current = None;
            return Some(if message.is_empty() { format!("ffmpeg stopped ({status})") } else { message });
        }
        // A missing permission makes AVFoundation block instead of fail, which
        // would otherwise leave a running timer above a file that never grows.
        if run.started.elapsed() > STALL_AFTER && file_len(&run.path) == 0 {
            return Some(format!(
                "no frames have arrived from {}. Grant Screen Recording (and Microphone) \
                 permission in System Settings > Privacy & Security, then restart scribecut - \
                 see the macOS notes in the README",
                self.target.name
            ));
        }
        None
    }

    pub fn shutdown(&mut self) {
        if let Some(mut run) = self.current.take() {
            let _ = run.child.kill();
            let _ = run.child.wait();
        }
    }
}

/// How long a segment may stay empty before it counts as stalled. Generous:
/// the device takes a moment to warm up and the first keyframe to be written.
const STALL_AFTER: Duration = Duration::from_secs(8);

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
    fn device_turn() -> std::sync::MutexGuard<'static, ()> {
        DEVICE.lock().unwrap_or_else(|e| e.into_inner())
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
        assert_eq!(d.audio, vec![0, 1]);
    }

    #[test]
    fn ignores_unrelated_output() {
        let d = parse_devices("ffmpeg version 9.0.1\nsomething [3] else\n");
        assert!(d.screens.is_empty() && d.audio.is_empty());
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
        let found = devices().expect("a Mac always has a screen to capture");
        assert!(!found.screens.is_empty());
        // One display here, but the camera must never be mistaken for a screen.
        assert!(found.screens.len() <= 8, "{found:?}");
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
        use crate::capture::Recorder;

        let _turn = device_turn();
        let (target, crop) = connect(Source::Monitor(None)).expect("no display to record");
        assert!(crop.is_none(), "a whole display needs no crop");
        let dir = std::env::temp_dir().join(format!("scribecut-rec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let mut rec = Recorder::new(target, crop, dir.clone()).unwrap();
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(3));
        assert!(rec.is_recording());
        // Pause, then resume into a second segment.
        rec.stop_segment().unwrap();
        assert!(!rec.is_recording());
        rec.start_segment().unwrap();
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(rec.segments.len(), 2);
        assert!(rec.poll_error().is_none(), "recording reported an error");

        let (file, starts) = rec.finish().unwrap();
        assert!(file.ends_with("recording.mkv"));
        // One resume point, somewhere inside the first segment's run.
        assert_eq!(starts.len(), 1, "{starts:?}");
        assert!((1.0..5.0).contains(&starts[0]), "resume point at {}", starts[0]);

        let info = crate::media::probe(&file).unwrap();
        assert!(info.width > 100 && info.height > 100, "{}x{}", info.width, info.height);
        assert!((3.0..9.0).contains(&info.duration), "duration {}", info.duration);
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
        use crate::capture::Recorder;
        use crate::geom::Area;

        let _turn = device_turn();
        let outputs = crate::platform::outputs().unwrap();
        let display = outputs.first().expect("no display").clone();
        // Well inside the display, so nothing is clipped at an edge.
        let area = Area { x: display.area.x + 100, y: display.area.y + 80, w: 400, h: 300 };
        let (target, crop) = connect(Source::Area(area)).unwrap();
        let crop = crop.expect("a region must crop the display");
        let (want_w, want_h) = crop.size(display.pixels);

        let dir = std::env::temp_dir().join(format!("scribecut-crop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut rec = Recorder::new(target, Some(crop), dir.clone()).unwrap();
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
}
