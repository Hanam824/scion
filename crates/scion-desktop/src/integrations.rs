//! Hand-offs to other apps: external editor, file manager, terminal.

use anyhow::{bail, Context, Result};
use std::path::Path;
use std::process::Command;

/// Opens `path` in the configured editor command (e.g. `code -r`), falling
/// back to the system default app when none is set.
pub fn open_in_editor(editor: &str, path: &Path) -> Result<()> {
    let mut parts = editor.split_whitespace();
    let Some(program) = parts.next() else {
        return opener::open(path).with_context(|| format!("opening {}", path.display()));
    };
    match Command::new(program).args(parts).arg(path).spawn() {
        Ok(_) => Ok(()),
        // GUI apps on macOS get a minimal PATH; accept an app name instead.
        Err(_) if cfg!(target_os = "macos") => {
            Command::new("open")
                .args(["-a", editor])
                .arg(path)
                .spawn()
                .with_context(|| format!("starting {editor}"))?;
            Ok(())
        }
        Err(e) => Err(e).with_context(|| format!("starting {editor}")),
    }
}

/// Shows `path` selected in Finder / Explorer / the file manager.
pub fn reveal(path: &Path) -> Result<()> {
    opener::reveal(path).with_context(|| format!("revealing {}", path.display()))
}

/// Opens a terminal in `dir`, using `terminal` (app name or command) if set.
pub fn open_terminal(terminal: &str, dir: &Path) -> Result<()> {
    if cfg!(target_os = "macos") {
        let app = if terminal.is_empty() {
            "Terminal"
        } else {
            terminal
        };
        Command::new("open")
            .args(["-a", app])
            .arg(dir)
            .spawn()
            .with_context(|| format!("starting {app}"))?;
        return Ok(());
    }
    if cfg!(windows) {
        let mut cmd = if terminal.is_empty() {
            let mut c = Command::new("cmd");
            c.args(["/C", "start", "cmd"]);
            c
        } else {
            Command::new(terminal)
        };
        cmd.current_dir(dir)
            .spawn()
            .context("starting a terminal")?;
        return Ok(());
    }
    let candidates: Vec<&str> = if terminal.is_empty() {
        vec![
            "x-terminal-emulator",
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "xterm",
        ]
    } else {
        vec![terminal]
    };
    for c in &candidates {
        if Command::new(c).current_dir(dir).spawn().is_ok() {
            return Ok(());
        }
    }
    bail!(
        "no terminal found (tried {}); set one in Settings",
        candidates.join(", ")
    )
}
