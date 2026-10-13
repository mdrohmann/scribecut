//! Video playback through GStreamer's playbin, with frames handed to egui.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use gst::prelude::*;
use gst_video::prelude::*;

pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

pub struct Player {
    playbin: gst::Element,
    /// Latest decoded frame, taken by the UI when it uploads a texture.
    pub frame: Arc<Mutex<Option<Frame>>>,
    pub playing: bool,
    last_pos: f64,
}

impl Player {
    /// `preview` is the size frames are scaled to before reaching the UI.
    pub fn new(path: &Path, preview: (u32, u32), ctx: egui::Context) -> Result<Self> {
        let playbin = gst::ElementFactory::make("playbin").build()?;
        let uri = gst::glib::filename_to_uri(path, None)?;
        playbin.set_property("uri", uri.as_str());

        let (w, h) = preview;
        let bin = gst::parse::bin_from_description(
            &format!(
                "videoconvert ! videoscale ! video/x-raw,format=RGBA,width={w},height={h},pixel-aspect-ratio=1/1 \
                 ! appsink name=sink max-buffers=1 drop=true"
            ),
            true,
        )?;
        let sink = bin
            .by_name("sink")
            .and_then(|s| s.downcast::<gst_app::AppSink>().ok())
            .context("no appsink")?;

        let frame = Arc::new(Mutex::new(None));
        let store = {
            let frame = frame.clone();
            move |sample: gst::Sample| {
                if let Some(f) = to_frame(&sample) {
                    *frame.lock().unwrap() = Some(f);
                    ctx.request_repaint();
                }
            }
        };
        let store2 = store.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    store(s.pull_sample().map_err(|_| gst::FlowError::Eos)?);
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |s| {
                    store2(s.pull_preroll().map_err(|_| gst::FlowError::Eos)?);
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        playbin.set_property("video-sink", &bin);
        playbin.set_state(gst::State::Paused).context("opening video")?;
        // Wait for preroll so the first frame shows and seeking works immediately.
        let _ = playbin.state(gst::ClockTime::from_seconds(5));
        Ok(Self { playbin, frame, playing: false, last_pos: 0.0 })
    }

    pub fn play(&mut self) {
        if self.playbin.set_state(gst::State::Playing).is_ok() {
            self.playing = true;
        }
    }

    pub fn pause(&mut self) {
        let _ = self.playbin.set_state(gst::State::Paused);
        self.playing = false;
    }

    pub fn toggle(&mut self) {
        if self.playing { self.pause() } else { self.play() }
    }

    pub fn seek(&mut self, t: f64) {
        let t = t.max(0.0);
        let _ = self.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::ClockTime::from_nseconds((t * 1e9) as u64),
        );
        self.last_pos = t;
    }

    /// Current position in seconds.
    pub fn position(&mut self) -> f64 {
        if let Some(p) = self.playbin.query_position::<gst::ClockTime>() {
            self.last_pos = p.nseconds() as f64 / 1e9;
        }
        self.last_pos
    }

    /// Handles end-of-stream and errors. Returns an error message if playback failed.
    pub fn poll(&mut self) -> Option<String> {
        let bus = self.playbin.bus()?;
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::Eos(_) => self.pause(),
                gst::MessageView::Error(e) => {
                    self.pause();
                    return Some(e.error().to_string());
                }
                _ => {}
            }
        }
        None
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

fn to_frame(sample: &gst::Sample) -> Option<Frame> {
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let buffer = sample.buffer()?;
    let vf = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).ok()?;
    let (width, height) = (info.width() as usize, info.height() as usize);
    let stride = vf.plane_stride()[0] as usize;
    let data = vf.plane_data(0).ok()?;
    let row = width * 4;
    let rgba = if stride == row {
        data[..row * height].to_vec()
    } else {
        (0..height).flat_map(|y| data[y * stride..y * stride + row].iter().copied()).collect()
    };
    Some(Frame { width, height, rgba })
}

/// Scales `(w, h)` down to fit within `max_w` wide, keeping even dimensions.
pub fn preview_size(w: u32, h: u32, max_w: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1280, 720);
    }
    let scale = (max_w as f64 / w as f64).min(1.0);
    let even = |v: f64| ((v / 2.0).round() as u32 * 2).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_fits_within_the_width_and_stays_even() {
        assert_eq!(preview_size(3584, 2240, 1280), (1280, 800));
        // Never upscales.
        assert_eq!(preview_size(640, 360, 1280), (640, 360));
        // A missing size falls back rather than dividing by zero.
        assert_eq!(preview_size(0, 0, 1280), (1280, 720));
        let (w, h) = preview_size(1919, 1079, 1000);
        assert_eq!((w % 2, h % 2), (0, 0), "4:2:0 needs even dimensions");
    }

    /// Decodes a real clip through playbin, which is what the editor's preview
    /// is built on. Needs GStreamer's decoders and ffmpeg to make the clip, so
    /// it runs with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn prerolls_a_frame_from_a_real_clip() {
        gst::init().unwrap();
        let dir = std::env::temp_dir().join(format!("scribecut-player-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mkv");
        let ok = crate::media::ffmpeg()
            .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30:duration=3"])
            .args(["-c:v", "libx264"])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok, "could not make a test clip");

        let player = Player::new(&clip, (320, 180), egui::Context::default()).unwrap();
        // `Player::new` waits for preroll, so a frame must already be waiting.
        let latest = player.frame.lock().unwrap();
        let frame = latest.as_ref().expect("playbin decoded no frame");
        assert_eq!((frame.width, frame.height), (320, 180));
        assert_eq!(frame.rgba.len(), 320 * 180 * 4);
        // A test pattern is never a single flat colour.
        let first = &frame.rgba[..4];
        assert!(frame.rgba.chunks(4).any(|px| px != first), "frame is blank");

        drop(latest);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
