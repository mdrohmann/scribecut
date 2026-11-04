//! Cuts the person out of the camera picture, so the export can put a blurred
//! or chosen background behind them.
//!
//! A portrait-matting model turns each camera frame into a matte: how much of
//! every pixel is the person (white) or the room (black). Which model is the
//! platform's business — Vision on macOS, MODNet through ONNX Runtime
//! elsewhere (see the two `Model`s in this module's folder) — and the two draw
//! slightly different outlines. Mattes are made once per export layout at a
//! reduced size and frame rate, only for the stretches where the camera is on
//! screen, and cached next to the recording; the engine's name is part of the
//! cache key, so a matte is never reused across platforms. ffmpeg then merges
//! matte, camera and background (see `export`). The editor's preview runs the
//! model live on smaller frames (`LiveMatte`).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};

use crate::project::{Project, Range};

#[cfg(target_os = "macos")]
mod vision;
#[cfg(target_os = "macos")]
use vision as engine;

#[cfg(not(target_os = "macos"))]
mod modnet;
#[cfg(not(target_os = "macos"))]
use modnet as engine;

use engine::Model;

/// What a matte is for. The preview has to keep up with playback and is shown
/// smaller; an export can take its time and is what people look at.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Quality {
    Live,
    Export,
}

/// Mattes per second: a person moves little between frames, and the model is
/// the slow part of an export with a background.
pub const MATTE_FPS: u32 = 15;
/// Width the model sees. Its height follows the camera's shape, in multiples
/// of 32 as the model needs.
const MATTE_WIDTH: u32 = 512;

/// Matte size for a camera of `w`×`h`.
fn matte_size((w, h): (u32, u32)) -> (u32, u32) {
    let height = (MATTE_WIDTH as f64 * h as f64 / w.max(1) as f64 / 32.0).round() as u32 * 32;
    (MATTE_WIDTH, height.max(32))
}

/// The camera's mattes as a grey video on the recording's timeline, at
/// [`MATTE_FPS`]: black where the camera isn't shown. Made on first use and
/// cached; `progress` gets 0..1 while it is made.
pub fn matte(p: &Project, mut progress: impl FnMut(f32)) -> Result<PathBuf> {
    let camera = p.camera_size.context("this recording has no camera track")?;
    let (w, h) = matte_size(camera);
    // Some margin around each stretch, so frames read from either side of a
    // boundary still find the person.
    let margin = 1.0 / MATTE_FPS as f64;
    let shown: Vec<Range> =
        p.camera_shown().iter().map(|r| Range { start: (r.start - margin).max(0.0), end: r.end + margin }).collect();
    let (Some(first), Some(last)) = (shown.first(), shown.last()) else { bail!("the camera is not shown anywhere") };
    let rounded: Vec<(i64, i64)> =
        shown.iter().map(|r| ((r.start * 10.0).round() as i64, (r.end * 10.0).round() as i64)).collect();
    let out = crate::sources::cache_dir(p)?
        .join(crate::sources::key(&p.media, (&*engine::NAME, MATTE_FPS, w, h, rounded))?)
        .with_extension("matte.mkv");
    if out.is_file() {
        return Ok(out);
    }

    let mut model = Model::new(w, h, Quality::Export)?;
    // Only the part of the camera track that is shown is read; the frames
    // before and after it are written black.
    let fps = MATTE_FPS as f64;
    let skip = (first.start * fps).floor() as u64;
    let total_frames = (p.duration * fps).ceil() as u64;
    let mut decode = crate::media::ffmpeg()
        .stdin(Stdio::null())
        .args(["-nostdin", "-v", "error", "-ss", &format!("{:.3}", skip as f64 / fps), "-i"])
        .arg(&p.media)
        .args(["-t", &format!("{:.3}", last.end - skip as f64 / fps)])
        .args(["-map", "0:v:1", "-vf", &format!("fps={MATTE_FPS}:round=near,scale={w}:{h}:flags=bilinear")])
        .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;
    let part = out.with_extension("part.mkv");
    let mut encode = crate::media::ffmpeg()
        .args(["-nostdin", "-y", "-v", "error", "-f", "rawvideo", "-pix_fmt", "gray"])
        .args(["-s", &format!("{w}x{h}"), "-framerate", &MATTE_FPS.to_string(), "-i", "-"])
        .args(["-c:v", "ffv1"])
        .arg(&part)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running ffmpeg")?;

    let total: f64 = shown.iter().map(|r| r.len() * fps).sum::<f64>().max(1.0);
    let mut done = 0.0;
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    let mut gray = vec![0u8; (w * h) as usize];
    let mut source = decode.stdout.take().unwrap();
    let mut sink = encode.stdin.take().unwrap();
    let result = (|| -> Result<()> {
        let mut k = 0;
        while k < skip {
            sink.write_all(&gray).context("writing the matte")?;
            k += 1;
        }
        while read_frame(&mut source, &mut rgb)? {
            let t = k as f64 / fps;
            if shown.iter().any(|r| r.start <= t && t < r.end) {
                model.run(&rgb, &mut gray)?;
                done += 1.0;
                progress((done / total).min(1.0) as f32);
            } else {
                gray.fill(0);
            }
            sink.write_all(&gray).context("writing the matte")?;
            k += 1;
        }
        gray.fill(0);
        while k < total_frames {
            sink.write_all(&gray).context("writing the matte")?;
            k += 1;
        }
        Ok(())
    })();
    drop(sink);
    let decoded = decode.wait_with_output()?;
    let encoded = encode.wait_with_output()?;
    result?;
    if !decoded.status.success() {
        bail!("reading the camera track failed: {}", String::from_utf8_lossy(&decoded.stderr).trim());
    }
    if !encoded.status.success() {
        bail!("writing the matte failed: {}", String::from_utf8_lossy(&encoded.stderr).trim());
    }
    std::fs::rename(&part, &out)?;
    Ok(out)
}

/// Width the model sees in the editor's preview: smaller than for the export,
/// so it keeps up with playback.
const LIVE_WIDTH: u32 = 320;

/// A matte at the model's size.
pub struct Matte {
    pub w: u32,
    pub h: u32,
    pub alpha: Vec<u8>,
}

/// Cuts the person out of preview frames on a background thread. The editor
/// hands it each new camera frame; a frame that arrives while the model is
/// busy replaces the waiting one, so the matte lags by at most one run.
pub struct LiveMatte {
    shared: std::sync::Arc<LiveShared>,
}

/// Width, height and RGBA pixels.
type RawFrame = (usize, usize, Vec<u8>);

#[derive(Default)]
struct LiveShared {
    /// The next frame to cut out, and whether to stop.
    input: std::sync::Mutex<(Option<RawFrame>, bool)>,
    wake: std::sync::Condvar,
    /// The latest matte and a count that goes up with each new one.
    output: std::sync::Mutex<(Option<std::sync::Arc<Matte>>, u64)>,
    error: std::sync::Mutex<Option<String>>,
}

impl LiveMatte {
    /// Starts the worker for camera frames of shape `w`×`h`; `repaint` is
    /// called after each new matte.
    pub fn start(camera: (u32, u32), repaint: impl Fn() + Send + 'static) -> Self {
        let shared = std::sync::Arc::new(LiveShared::default());
        let worker = shared.clone();
        let height = (LIVE_WIDTH as f64 * camera.1 as f64 / camera.0.max(1) as f64 / 32.0).round() as u32 * 32;
        let (w, h) = (LIVE_WIDTH, height.max(32));
        std::thread::spawn(move || {
            let mut model = match Model::new(w, h, Quality::Live) {
                Ok(m) => m,
                Err(e) => {
                    *worker.error.lock().unwrap() = Some(format!("{e:#}"));
                    repaint();
                    return;
                }
            };
            let mut matte = vec![0u8; (w * h) as usize];
            loop {
                let (fw, fh, rgba) = {
                    let mut input = worker.input.lock().unwrap();
                    loop {
                        if input.1 {
                            return;
                        }
                        if let Some(frame) = input.0.take() {
                            break frame;
                        }
                        input = worker.wake.wait(input).unwrap();
                    }
                };
                let Some(frame) = image::RgbaImage::from_raw(fw as u32, fh as u32, rgba) else { continue };
                let small = image::imageops::resize(&frame, w, h, image::imageops::FilterType::Triangle);
                let rgb: Vec<u8> = small.pixels().flat_map(|p| [p[0], p[1], p[2]]).collect();
                if let Err(e) = model.run(&rgb, &mut matte) {
                    *worker.error.lock().unwrap() = Some(format!("{e:#}"));
                    continue;
                }
                let mut output = worker.output.lock().unwrap();
                output.0 = Some(std::sync::Arc::new(Matte { w, h, alpha: matte.clone() }));
                output.1 += 1;
                drop(output);
                repaint();
            }
        });
        Self { shared }
    }

    /// Queues a camera frame (RGBA) to be cut out next.
    pub fn submit(&self, w: usize, h: usize, rgba: Vec<u8>) {
        self.shared.input.lock().unwrap().0 = Some((w, h, rgba));
        self.shared.wake.notify_one();
    }

    /// The newest matte and its number, which changes with every new one.
    pub fn latest(&self) -> (Option<std::sync::Arc<Matte>>, u64) {
        self.shared.output.lock().unwrap().clone()
    }

    /// Why the model could not run, if it couldn't.
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().unwrap().clone()
    }
}

impl Drop for LiveMatte {
    fn drop(&mut self) {
        self.shared.input.lock().unwrap().1 = true;
        self.shared.wake.notify_one();
    }
}

/// Blur radius for a camera picture of `w`×`h`, the same in preview and export.
pub fn blur_radius(w: u32, h: u32) -> u32 {
    (w.min(h) / 24).max(1)
}

/// Puts the person in `rgba` (`w`×`h`) in front of `background`: a picture
/// of the same size, or `None` for the room blurred. `matte` may be smaller;
/// it is stretched to fit.
pub fn compose(rgba: &mut [u8], w: usize, h: usize, matte: &Matte, background: Option<&[u8]>) {
    let alpha = image::imageops::resize(
        &image::GrayImage::from_raw(matte.w, matte.h, matte.alpha.clone()).expect("matte size"),
        w as u32,
        h as u32,
        image::imageops::FilterType::Triangle,
    );
    let alpha = alpha.as_raw();
    let blurred;
    let background = match background {
        Some(b) => b,
        None => {
            blurred = blur_room(rgba, w, h, alpha, blur_radius(w as u32, h as u32) as usize);
            &blurred
        }
    };
    for ((px, bg), &a) in rgba.as_chunks_mut::<4>().0.iter_mut().zip(background.as_chunks::<4>().0).zip(alpha) {
        let a = a as u32;
        for c in 0..3 {
            px[c] = ((px[c] as u32 * a + bg[c] as u32 * (255 - a) + 127) / 255) as u8;
        }
    }
}

/// The room blurred without the person in it: every colour is weighted by how
/// much of its pixel is room, blurred, and divided by the blurred weights, so
/// the person doesn't leave a dark smudge around themselves.
fn blur_room(rgba: &[u8], w: usize, h: usize, alpha: &[u8], radius: usize) -> Vec<u8> {
    let n = w * h;
    let mut planes = vec![0f32; 4 * n];
    for (i, (px, &a)) in rgba.as_chunks::<4>().0.iter().zip(alpha).enumerate() {
        let room = (255 - a) as f32 / 255.0;
        for c in 0..3 {
            planes[c * n + i] = px[c] as f32 * room;
        }
        planes[3 * n + i] = room;
    }
    // Two box blurs: close to a Gaussian, like ffmpeg's `boxblur=r:2`.
    for plane in planes.chunks_mut(n) {
        for _ in 0..2 {
            box_blur(plane, w, h, radius);
        }
    }
    let mut out = rgba.to_vec();
    for (i, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let weight = planes[3 * n + i];
        if weight > 1e-3 {
            for c in 0..3 {
                px[c] = (planes[c * n + i] / weight).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

/// Box blur of one plane, rows then columns, edges clamped.
fn box_blur(plane: &mut [f32], w: usize, h: usize, r: usize) {
    let mut line = Vec::new();
    let mut pass = |plane: &mut [f32], count: usize, len: usize, at: &dyn Fn(usize, usize) -> usize| {
        for k in 0..count {
            line.clear();
            line.extend((0..len).map(|i| plane[at(k, i)]));
            let get = |i: isize| line[i.clamp(0, len as isize - 1) as usize];
            let mut sum: f32 = (-(r as isize)..=r as isize).map(get).sum();
            let scale = 1.0 / (2 * r + 1) as f32;
            for i in 0..len {
                plane[at(k, i)] = sum * scale;
                sum += get(i as isize + r as isize + 1) - get(i as isize - r as isize);
            }
        }
    };
    pass(plane, h, w, &|y, x| y * w + x);
    pass(plane, w, h, &|x, y| y * w + x);
}

/// Fills `buf` with the next frame; false at the end of the stream.
fn read_frame(source: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match source.read(&mut buf[filled..])? {
            0 if filled == 0 => return Ok(false),
            0 => bail!("the camera track ended mid-frame"),
            n => filled += n,
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preview and the export each load a model in the same process.
    #[test]
    #[ignore = "downloads the cutout model"]
    fn two_models_in_one_process() {
        let mut a = Model::new(64, 64, Quality::Live).unwrap();
        let mut b = Model::new(64, 64, Quality::Export).unwrap();
        let (rgb, mut matte) = (vec![128u8; 64 * 64 * 3], vec![0u8; 64 * 64]);
        a.run(&rgb, &mut matte).unwrap();
        b.run(&rgb, &mut matte).unwrap();
    }

    #[test]
    fn matte_keeps_the_camera_shape_in_multiples_of_32() {
        assert_eq!(matte_size((1280, 720)), (512, 288));
        assert_eq!(matte_size((640, 480)), (512, 384));
        assert_eq!(matte_size((720, 1280)), (512, 896));
    }

    #[test]
    fn blurred_room_has_no_shadow_of_the_person() {
        // A white room with a black "person" in the middle third.
        let (w, h) = (60, 30);
        let mut rgba = vec![255u8; w * h * 4];
        let mut alpha = vec![0u8; w * h];
        for y in 0..h {
            for x in 20..40 {
                rgba[(y * w + x) * 4..][..3].fill(0);
                alpha[y * w + x] = 255;
            }
        }
        let room = blur_room(&rgba, w, h, &alpha, 5);
        // Right next to the person the room stays white; a plain blur would grey it.
        assert_eq!(&room[(10 * w + 18) * 4..][..3], &[255, 255, 255]);
        assert_eq!(&room[(10 * w + 41) * 4..][..3], &[255, 255, 255]);

        // Composed over a red picture: person kept, room replaced.
        let matte = Matte { w: w as u32, h: h as u32, alpha };
        let red: Vec<u8> = (0..w * h).flat_map(|_| [255, 0, 0, 255]).collect();
        let mut out = rgba.clone();
        compose(&mut out, w, h, &matte, Some(&red));
        assert_eq!(&out[(10 * w + 30) * 4..][..3], &[0, 0, 0]);
        assert_eq!(&out[(10 * w + 5) * 4..][..3], &[255, 0, 0]);
    }

    /// The preview worker on a real picture: `SCRIBECUT_TEST_PICTURE` = a camera
    /// frame (PNG). Writes `<picture>.blurred.png` next to it.
    #[test]
    #[ignore]
    fn live_matte_on_a_picture() {
        let path = PathBuf::from(std::env::var("SCRIBECUT_TEST_PICTURE").expect("SCRIBECUT_TEST_PICTURE"));
        let frame = image::open(&path).unwrap().resize(640, 640, image::imageops::FilterType::Triangle).to_rgba8();
        let (w, h) = (frame.width() as usize, frame.height() as usize);
        let live = LiveMatte::start((w as u32, h as u32), || {});
        let started = std::time::Instant::now();
        let mut times = Vec::new();
        for round in 1..=4 {
            let sent = std::time::Instant::now();
            live.submit(w, h, frame.as_raw().clone());
            while live.latest().1 < round {
                assert!(live.error().is_none(), "{:?}", live.error());
                assert!(started.elapsed().as_secs() < 120, "no matte");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            times.push(sent.elapsed());
        }
        println!("matte times {times:?}");
        let matte = live.latest().0.unwrap();
        let mut rgba = frame.into_raw();
        compose(&mut rgba, w, h, &matte, None);
        image::RgbaImage::from_raw(w as u32, h as u32, rgba).unwrap().save(path.with_extension("blurred.png")).unwrap();
    }

    /// Downloads the model and cuts the person out of a real camera recording:
    /// `SCRIBECUT_TEST_MEDIA` = a recording with a camera track. Writes the
    /// matte to the recording's `.scribecut-cache/`.
    #[test]
    #[ignore]
    fn mattes_a_real_camera_track() {
        let media = PathBuf::from(std::env::var("SCRIBECUT_TEST_MEDIA").expect("SCRIBECUT_TEST_MEDIA"));
        let info = crate::media::probe(&media).unwrap();
        let mut p = Project {
            media,
            duration: 4.0,
            camera_size: info.camera,
            camera: crate::project::CameraSettings { default: crate::project::CameraMode::Full, ..Default::default() },
            ..Default::default()
        };
        p.add_cut(Range::new(2.0, 3.0));
        let matte = matte(&p, |_| {}).unwrap();
        let frames = std::process::Command::new("ffprobe")
            .args(["-v", "error", "-count_frames", "-select_streams", "v", "-show_entries", "stream=nb_read_frames"])
            .args(["-of", "csv=p=0"])
            .arg(&matte)
            .output()
            .unwrap();
        let n: u32 = String::from_utf8_lossy(&frames.stdout).trim().parse().unwrap();
        println!("{} has {n} frames", matte.display());
        assert!(n >= 4 * MATTE_FPS, "{n} frames");
    }
}
