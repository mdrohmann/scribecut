//! Portrait-matting models run by ONNX Runtime: MODNet in its several
//! precisions, and MediaPipe's selfie segmentation.
//!
//! The matte is what `cutout` asks for: one byte per pixel, 255 where the
//! person is. macOS uses Vision instead, so this is every platform but that.
//!
//! `SCRIBECUT_CUTOUT_MODEL` picks a model by its short name (see [`CANDIDATES`])
//! for both the preview and the export, so models can be compared without a
//! rebuild.

use std::path::PathBuf;
use std::sync::LazyLock;

use anyhow::{Context, Result, anyhow, bail};
use image::{GrayImage, RgbImage, imageops};
use ort::session::Session;
use ort::value::Tensor;

use super::Quality;

/// How a model wants its picture.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// MODNet: any size in multiples of 32, RGB scaled to -1..1.
    Modnet,
    /// MediaPipe selfie segmentation: one fixed size, RGB scaled to 0..1.
    Selfie,
}

/// A model the cutout can run.
struct Candidate {
    name: &'static str,
    url: &'static str,
    file: &'static str,
    kind: Kind,
}

/// Every model `SCRIBECUT_CUTOUT_MODEL` can name, all Apache 2.0 and exported
/// to ONNX for transformers.js. The MODNets differ only in precision.
const CANDIDATES: &[Candidate] = &[
    Candidate {
        name: "modnet",
        url: "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model.onnx",
        file: "modnet.onnx",
        kind: Kind::Modnet,
    },
    Candidate {
        name: "modnet-quantized",
        url: "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model_quantized.onnx",
        file: "modnet_quantized.onnx",
        kind: Kind::Modnet,
    },
    Candidate {
        name: "modnet-uint8",
        url: "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model_uint8.onnx",
        file: "modnet_uint8.onnx",
        kind: Kind::Modnet,
    },
    Candidate {
        name: "modnet-fp16",
        url: "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model_fp16.onnx",
        file: "modnet_fp16.onnx",
        kind: Kind::Modnet,
    },
    Candidate {
        name: "modnet-q4",
        url: "https://huggingface.co/Xenova/modnet/resolve/main/onnx/model_q4.onnx",
        file: "modnet_q4.onnx",
        kind: Kind::Modnet,
    },
    Candidate {
        name: "selfie",
        url: "https://huggingface.co/onnx-community/mediapipe_selfie_segmentation/resolve/main/onnx/model.onnx",
        file: "selfie.onnx",
        kind: Kind::Selfie,
    },
    Candidate {
        name: "selfie-landscape",
        url: "https://huggingface.co/onnx-community/mediapipe_selfie_segmentation_landscape/resolve/main/onnx/model.onnx",
        file: "selfie_landscape.onnx",
        kind: Kind::Selfie,
    },
];

/// The model a tier runs: the `SCRIBECUT_CUTOUT_MODEL` override, or the
/// default. The preview takes the selfie model, which keeps up with playback
/// on a CPU where MODNet doesn't, at the price of a coarser outline; the export
/// keeps full-precision MODNet for the hair. The other precisions of MODNet
/// were no faster on a CPU, and the 8-bit ones soften the outline.
fn candidate(quality: Quality) -> Result<&'static Candidate> {
    let name = std::env::var("SCRIBECUT_CUTOUT_MODEL").unwrap_or_else(|_| {
        match quality {
            Quality::Live => "selfie",
            Quality::Export => "modnet",
        }
        .to_owned()
    });
    CANDIDATES.iter().find(|c| c.name == name).with_context(|| {
        let names: Vec<&str> = CANDIDATES.iter().map(|c| c.name).collect();
        format!("SCRIBECUT_CUTOUT_MODEL={name} is not one of {}", names.join(", "))
    })
}

/// Names the export's model in the matte cache key, so mattes from different
/// models are never mixed up.
pub(super) static NAME: LazyLock<String> =
    LazyLock::new(|| candidate(Quality::Export).map_or_else(|e| e.to_string(), |c| c.file.to_owned()));

/// A model, ready to run on pictures of one size.
pub(super) struct Model {
    session: Session,
    input: String,
    kind: Kind,
    /// The pictures' size.
    w: u32,
    h: u32,
    /// The size the model runs at: the pictures' own, or the model's fixed one.
    model_w: u32,
    model_h: u32,
    /// Channels first (NCHW) rather than last (NHWC).
    planar: bool,
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
    /// Loads the tier's model (downloading it once) for `w`×`h` RGB pictures.
    /// An export gets the machine; the preview leaves cores for playback.
    pub(super) fn new(w: u32, h: u32, quality: Quality) -> Result<Self> {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        let threads = match quality {
            Quality::Live => (cores / 2).clamp(1, 4),
            Quality::Export => cores.min(8),
        };
        let candidate = candidate(quality)?;
        let path = ensure_model(candidate, |_| {})?;
        // The builder's errors carry the builder, which can't cross threads: keep the message.
        let session = Session::builder(&environment()?)?
            .with_intra_threads(threads)
            .map_err(|e| anyhow!("{e}"))?
            .commit_from_file(&path)
            .map_err(|e| anyhow!("loading the cutout model: {e}"))?;
        let input = session.inputs()[0].name().to_string();
        // MODNet takes any size; the selfie models say theirs, in either layout.
        let shape: Vec<i64> = session.inputs()[0].dtype().tensor_shape().map(|s| s.to_vec()).unwrap_or_default();
        let (model_w, model_h, planar) = match (candidate.kind, shape.as_slice()) {
            (Kind::Modnet, _) => (w, h, true),
            (Kind::Selfie, &[_, 3, mh, mw]) if mw > 0 && mh > 0 => (mw as u32, mh as u32, true),
            (Kind::Selfie, &[_, mh, mw, 3]) if mw > 0 && mh > 0 => (mw as u32, mh as u32, false),
            (Kind::Selfie, other) => bail!("{}: unexpected input shape {other:?}", candidate.name),
        };
        Ok(Self {
            session,
            input,
            kind: candidate.kind,
            w,
            h,
            model_w,
            model_h,
            planar,
            pixels: vec![0.0; (model_w * model_h * 3) as usize],
        })
    }

    /// The matte of one picture (RGB, `w`×`h`): 255 = person, 0 = room.
    pub(super) fn run(&mut self, rgb: &[u8], matte: &mut [u8]) -> Result<()> {
        let resized;
        let rgb = if (self.model_w, self.model_h) == (self.w, self.h) {
            rgb
        } else {
            let picture = RgbImage::from_raw(self.w, self.h, rgb.to_vec()).context("picture size")?;
            resized = imageops::resize(&picture, self.model_w, self.model_h, imageops::FilterType::Triangle);
            resized.as_raw()
        };
        let plane = (self.model_w * self.model_h) as usize;
        for (i, px) in rgb.as_chunks::<3>().0.iter().enumerate() {
            for (c, &v) in px.iter().enumerate() {
                // As each model was trained: MODNet -1..1, the selfie models 0..1.
                let v = match self.kind {
                    Kind::Modnet => v as f32 / 127.5 - 1.0,
                    Kind::Selfie => v as f32 / 255.0,
                };
                let at = if self.planar { c * plane + i } else { i * 3 + c };
                self.pixels[at] = v;
            }
        }
        let shape = if self.planar {
            [1usize, 3, self.model_h as usize, self.model_w as usize]
        } else {
            [1usize, self.model_h as usize, self.model_w as usize, 3]
        };
        let tensor = Tensor::from_array((shape, self.pixels.clone()))?;
        let outputs = self.session.run(ort::inputs![self.input.as_str() => tensor])?;
        // One channel, so [1,1,h,w] and [1,h,w,1] are the same bytes.
        let (_, alpha) = outputs[0].try_extract_tensor::<f32>()?;
        let alpha = alpha.iter().map(|a| (a.clamp(0.0, 1.0) * 255.0).round() as u8);
        if (self.model_w, self.model_h) == (self.w, self.h) {
            for (g, a) in matte.iter_mut().zip(alpha) {
                *g = a;
            }
        } else {
            let small = GrayImage::from_raw(self.model_w, self.model_h, alpha.collect()).context("matte size")?;
            let big = imageops::resize(&small, self.w, self.h, imageops::FilterType::Triangle);
            matte.copy_from_slice(big.as_raw());
        }
        Ok(())
    }
}

/// The model's local path, downloading it once if needed.
fn ensure_model(candidate: &Candidate, progress: impl FnMut(f32)) -> Result<PathBuf> {
    let dir = dirs::data_dir().context("no data dir")?.join("scribecut/models");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(candidate.file);
    if !path.is_file() {
        crate::media::download(candidate.url, &path, progress)?;
    }
    Ok(path)
}
