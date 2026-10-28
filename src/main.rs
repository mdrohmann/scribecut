mod app;
mod audio;
mod capture;
mod edit_file;
mod editor;
mod export;
mod geom;
mod media;
mod platform;
mod player;
mod project;
mod region;
mod script;
mod settings;
mod sources;
mod transcribe;
mod voiceover;

fn main() -> eframe::Result {
    gst::init().expect("failed to initialise GStreamer");
    let args: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    // `scribecut <video|project> --export <out.mp4>` renders without opening a window.
    if let [input, flag, out] = args.as_slice()
        && flag.as_os_str() == "--export"
    {
        if let Err(e) = headless_export(input, out) {
            eprintln!("export failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // `scribecut <video|project> --script <out.md>` writes the edited transcript.
    if let [input, flag, out] = args.as_slice()
        && flag.as_os_str() == "--script"
    {
        if let Err(e) = load_project(input).and_then(|p| Ok(std::fs::write(out, script::script(&p))?)) {
            eprintln!("script export failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // `scribecut <video|edit file> --transcribe` (re)writes the transcript files.
    if let [input, flag] = args.as_slice()
        && flag.as_os_str() == "--transcribe"
    {
        if let Err(e) = headless_transcribe(input) {
            eprintln!("transcription failed: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    // `scribecut <video|edit file> --check` validates the edit file (for agents).
    if let [input, flag] = args.as_slice()
        && flag.as_os_str() == "--check"
    {
        match project::Project::load(input) {
            Ok(p) => {
                let problems = edit_file::validate(&p);
                for problem in &problems {
                    println!("{problem}");
                }
                if problems.is_empty() {
                    println!("OK: {} cuts, {} slides, {} camera ranges, {} overlays, {} voiceovers; {} after cuts",
                        p.cuts.len(), p.slides.len(), p.camera_ranges.len(), p.overlays.len(), p.voiceovers.len(),
                        editor::fmt_time(p.output_duration()));
                } else {
                    std::process::exit(1);
                }
            }
            Err(e) => {
                println!("{e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    // Optional: a video or project file to open straight away.
    let open = args.first().cloned();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("scribecut")
            .with_app_id("scribecut")
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native("scribecut", options, Box::new(|cc| Ok(Box::new(app::App::new(cc, open)))))
}

fn load_project(input: &std::path::Path) -> anyhow::Result<project::Project> {
    project::Project::load(input)
}

fn headless_export(input: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let project = load_project(input)?;
    export::export(&project, out, |p| eprint!("\rexporting {:.0}%", p * 100.0))?;
    eprintln!();
    Ok(())
}

/// Transcribes a recording (named by its video or edit file) or any other
/// video, such as an export, writing `<video>.transcript.json` / `.txt` next
/// to the video. Never creates an edit file.
fn headless_transcribe(input: &std::path::Path) -> anyhow::Result<()> {
    let media = match input.extension().and_then(|e| e.to_str()) {
        Some("toml" | "json") => load_project(input)?.media,
        _ => input.to_path_buf(),
    };
    // Audio-only files (a re-recorded take) don't probe as videos; just try them.
    if media::probe(&media).is_ok_and(|info| !info.has_audio) {
        anyhow::bail!("{} has no audio track", media.display());
    }
    let out = edit_file::transcript_path(&media);
    if out.is_file() {
        eprintln!("replacing the existing transcript {}", out.display());
    }
    let words = transcribe::transcribe(&media, |s| match s {
        transcribe::Status::Downloading(p) => eprint!("\rdownloading model {:.0}%", p * 100.0),
        transcribe::Status::Transcribing(p) => eprint!("\rtranscribing {:.0}%", p * 100.0),
    })?;
    eprintln!();
    edit_file::write_transcript(&media, &words)?;
    eprintln!("{} words written to {}", words.len(), out.display());
    Ok(())
}
