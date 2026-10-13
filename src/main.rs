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
                    println!("OK: {} cuts, {} slides, {} camera ranges, {} overlays; {} after cuts",
                        p.cuts.len(), p.slides.len(), p.camera_ranges.len(), p.overlays.len(),
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
