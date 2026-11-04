//! Person segmentation with Vision, the matting model macOS ships with.
//!
//! The matte is what `cutout` asks for: one byte per pixel, 255 where the
//! person is. Vision replaces MODNet and ONNX Runtime here, which also spares
//! the mac a 25 MB model download and a library nobody builds for Intel macs
//! any more. Its outline is drawn a little differently, so a matte cached on
//! one platform is not reused on the other (see the cache key in `cutout`).
//!
//! A request runs on a `VNSequenceRequestHandler`, which is the one meant for
//! the frames of a video, and one `Model` belongs to one thread: Vision's
//! request objects are not thread-safe, and neither the preview worker nor the
//! export hands theirs on.
//!
//! The quality levels are far apart, so the preview and the export pick
//! different ones. On a 2019 Intel i9, with no neural engine to help:
//!
//! | level | 320 px (preview) | 512 px (export) |
//! | --- | --- | --- |
//! | fast | 35-45 ms a frame | |
//! | accurate | ~290 ms a frame | ~240 ms a frame |
//!
//! The first call also costs a second or two while Vision loads the model.
//! `fast` is as good as `accurate` at preview size here, so the preview takes
//! it; the export takes `accurate` and spends about 3.6 seconds per second of
//! camera shown. Both beat MODNet on the same machine (~50 ms a preview frame,
//! about 5 seconds per second of camera in an export).

use std::ffi::c_void;
use std::ptr::NonNull;

use anyhow::{Result, bail};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferCreateWithBytes, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_OneComponent8,
};
use objc2_foundation::NSArray;
use objc2_vision::{
    VNGeneratePersonSegmentationRequest, VNGeneratePersonSegmentationRequestQualityLevel, VNRequest,
    VNSequenceRequestHandler,
};

/// Names this engine in the export's matte cache key.
pub(super) const NAME: &str = "vision-person-segmentation";

/// The request, its handler and the frame buffer it reads, for pictures of one
/// size.
pub(super) struct Model {
    requests: Retained<NSArray<VNRequest>>,
    request: Retained<VNGeneratePersonSegmentationRequest>,
    handler: Retained<VNSequenceRequestHandler>,
    w: u32,
    h: u32,
    /// The picture as Vision wants it: 32-bit BGRA, one row after another.
    bgra: Vec<u8>,
}

impl Model {
    /// A model for `w`×`h` RGB pictures. Vision shares out the cores itself;
    /// what it offers instead is a quality level, and the levels are far
    /// apart: see the module's docs for what each costs here.
    pub(super) fn new(w: u32, h: u32, quality: super::Quality) -> Result<Self> {
        // SAFETY: a plain alloc/init of a Vision request, with a quality level
        // from its own enum.
        let request = unsafe {
            let request = VNGeneratePersonSegmentationRequest::new();
            request.setQualityLevel(match quality {
                super::Quality::Live => VNGeneratePersonSegmentationRequestQualityLevel::Fast,
                super::Quality::Export => VNGeneratePersonSegmentationRequestQualityLevel::Accurate,
            });
            // One byte per pixel, so the mask needs no conversion after.
            request.setOutputPixelFormat(kCVPixelFormatType_OneComponent8);
            request
        };
        // SAFETY: a person-segmentation request is a VNRequest.
        let as_request = unsafe { Retained::cast_unchecked::<VNRequest>(request.clone()) };
        let requests = NSArray::from_retained_slice(&[as_request]);
        let handler = unsafe { VNSequenceRequestHandler::init(VNSequenceRequestHandler::alloc()) };
        Ok(Self { requests, request, handler, w, h, bgra: vec![255; (w * h * 4) as usize] })
    }

    /// The matte of one picture (RGB, `w`×`h`): 255 = person, 0 = room.
    pub(super) fn run(&mut self, rgb: &[u8], matte: &mut [u8]) -> Result<()> {
        for (px, out) in rgb.as_chunks::<3>().0.iter().zip(self.bgra.as_chunks_mut::<4>().0) {
            *out = [px[2], px[1], px[0], 255];
        }
        let frame = pixel_buffer(&mut self.bgra, self.w, self.h)?;
        // SAFETY: the buffer and the requests outlive the call, which is
        // synchronous, and this `Model` is only ever used from one thread.
        unsafe { self.handler.performRequests_onCVPixelBuffer_error(&self.requests, &frame) }
            .map_err(|e| anyhow::anyhow!("cutting the person out: {e}"))?;
        // Nobody in the picture: all room, which is not an error.
        let Some(mask) = unsafe { self.request.results() }.and_then(|r| r.firstObject()) else {
            matte.fill(0);
            return Ok(());
        };
        let mask = unsafe { mask.pixelBuffer() };
        let (mw, mh) = (CVPixelBufferGetWidth(&mask), CVPixelBufferGetHeight(&mask));
        if CVPixelBufferGetPixelFormatType(&mask) != kCVPixelFormatType_OneComponent8 {
            bail!("the person mask came back in an unexpected pixel format");
        }
        let gray = read_gray(&mask, mw, mh)?;
        // Vision picks the mask's own size, smaller than the picture.
        if (mw as u32, mh as u32) == (self.w, self.h) {
            matte.copy_from_slice(&gray);
        } else {
            let small = image::GrayImage::from_raw(mw as u32, mh as u32, gray).expect("mask size");
            let scaled = image::imageops::resize(&small, self.w, self.h, image::imageops::FilterType::Triangle);
            matte.copy_from_slice(scaled.as_raw());
        }
        Ok(())
    }
}

/// A pixel buffer over `bgra`, which has to outlive it. Lending the pixels
/// rather than copying them is the point: this runs per frame.
fn pixel_buffer(bgra: &mut [u8], w: u32, h: u32) -> Result<Retained<CVPixelBuffer>> {
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: `bgra` is w*h*4 bytes of our own and stays put for the call; no
    // release callback, because the pixels are not the buffer's to free.
    let status = unsafe {
        CVPixelBufferCreateWithBytes(
            None,
            w as usize,
            h as usize,
            kCVPixelFormatType_32BGRA,
            NonNull::new(bgra.as_mut_ptr().cast::<c_void>()).expect("frame pixels"),
            (w * 4) as usize,
            None,
            std::ptr::null_mut(),
            None,
            NonNull::from(&mut out),
        )
    };
    if status != 0 {
        bail!("could not wrap the camera frame for Vision ({status})");
    }
    // SAFETY: CoreVideo handed us one reference, which we take ownership of.
    unsafe { Retained::from_raw(out) }.ok_or_else(|| anyhow::anyhow!("no pixel buffer for the camera frame"))
}

/// The grey pixels of `mask`, row padding removed.
fn read_gray(mask: &CVPixelBuffer, w: usize, h: usize) -> Result<Vec<u8>> {
    // SAFETY: read-only lock, released below before the buffer is dropped.
    if unsafe { CVPixelBufferLockBaseAddress(mask, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        bail!("could not read the person mask");
    }
    let base = CVPixelBufferGetBaseAddress(mask);
    let stride = CVPixelBufferGetBytesPerRow(mask);
    let mut gray = vec![0u8; w * h];
    if !base.is_null() {
        for y in 0..h {
            // SAFETY: inside the locked buffer, one row at a time.
            let row = unsafe { std::slice::from_raw_parts(base.cast::<u8>().add(y * stride), w) };
            gray[y * w..][..w].copy_from_slice(row);
        }
    }
    // SAFETY: matches the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(mask, CVPixelBufferLockFlags::ReadOnly) };
    if base.is_null() {
        bail!("the person mask had no pixels");
    }
    Ok(gray)
}
