//! Lets scribecut tell the wlroots screencast portal which monitor or window
//! to share, so the user picks it in scribecut's own lists instead of the
//! portal's crosshair (which can only pick monitors).
//!
//! The portal asks a chooser command whenever an app starts a screen share.
//! The chooser installed here answers with the choice scribecut has just
//! written, if there is a fresh one, and otherwise runs the portal's usual
//! picker, so screen sharing in other apps works as before.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

/// How long a choice stays valid: the portal asks right after scribecut
/// writes it, and a stale one must not answer another app's request.
const FRESH_SECONDS: u32 = 10;

/// The portal's default chooser on sway, used for everything scribecut did not ask for.
const FALLBACK: &str = "slurp -f 'Monitor: %o' -or";

fn script_path() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join("scribecut/portal-chooser"))
}

fn choice_path() -> Option<PathBuf> {
    Some(dirs::runtime_dir()?.join("scribecut-portal-choice"))
}

fn config_dir() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("xdg-desktop-portal-wlr"))
}

/// The config file the portal reads: one named after the desktop wins over
/// the generic `config`.
fn config_path() -> Option<PathBuf> {
    let dir = config_dir()?;
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let named = desktops.split(':').filter(|d| !d.is_empty()).map(|d| dir.join(d)).find(|p| p.exists());
    Some(named.unwrap_or_else(|| dir.join("config")))
}

fn script_text() -> String {
    format!(
        "#!/bin/sh\n\
         # Screen-share chooser for xdg-desktop-portal-wlr, installed by scribecut.\n\
         # Answers with the monitor or window scribecut has just asked for;\n\
         # every other request gets the portal's usual picker.\n\
         choice=\"${{XDG_RUNTIME_DIR:-/tmp}}/scribecut-portal-choice\"\n\
         if [ -n \"$(find \"$choice\" -newermt '-{FRESH_SECONDS} seconds' 2>/dev/null)\" ]; then\n\
         \tcat \"$choice\"\n\
         \trm -f \"$choice\"\n\
         \texit 0\n\
         fi\n\
         rm -f \"$choice\"\n\
         exec {FALLBACK}\n"
    )
}

/// Whether the portal is set up to use scribecut's chooser.
pub fn installed() -> bool {
    let (Some(script), Some(config)) = (script_path(), config_path()) else { return false };
    script.exists()
        && std::fs::read_to_string(config).is_ok_and(|c| {
            let script = script.to_string_lossy();
            c.lines().any(|l| l.trim().strip_prefix("chooser_cmd").is_some_and(|v| v.contains(&*script)))
        })
}

/// Writes the chooser script and points the portal at it. Refuses to change
/// an existing portal config that sets something else. The portal reads its
/// config at start, so it has to be restarted afterwards.
pub fn install() -> Result<PathBuf> {
    let script = script_path().context("no data dir")?;
    let config = config_path().context("no config dir")?;
    std::fs::create_dir_all(script.parent().unwrap())?;
    std::fs::write(&script, script_text())?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    if installed() {
        return Ok(config);
    }
    if config.exists() {
        bail!(
            "{} already exists; add these lines to its [screencast] section instead:\n\
             chooser_type=simple\nchooser_cmd={}",
            config.display(),
            script.display()
        );
    }
    std::fs::create_dir_all(config.parent().unwrap())?;
    std::fs::write(
        &config,
        format!(
            "# Written by scribecut: it picks the monitor or window to record itself.\n\
             # Other apps still get the usual monitor picker.\n\
             [screencast]\nchooser_type=simple\nchooser_cmd={}\n",
            script.display()
        ),
    )?;
    Ok(config)
}

/// Restarts the portal so it reads the new config. Ends any screen share in progress.
pub fn restart_portal() -> Result<()> {
    let ok = std::process::Command::new("systemctl")
        .args(["--user", "restart", "xdg-desktop-portal-wlr"])
        .stdin(std::process::Stdio::null())
        .status()
        .context("running systemctl")?
        .success();
    if !ok {
        bail!("could not restart xdg-desktop-portal-wlr");
    }
    Ok(())
}

/// What scribecut wants shared next.
pub enum Choice<'a> {
    Monitor(&'a str),
    /// A window, by its foreign-toplevel identifier.
    Window(&'a str),
}

/// Answers the portal's next chooser call. Dropping the guard removes an
/// unused choice, so it cannot answer a later request from another app.
pub fn choose(choice: Choice) -> Result<ChoiceGuard> {
    let path = choice_path().context("no runtime dir")?;
    let text = match choice {
        Choice::Monitor(name) => format!("Monitor: {name}"),
        Choice::Window(id) => format!("Window: {id}"),
    };
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(ChoiceGuard(path))
}

pub struct ChoiceGuard(PathBuf);

impl Drop for ChoiceGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the chooser script with and without a fresh choice. The fallback
    /// is swapped for `echo` so the test needs no screen.
    #[test]
    fn script_answers_once_then_falls_back() {
        let dir = std::env::temp_dir().join(format!("scribecut-chooser-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("chooser");
        std::fs::write(&file, script_text().replace(FALLBACK, "echo fallback")).unwrap();
        let run = || {
            let out = std::process::Command::new("sh").arg(&file).env("XDG_RUNTIME_DIR", &dir).output().unwrap();
            String::from_utf8(out.stdout).unwrap()
        };
        std::fs::write(dir.join("scribecut-portal-choice"), "Window: abc").unwrap();
        assert_eq!(run(), "Window: abc");
        assert_eq!(run(), "fallback\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
