//! Filesystem watcher that asks the worker to refresh `status` after edits,
//! debounced so a burst of writes (save, build, checkout) costs one rescan.

use crate::worker::Command;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use scn::worktree::Ignore;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

const DEBOUNCE: Duration = Duration::from_millis(300);

fn relevant(root: &Path, ignore: &Ignore, path: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rel| !ignore.is_ignored(rel))
}

/// Watches `root` until the returned watcher is dropped.
pub fn watch(root: PathBuf, tx: UnboundedSender<Command>) -> notify::Result<RecommendedWatcher> {
    let (ping, pings) = mpsc::channel::<()>();
    let ignore = Ignore::load(&root);
    let watched = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if ev.paths.iter().any(|p| relevant(&watched, &ignore, p)) {
                let _ = ping.send(());
            }
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    // Exits when the watcher (and with it the `ping` sender) is dropped.
    std::thread::spawn(move || {
        while pings.recv().is_ok() {
            while pings.recv_timeout(DEBOUNCE).is_ok() {}
            if tx.send(Command::Refresh).is_err() {
                break;
            }
        }
    });
    Ok(watcher)
}
