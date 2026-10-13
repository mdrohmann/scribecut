mod app;
mod audio;
mod capture;
mod editor;
mod export;
mod geom;
mod media;
mod platform;
mod player;
mod project;
mod region;
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

fn headless_export(input: &std::path::Path, out: &std::path::Path) -> anyhow::Result<()> {
    let project = if input.extension().is_some_and(|e| e == "json") {
        project::Project::load(input)?
    } else {
        project::Project::load(&project::Project::path_for(input))?
    };
    export::export(&project, out, |p| eprint!("\rexporting {:.0}%", p * 100.0))?;
    eprintln!();
    Ok(())
}
