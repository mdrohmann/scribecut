//! MODNet, a portrait-matting model, run by ONNX Runtime.
//!
//! The matte is what `cutout` asks for: one byte per pixel, 255 where the
//! person is. macOS uses Vision instead, so this is every platform but that.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use ort::session::Session;
use ort::value::Tensor;

/// Names this engine in the export's matte cache key.
pub(super) const NAME: &str = "modnet.onnx";

/// MODNet (Apache 2.0), exported to ONNX for transformers.js.
const MODEL_URL: &str = "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model.onnx";
const MODEL_FILE: &str = "modnet.onnx";

/// The model, ready to run on pictures of one size.
pub(super) struct Model {
    session: Session,
    input: String,
    w: u32,
    h: u32,
    pixels: Vec<f32>,
}

/// ONNX Runtime allows one environment per process, shared by the preview and the export.
fn environment() -> Result<ort::environment::Environment> {
    static ENV: std::sync::Mutex<Option<ort::environment::Environment>> = std::sync::Mutex::new(None);
    let mut env = ENV.lock().unwrap();
    if env.is_none() {
        *env = Some(ort::init().build().context("starting ONNX Runtime")?);
    }
    Ok(env.clone().unwrap())
}

impl Model {
    /// Loads the model (downloading it once) for `w`×`h` RGB pictures. An
    /// export gets the machine; the preview leaves cores for playback.
    pub(super) fn new(w: u32, h: u32, quality: super::Quality) -> Result<Self> {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = match quality {
            super::Quality::Live => (cores / 2).clamp(1, 4),
            super::Quality::Export => cores.min(8),
        };
        let path = ensure_model(|_| {})?;
        // The builder's errors carry the builder, which can't cross threads: keep the message.
        let session = Session::builder(&environment()?)?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(&path)
            .map_err(|e| anyhow!("loading the cutout model: {e}"))?;
        let input = session.inputs()[0].name().to_string();
        Ok(Self { session, input, w, h, pixels: vec![0.0; (w * h * 3) as usize] })
    }

    /// The matte of one picture (RGB, `w`×`h`): 255 = person, 0 = room.
    pub(super) fn run(&mut self, rgb: &[u8], matte: &mut [u8]) -> Result<()> {
        let plane = (self.w * self.h) as usize;
        // RGB planes scaled to -1..1, as the model was trained.
        for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
            for (c, &v) in px.iter().enumerate() {
                self.pixels[c * plane + i] = v as f32 / 127.5 - 1.0;
            }
        }
        let shape = [1usize, 3, self.h as usize, self.w as usize];
        let tensor = Tensor::from_array((shape, self.pixels.clone()))?;
        let outputs = self.session.run(ort::inputs![self.input.as_str() => tensor])?;
        let (_, alpha) = outputs[0].try_extract_tensor::<f32>()?;
        for (g, a) in matte.iter_mut().zip(alpha) {
            *g = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        Ok(())
    }
}


/// The model's local path, downloading it once if needed.
fn ensure_model(progress: impl FnMut(f32)) -> Result<PathBuf> {
    let dir = dirs::data_dir().context("no data dir")?.join("scribecut/models");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(MODEL_FILE);
    if !path.is_file() {
        crate::media::download(MODEL_URL, &path, progress)?;
    }
    Ok(path)
}
