//! Turns slide files (images, PDF pages, presentations, video clips) into
//! something the preview and ffmpeg can show. Renders are cached in
//! `.scribecut-cache/` next to the recording.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::project::{Project, Slide};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Pdf,
    /// PowerPoint / LibreOffice / Keynote, converted to PDF with LibreOffice.
    Presentation,
    Video,
}

pub fn kind(file: &Path) -> Option<Kind> {
    let ext = file.extension()?.to_str()?.to_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" => Kind::Image,
        "pdf" => Kind::Pdf,
        "pptx" | "ppt" | "odp" | "key" => Kind::Presentation,
        "mp4" | "mkv" | "webm" | "mov" | "m4v" | "avi" => Kind::Video,
        _ => return None,
    })
}

/// File extensions for the "add slide" file picker.
pub const EXTENSIONS: &[&str] =
    &["png", "jpg", "jpeg", "pdf", "pptx", "ppt", "odp", "key", "mp4", "mkv", "webm", "mov", "m4v"];

/// Cheap checks for `--check`: the file exists, is a known kind, and PDF pages exist.
pub fn check(p: &Project, slide: &Slide) -> Result<()> {
    let path = p.resolve(&slide.file);
    if !path.is_file() {
        bail!("file {} not found", slide.file.display());
    }
    match kind(&path) {
        None => bail!("{}: unsupported file type", slide.file.display()),
        Some(Kind::Pdf) => {
            let pages = pdf_pages(&path)?;
            if slide.page > pages {
                bail!("page {} but {} has only {pages}", slide.page, slide.file.display());
            }
        }
        _ => {}
    }
    Ok(())
}

/// What ffmpeg should read for a slide.
pub enum Input {
    /// A still image, looped for the slide's duration.
    Still(PathBuf),
    /// A video clip, starting `start` seconds in.
    Clip { path: PathBuf, start: f64 },
}

pub fn export_input(p: &Project, slide: &Slide) -> Result<Input> {
    let path = p.resolve(&slide.file);
    Ok(match kind(&path) {
        Some(Kind::Video) => Input::Clip { path, start: slide.clip_start },
        _ => Input::Still(still(p, slide, p.height.max(360))?),
    })
}

/// An image for the slide: the image itself, a rendered page, or a clip's first frame.
/// `height` is the wanted pixel height for rendered pages.
pub fn still(p: &Project, slide: &Slide, height: u32) -> Result<PathBuf> {
    let path = p.resolve(&slide.file);
    match kind(&path).with_context(|| format!("{}: unsupported file type", slide.file.display()))? {
        Kind::Image => Ok(path),
        Kind::Pdf => pdf_page(p, &path, slide.page, height),
        Kind::Presentation => {
            let pdf = presentation_pdf(p, &path)?;
            pdf_page(p, &pdf, slide.page, height)
        }
        Kind::Video => video_frame(p, &path, slide.clip_start),
    }
}

fn cache_dir(p: &Project) -> Result<PathBuf> {
    let dir = p.base_dir().join(".scribecut-cache");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A cache key that changes when the source file changes.
fn key(path: &Path, extra: impl Hash) -> Result<String> {
    let meta = std::fs::metadata(path).with_context(|| format!("{} not found", path.display()))?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    meta.len().hash(&mut h);
    meta.modified().ok().hash(&mut h);
    extra.hash(&mut h);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    Ok(format!("{stem}-{:016x}", h.finish()))
}

fn run(cmd: &mut Command, what: &str) -> Result<()> {
    let out = cmd.stdin(Stdio::null()).output().with_context(|| format!("running {what}"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

fn pdf_pages(pdf: &Path) -> Result<u32> {
    let out = Command::new("pdfinfo").arg(pdf).stdin(Stdio::null()).output().context("running pdfinfo")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("Pages:").and_then(|n| n.trim().parse().ok()))
        .with_context(|| format!("{} doesn't look like a PDF", pdf.display()))
}

fn pdf_page(p: &Project, pdf: &Path, page: u32, height: u32) -> Result<PathBuf> {
    let base = cache_dir(p)?.join(key(pdf, (page, height))?);
    let png = base.with_extension("png");
    if !png.is_file() {
        run(
            Command::new("pdftoppm")
                .args(["-png", "-singlefile", "-f", &page.to_string(), "-l", &page.to_string()])
                .args(["-scale-to-x", "-1", "-scale-to-y", &height.to_string()])
                .arg(pdf)
                .arg(&base),
            "pdftoppm",
        )?;
    }
    Ok(png)
}

fn presentation_pdf(p: &Project, file: &Path) -> Result<PathBuf> {
    let out_dir = cache_dir(p)?.join(key(file, "pdf")?);
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("slides");
    let pdf = out_dir.join(format!("{stem}.pdf"));
    if !pdf.is_file() {
        // A private profile so this works while LibreOffice is open elsewhere.
        std::fs::create_dir_all(&out_dir)?;
        let profile = cache_dir(p)?.join("libreoffice-profile");
        run(
            Command::new("soffice")
                .arg(format!("-env:UserInstallation=file://{}", profile.display()))
                .args(["--headless", "--convert-to", "pdf", "--outdir"])
                .arg(&out_dir)
                .arg(file),
            "LibreOffice (soffice)",
        )?;
        if !pdf.is_file() {
            bail!("LibreOffice did not produce a PDF for {}", file.display());
        }
    }
    Ok(pdf)
}

fn video_frame(p: &Project, clip: &Path, at: f64) -> Result<PathBuf> {
    let png = cache_dir(p)?.join(key(clip, at.to_bits())?).with_extension("png");
    if !png.is_file() {
        run(
            Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-y", "-ss", &format!("{at:.3}"), "-i"])
                .arg(clip)
                .args(["-frames:v", "1"])
                .arg(&png),
            "ffmpeg",
        )?;
    }
    Ok(png)
}
