//! Background worker that owns the `scn::Workspace`. The UI sends
//! `Command`s; the worker runs them one at a time on a tokio runtime and
//! reports back through a `Sink` as plain-data `Event`s, so the UI thread
//! never blocks on disk or network.

use crate::config::{Config, Settings};
use crate::highlight::highlight;
use crate::model::{parse_conflict, parse_patch, LineKind, ParsedLine, MAX_DIFF_LINES};
use crate::{integrations, local_server, watch};
use anyhow::{bail, Context, Result};
use scion_proto::pb;
use scn::ops::{default_author, format_timestamp, ConflictSet, LandOutcome, Outcome, Resolution};
use scn::worktree::{ChangeKind, LocalChange};
use scn::Workspace;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

const HISTORY_PAGE: u32 = 200;
/// How often to check the server for new changes on the current branch.
const POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub enum Command {
    Open(PathBuf),
    /// "editor", "reveal", "terminal" or "remove" (from the recent list).
    RepoAction {
        action: String,
        path: PathBuf,
    },
    Clone {
        url: String,
        local: bool,
        dest: String,
        track: String,
        sparse: Vec<String>,
    },
    Refresh,
    /// Periodic background check for remote changes.
    Poll,
    TabChanged(usize),
    /// Shows the diff of a changed file (by repository path).
    SelectFile(String),
    ToggleBlame(bool),
    /// `paths: None` commits every change.
    Commit {
        summary: String,
        description: String,
        paths: Option<Vec<String>>,
    },
    /// Restores files; added files are moved to the trash.
    Discard(Vec<String>),
    /// The toolbar's main action: land if ahead, otherwise sync.
    LandOrSync,
    Sync,
    LoadHistory(String),
    LoadMore,
    ShowChangeset(String),
    ShowChangesetFile(usize),
    Revert(String),
    StartResolution,
    ResolveFile(String, Resolution),
    AbortResolution,
    SelectConflict(usize),
    LoadBranches,
    SwitchBranch(String),
    CreateBranch(String),
    RenameBranch(String, String),
    DeleteBranch(String),
    MergeBranch(String),
    /// `None` opens the repository folder.
    OpenInEditor(Option<String>),
    Reveal(Option<String>),
    OpenTerminal,
    SaveSettings(Settings),
    /// Remembers the sidebar width (silently, no status message).
    SaveSidebarWidth(f32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileItem {
    pub path: String,
    pub kind: &'static str,
    pub conflict: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogItem {
    pub id: String,
    pub short: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchItem {
    pub name: String,
    pub short: String,
    pub current: bool,
}

/// Contents of the main diff panel.
#[derive(Debug, Clone, Default)]
pub struct DiffDoc {
    pub title: String,
    pub lines: Vec<ParsedLine>,
    /// Show the blame gutter.
    pub gutter: bool,
    /// `None` hides the Blame/Diff toggle, `Some(on)` shows it.
    pub blame_toggle: Option<bool>,
    /// Set for a conflicted file: shows the resolve actions.
    pub conflict: Option<String>,
    /// Repository path of the file shown, for Open/Reveal.
    pub path: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Event {
    Busy(bool),
    Status {
        text: String,
        error: bool,
    },
    Error(String),
    Repo {
        name: String,
        root: String,
        tracked: String,
        base: String,
        info: String,
    },
    Recent(Vec<(String, String)>),
    Files {
        files: Vec<FileItem>,
        selected: Option<usize>,
    },
    Diff(DiffDoc),
    History(Vec<LogItem>),
    /// Header and file list of the changeset selected in History.
    Changeset {
        id: String,
        message: String,
        author: String,
        date: String,
        files: Vec<(String, &'static str)>,
    },
    Conflicts {
        rows: Vec<(String, String)>,
        first: Vec<ParsedLine>,
        can_resolve: bool,
    },
    ConflictDiff(Vec<ParsedLine>),
    Resolution {
        active: bool,
        unresolved: usize,
        message: Option<String>,
    },
    /// `unpublished`: the branch does not exist on the server yet.
    Compare {
        ahead: u64,
        behind: u64,
        unpublished: bool,
    },
    Branches(Vec<BranchItem>),
    Settings(Settings),
    /// Saved sidebar width, sent once at startup (0 = default).
    SidebarWidth(f32),
    CommitDone,
}

pub trait Sink: Send + 'static {
    fn send(&self, ev: Event);
}

/// What the Changes tab's main panel shows, so refreshes keep it current.
#[derive(Debug, Clone, PartialEq)]
enum View {
    Empty,
    FileDiff(String),
    FileBlame(String),
}

/// The changeset open in the History tab.
struct HistorySel {
    id: String,
    parent: String,
    files: Vec<pb::FileDiff>,
    file: Option<usize>,
    blame: bool,
}

pub struct Worker<S: Sink> {
    sink: S,
    tx: UnboundedSender<Command>,
    ws: Option<Workspace>,
    tab: usize,
    changes: Vec<LocalChange>,
    view: View,
    history_path: String,
    history_limit: u32,
    history_sel: Option<HistorySel>,
    conflict_set: Option<ConflictSet>,
    /// Whether the main toolbar action should land (vs. sync).
    should_land: bool,
    watcher: Option<notify::RecommendedWatcher>,
    config: Config,
    config_path: Option<PathBuf>,
    data_dir: PathBuf,
}

/// Starts the worker thread and returns the command channel.
pub fn spawn<S: Sink>(
    sink: S,
    config_path: Option<PathBuf>,
    data_dir: PathBuf,
) -> UnboundedSender<Command> {
    let (tx, mut rx) = unbounded_channel();
    let worker_tx = tx.clone();
    std::thread::Builder::new()
        .name("scion-worker".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("starting tokio runtime");
            rt.block_on(async move {
                let poll_tx = worker_tx.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(POLL_INTERVAL);
                    tick.tick().await;
                    loop {
                        tick.tick().await;
                        if poll_tx.send(Command::Poll).is_err() {
                            break;
                        }
                    }
                });
                let mut w = Worker::new(sink, worker_tx, config_path, data_dir);
                w.start().await;
                while let Some(cmd) = rx.recv().await {
                    w.run(cmd).await;
                }
            });
        })
        .expect("spawning worker thread");
    tx
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), directories::BaseDirs::new()) {
        (Some(rest), Some(d)) => d.home_dir().join(rest),
        _ => PathBuf::from(p),
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn change_kind(k: ChangeKind) -> &'static str {
    match k {
        ChangeKind::Added => "A",
        ChangeKind::Modified => "M",
        ChangeKind::Deleted => "D",
    }
}

fn diff_kind(f: &pb::FileDiff) -> &'static str {
    if f.old_blob.is_empty() {
        "A"
    } else if f.new_blob.is_empty() {
        "D"
    } else {
        "M"
    }
}

fn note(text: impl Into<String>) -> ParsedLine {
    ParsedLine::new(LineKind::Note, text)
}

impl<S: Sink> Worker<S> {
    pub fn new(
        sink: S,
        tx: UnboundedSender<Command>,
        config_path: Option<PathBuf>,
        data_dir: PathBuf,
    ) -> Self {
        let config = Config::load(config_path.as_deref());
        Worker {
            sink,
            tx,
            ws: None,
            tab: 0,
            changes: Vec::new(),
            view: View::Empty,
            history_path: String::new(),
            history_limit: HISTORY_PAGE,
            history_sel: None,
            conflict_set: None,
            should_land: false,
            watcher: None,
            config,
            config_path,
            data_dir,
        }
    }

    /// Publishes settings and the recent list, and reopens the last repository.
    pub async fn start(&mut self) {
        self.sink
            .send(Event::Settings(self.config.settings.clone()));
        self.sink
            .send(Event::SidebarWidth(self.config.sidebar_width));
        self.send_recent();
        let Some(last) = self.config.recent.first().cloned() else {
            return;
        };
        if !last.join(scn::state::DIR).is_dir() {
            return;
        }
        if let Err(e) = self.open(&last).await {
            self.status(format!("Could not reopen {}: {e:#}", last.display()), true);
        }
    }

    /// Runs one command, reporting failures. Background refreshes and polls
    /// are quiet: no busy indicator and errors only in the status bar.
    pub async fn run(&mut self, cmd: Command) {
        let quiet = matches!(cmd, Command::Refresh | Command::Poll);
        if !quiet {
            self.sink.send(Event::Busy(true));
        }
        if let Err(e) = self.handle(cmd).await {
            let msg = format!("{e:#}");
            if quiet {
                self.status(msg, true);
            } else {
                self.sink.send(Event::Error(msg));
            }
        }
        if !quiet {
            self.sink.send(Event::Busy(false));
        }
    }

    async fn handle(&mut self, cmd: Command) -> Result<()> {
        match cmd {
            Command::Open(path) => self.open(&path).await,
            Command::RepoAction { action, path } => {
                let s = &self.config.settings;
                match action.as_str() {
                    "editor" => integrations::open_in_editor(&s.editor, &path),
                    "reveal" => integrations::reveal(&path),
                    "terminal" => integrations::open_terminal(&s.terminal, &path),
                    "remove" => {
                        self.config.recent.retain(|p| p != &path);
                        self.save_config();
                        self.send_recent();
                        Ok(())
                    }
                    other => bail!("unknown repository action {other}"),
                }
            }
            Command::Clone {
                url,
                local,
                dest,
                track,
                sparse,
            } => {
                let url = if local {
                    local_server::ensure_running(&self.data_dir.join("server")).await?;
                    local_server::URL.to_string()
                } else {
                    url
                };
                if dest.trim().is_empty() {
                    bail!("choose a destination folder");
                }
                let dest = expand_home(dest.trim());
                let ws = Workspace::clone(&url, &dest, track.trim(), &sparse).await?;
                self.set_workspace(ws).await?;
                self.status(format!("Cloned into {}", dest.display()), false);
                Ok(())
            }
            Command::Refresh => self.refresh().await,
            Command::Poll => {
                if self.ws.is_some() {
                    self.compare().await?;
                }
                Ok(())
            }
            Command::TabChanged(tab) => {
                self.tab = tab;
                if tab == 0 {
                    self.refresh().await?;
                    self.show_view().await
                } else {
                    self.history().await?;
                    self.show_history_file().await
                }
            }
            Command::SelectFile(path) => {
                if !self.changes.iter().any(|c| c.path == path) {
                    bail!("{path} is no longer changed");
                }
                self.view = View::FileDiff(path);
                self.show_view().await
            }
            Command::ToggleBlame(on) => {
                if self.tab == 1 {
                    let sel = self
                        .history_sel
                        .as_mut()
                        .context("select a changeset first")?;
                    sel.blame = on;
                    return self.show_history_file().await;
                }
                self.view = match &self.view {
                    View::FileDiff(p) | View::FileBlame(p) if on => View::FileBlame(p.clone()),
                    View::FileDiff(p) | View::FileBlame(p) => View::FileDiff(p.clone()),
                    View::Empty => bail!("select a file first"),
                };
                self.show_view().await
            }
            Command::Commit {
                summary,
                description,
                paths,
            } => {
                let summary = summary.trim();
                if summary.is_empty() {
                    bail!("enter a commit summary first");
                }
                let message = match description.trim() {
                    "" => summary.to_string(),
                    d => format!("{summary}\n\n{d}"),
                };
                let author = match self.config.settings.author.trim() {
                    "" => default_author(),
                    a => a.to_string(),
                };
                let ws = self.ws()?;
                let id = ws.commit_paths(&message, &author, paths.as_deref()).await?;
                let tracked = ws.tracked().to_string();
                self.sink.send(Event::CommitDone);
                self.status(
                    format!(
                        "Created changeset {} — Land to publish it to {tracked}",
                        id.short()
                    ),
                    false,
                );
                self.view = View::Empty;
                self.after_change().await
            }
            Command::Discard(paths) => {
                let ws = self.ws.as_mut().context("no repository is open")?;
                let added: Vec<String> = ws
                    .status()?
                    .into_iter()
                    .filter(|c| c.kind == ChangeKind::Added && paths.contains(&c.path))
                    .map(|c| c.path)
                    .collect();
                for p in &added {
                    trash::delete(ws.root.join(p))
                        .with_context(|| format!("moving {p} to the trash"))?;
                }
                let restored = ws.discard(&paths, false).await?;
                self.status(
                    format!("Discarded changes to {} file(s)", restored + added.len()),
                    false,
                );
                self.after_change().await
            }
            Command::LandOrSync => {
                if self.should_land {
                    self.land().await
                } else {
                    self.sync().await
                }
            }
            Command::Sync => self.sync().await,
            Command::LoadHistory(path) => {
                self.history_path = path.trim().trim_matches('/').to_string();
                self.history_limit = HISTORY_PAGE;
                self.history().await
            }
            Command::LoadMore => {
                self.history_limit += HISTORY_PAGE;
                self.history().await
            }
            Command::ShowChangeset(id) => self.show_changeset(&id).await,
            Command::ShowChangesetFile(i) => {
                let sel = self
                    .history_sel
                    .as_mut()
                    .context("select a changeset first")?;
                sel.file = Some(i);
                sel.blame = false;
                self.show_history_file().await
            }
            Command::Revert(id) => {
                match self.ws()?.revert(&id).await? {
                    Outcome::Done(rid) => self.status(
                        format!(
                            "Created revert changeset {} — Land to publish it",
                            rid.short()
                        ),
                        false,
                    ),
                    Outcome::Conflicts(set) => self.show_conflicts(set),
                }
                self.after_change().await
            }
            Command::StartResolution => {
                let set = self
                    .conflict_set
                    .clone()
                    .context("no conflicts to resolve")?;
                let paths = self.ws()?.start_resolution(&set).await?;
                self.conflict_set = None;
                self.status(
                    format!(
                        "{} conflicted file(s) — resolve them, then commit",
                        paths.len()
                    ),
                    false,
                );
                self.tab = 0;
                self.after_change().await
            }
            Command::ResolveFile(path, choice) => {
                self.ws()?.resolve_conflict(&path, choice).await?;
                self.refresh().await?;
                self.show_view().await
            }
            Command::AbortResolution => {
                self.ws()?.abort_resolution().await?;
                self.status("Conflict resolution abandoned".into(), false);
                self.view = View::Empty;
                self.after_change().await
            }
            Command::SelectConflict(i) => {
                let c = self
                    .conflict_set
                    .as_ref()
                    .and_then(|s| s.conflicts.get(i))
                    .context("no such conflict")?;
                self.sink
                    .send(Event::ConflictDiff(parse_conflict(&c.detail)));
                Ok(())
            }
            Command::LoadBranches => self.branches().await,
            Command::SwitchBranch(name) => {
                let r = self.ws()?.switch_branch(&name).await?;
                self.status(
                    format!(
                        "Switched to {name} ({} written, {} removed)",
                        r.written, r.removed
                    ),
                    false,
                );
                self.view = View::Empty;
                self.history_sel = None;
                self.after_change().await?;
                self.branches().await
            }
            Command::CreateBranch(name) => {
                let name = name.trim().to_string();
                let at = self.ws()?.create_branch(&name, true).await?;
                self.status(
                    format!("Created and switched to {name} at {}", at.short()),
                    false,
                );
                self.after_change().await?;
                self.branches().await
            }
            Command::RenameBranch(old, new) => {
                self.ws()?.rename_branch(&old, new.trim()).await?;
                self.status(format!("Renamed {old} to {}", new.trim()), false);
                self.send_repo();
                self.branches().await
            }
            Command::DeleteBranch(name) => {
                self.ws()?.delete_branch(&name).await?;
                self.status(format!("Deleted {name}"), false);
                self.branches().await
            }
            Command::MergeBranch(name) => {
                let merge = self.config.settings.merge;
                match self.ws()?.merge_branch(&name, merge).await? {
                    LandOutcome::Conflicts(set) => self.show_conflicts(set),
                    LandOutcome::Landed { id, up_to_date, .. } => {
                        let tracked = self.ws()?.tracked().to_string();
                        if up_to_date {
                            self.status(format!("{tracked} already contains {name}"), false);
                        } else {
                            self.status(
                                format!("Merged {name} into {tracked} at {}", id.short()),
                                false,
                            );
                        }
                    }
                }
                self.after_change().await
            }
            Command::OpenInEditor(path) => {
                let target = self.abs(path.as_deref())?;
                integrations::open_in_editor(&self.config.settings.editor, &target)
            }
            Command::Reveal(path) => integrations::reveal(&self.abs(path.as_deref())?),
            Command::OpenTerminal => {
                let root = self.abs(None)?;
                integrations::open_terminal(&self.config.settings.terminal, &root)
            }
            Command::SaveSidebarWidth(w) => {
                self.config.sidebar_width = w;
                self.save_config();
                Ok(())
            }
            Command::SaveSettings(s) => {
                self.config.settings = s;
                self.save_config();
                self.sink
                    .send(Event::Settings(self.config.settings.clone()));
                self.status("Settings saved".into(), false);
                Ok(())
            }
        }
    }

    fn ws(&mut self) -> Result<&mut Workspace> {
        self.ws.as_mut().context("no repository is open")
    }

    fn abs(&self, path: Option<&str>) -> Result<PathBuf> {
        let ws = self.ws.as_ref().context("no repository is open")?;
        Ok(match path {
            Some(p) => ws.root.join(p),
            None => ws.root.clone(),
        })
    }

    fn status(&self, text: String, error: bool) {
        self.sink.send(Event::Status { text, error });
    }

    fn save_config(&self) {
        if let Some(p) = &self.config_path {
            if let Err(e) = self.config.save(p) {
                self.status(format!("Could not save settings: {e:#}"), true);
            }
        }
    }

    fn send_recent(&self) {
        let rows = self
            .config
            .recent
            .iter()
            .map(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (name, p.display().to_string())
            })
            .collect();
        self.sink.send(Event::Recent(rows));
    }

    async fn open(&mut self, path: &Path) -> Result<()> {
        let root = Workspace::find_root(path)?;
        let ws = match Workspace::open(&root).await {
            Ok(ws) => ws,
            // Working copies of the built-in server need it running first.
            Err(e) => {
                let state = scn::state::State::load(&root)?;
                if state.server != local_server::URL {
                    return Err(e);
                }
                local_server::ensure_running(&self.data_dir.join("server")).await?;
                Workspace::open(&root).await?
            }
        };
        self.set_workspace(ws).await
    }

    async fn set_workspace(&mut self, ws: Workspace) -> Result<()> {
        let root = ws.root.clone();
        self.watcher = None;
        self.ws = Some(ws);
        self.changes.clear();
        self.view = View::Empty;
        self.history_path.clear();
        self.history_limit = HISTORY_PAGE;
        self.history_sel = None;
        self.conflict_set = None;
        self.watcher = match watch::watch(root.clone(), self.tx.clone()) {
            Ok(w) => Some(w),
            Err(e) => {
                self.status(
                    format!("File watching unavailable ({e}); use Refresh"),
                    true,
                );
                None
            }
        };
        self.config.add_recent(root);
        self.save_config();
        self.send_recent();
        self.sink.send(Event::Diff(DiffDoc::default()));
        self.after_change().await
    }

    /// Refreshes everything that a commit, land, sync or branch switch can change.
    async fn after_change(&mut self) -> Result<()> {
        self.send_repo();
        self.refresh().await?;
        if self.tab == 0 {
            self.show_view().await?;
        }
        self.history().await?;
        self.compare().await
    }

    fn send_repo(&self) {
        let Some(ws) = &self.ws else { return };
        let name = ws
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let base = ws
            .base()
            .map(|b| b.short())
            .unwrap_or_else(|| "empty".into());
        self.sink.send(Event::Repo {
            name,
            root: ws.root.display().to_string(),
            tracked: ws.tracked().to_string(),
            info: format!("{} · {}", ws.server(), ws.root.display()),
            base,
        });
    }

    async fn compare(&mut self) -> Result<()> {
        let c = self.ws()?.compare().await?.unwrap_or_default();
        self.should_land = c.ahead > 0 || c.unpublished;
        self.sink.send(Event::Compare {
            ahead: c.ahead,
            behind: c.behind,
            unpublished: c.unpublished,
        });
        Ok(())
    }

    async fn refresh(&mut self) -> Result<()> {
        let ws = self.ws.as_ref().context("no repository is open")?;
        let changes = ws.status()?;
        let unresolved = ws.unresolved()?;
        let active = !ws.state.conflicts.is_empty();
        let message = ws.state.pending_message.clone();
        let selected = match &self.view {
            View::FileDiff(p) | View::FileBlame(p) => changes.iter().position(|c| &c.path == p),
            View::Empty => None,
        };
        let files = changes
            .iter()
            .map(|c| FileItem {
                path: c.path.clone(),
                kind: change_kind(c.kind),
                conflict: unresolved.contains(&c.path),
            })
            .collect();
        self.sink.send(Event::Files { files, selected });
        self.sink.send(Event::Resolution {
            active,
            unresolved: unresolved.len(),
            message,
        });
        let was_shown = selected.is_some();
        self.changes = changes;
        // Keep an open file diff current; clear it once the file is clean.
        if self.tab == 0 {
            match &self.view {
                View::FileDiff(_) if was_shown => self.show_view().await?,
                View::FileDiff(_) | View::FileBlame(_) if !was_shown => {
                    self.view = View::Empty;
                    self.sink.send(Event::Diff(DiffDoc::default()));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Renders the Changes tab's current view.
    async fn show_view(&mut self) -> Result<()> {
        let doc = match self.view.clone() {
            View::Empty => DiffDoc::default(),
            View::FileBlame(path) => {
                let lines = self.blame_lines(None, &path).await?;
                DiffDoc {
                    title: format!("Blame — {path}"),
                    lines,
                    gutter: true,
                    blame_toggle: Some(true),
                    path: Some(path),
                    ..Default::default()
                }
            }
            View::FileDiff(path) => {
                let c = self.changes.iter().find(|c| c.path == path).cloned();
                let Some(c) = c else {
                    self.view = View::Empty;
                    self.sink.send(Event::Diff(DiffDoc::default()));
                    return Ok(());
                };
                self.change_doc(&c).await?
            }
        };
        self.sink.send(Event::Diff(doc));
        Ok(())
    }

    async fn change_doc(&mut self, c: &LocalChange) -> Result<DiffDoc> {
        let ws = self.ws.as_mut().context("no repository is open")?;
        let entry = ws.state.conflicts.get(&c.path).cloned();
        let unresolved = ws.unresolved()?.contains(&c.path);
        let mut doc = DiffDoc {
            title: c.path.clone(),
            path: Some(c.path.clone()),
            blame_toggle: (c.kind != ChangeKind::Added).then_some(false),
            ..Default::default()
        };
        match entry {
            Some(e) if unresolved && e.kind == "content" => {
                let text = std::fs::read_to_string(ws.root.join(&c.path)).unwrap_or_default();
                doc.lines = parse_conflict(&text);
                doc.lines.insert(0, note("Conflicted: red is already landed, green is your changeset. Edit the file or pick a side."));
                doc.conflict = Some(c.path.clone());
                doc.blame_toggle = None;
            }
            Some(e) if unresolved => {
                let what = match e.kind.as_str() {
                    "delete/modify" => "deleted on the branch but modified in your changeset (your version is shown)",
                    "modify/delete" => "modified on the branch but deleted in your changeset (deleted here)",
                    _ => "changed on both sides; a binary file cannot be merged (your version is shown)",
                };
                let patch = ws.diff_change(c).await?;
                doc.lines = parse_patch(&patch, MAX_DIFF_LINES);
                doc.lines.insert(0, note(format!("Conflicted: {what}.")));
                doc.conflict = Some(c.path.clone());
            }
            _ => {
                let patch = ws.diff_change(c).await?;
                doc.lines = parse_patch(&patch, MAX_DIFF_LINES);
                highlight(&mut doc.lines, None);
            }
        }
        Ok(doc)
    }

    async fn blame_lines(&mut self, rev: Option<&str>, path: &str) -> Result<Vec<ParsedLine>> {
        let lines = self.ws()?.blame(rev, path).await?;
        let mut rows: Vec<ParsedLine> = lines
            .into_iter()
            .enumerate()
            .map(|(i, l)| {
                let mut row = ParsedLine::new(LineKind::Context, l.text.replace('\t', "    "));
                row.new_no = Some(i as u32 + 1);
                row.gutter = format!("{}  {}", &l.changeset[..l.changeset.len().min(8)], l.author);
                row
            })
            .collect();
        highlight(&mut rows, Some(path));
        Ok(rows)
    }

    async fn sync(&mut self) -> Result<()> {
        let r = self.ws()?.sync(None).await?;
        let to = r.to.map(|t| t.short()).unwrap_or_else(|| "empty".into());
        self.status(
            format!(
                "Synced to {to} ({} written, {} removed)",
                r.written, r.removed
            ),
            false,
        );
        self.after_change().await
    }

    async fn land(&mut self) -> Result<()> {
        let merge = self.config.settings.merge;
        match self.ws()?.land(None, merge).await? {
            LandOutcome::Conflicts(set) => self.show_conflicts(set),
            LandOutcome::Landed {
                id,
                fast_forward,
                up_to_date,
                attempts,
                sync_error,
            } => {
                let tracked = self.ws()?.tracked().to_string();
                let how = if up_to_date {
                    format!("{tracked} already contains it")
                } else if fast_forward {
                    "fast-forward".into()
                } else if merge {
                    "merged".into()
                } else {
                    "rebased".into()
                };
                match sync_error {
                    Some(e) => self.status(
                        format!(
                            "Landed {} ({how}), but the working copy was not updated: {e}",
                            id.short()
                        ),
                        true,
                    ),
                    None => self.status(
                        format!(
                            "Landed {} onto {tracked} ({how}, {attempts} attempt(s))",
                            id.short()
                        ),
                        false,
                    ),
                }
            }
        }
        self.after_change().await
    }

    fn show_conflicts(&mut self, set: ConflictSet) {
        let rows = set
            .conflicts
            .iter()
            .map(|c| (c.path.clone(), c.kind.clone()))
            .collect();
        let first = set
            .conflicts
            .first()
            .map(|c| parse_conflict(&c.detail))
            .unwrap_or_default();
        let can_resolve = set.conflicts.iter().all(|c| !c.path.is_empty());
        self.conflict_set = Some(set);
        self.sink.send(Event::Conflicts {
            rows,
            first,
            can_resolve,
        });
        self.status("Blocked by conflicts; nothing was landed".into(), true);
    }

    async fn branches(&mut self) -> Result<()> {
        let ws = self.ws()?;
        let current = ws.tracked().to_string();
        let mut items: Vec<BranchItem> = ws
            .refs()
            .await?
            .into_iter()
            .map(|r| BranchItem {
                current: r.name == current,
                short: short(&r.target).to_string(),
                name: r.name,
            })
            .collect();
        // Current branch first, as in GitHub Desktop's branch list.
        items.sort_by_key(|b| !b.current);
        if !items.iter().any(|b| b.current) {
            items.insert(
                0,
                BranchItem {
                    name: current,
                    short: "(not created yet)".into(),
                    current: true,
                },
            );
        }
        self.sink.send(Event::Branches(items));
        Ok(())
    }

    async fn history(&mut self) -> Result<()> {
        let path = (!self.history_path.is_empty()).then(|| self.history_path.clone());
        let limit = self.history_limit;
        let ws = self.ws()?;
        // An empty repository has no history yet.
        if ws.base().is_none() {
            self.sink.send(Event::History(vec![]));
            return Ok(());
        }
        let entries = ws.log(None, path.as_deref(), limit).await?;
        let items = entries
            .into_iter()
            .map(|c| LogItem {
                short: short(&c.id).to_string(),
                message: first_line(&c.message),
                author: c.author,
                date: format_timestamp(c.timestamp),
                id: c.id,
            })
            .collect();
        self.sink.send(Event::History(items));
        Ok(())
    }

    async fn show_changeset(&mut self, id: &str) -> Result<()> {
        let ws = self.ws()?;
        let info = ws.changeset(id).await?;
        let parent = info.parents.first().cloned().unwrap_or_default();
        let files = ws.changed_files(&parent, &info.id).await?;
        self.sink.send(Event::Changeset {
            id: info.id.clone(),
            message: info.message.clone(),
            author: info.author.clone(),
            date: format_timestamp(info.timestamp),
            files: files
                .iter()
                .map(|f| (f.path.clone(), diff_kind(f)))
                .collect(),
        });
        let file = (!files.is_empty()).then_some(0);
        self.history_sel = Some(HistorySel {
            id: info.id,
            parent,
            files,
            file,
            blame: false,
        });
        self.show_history_file().await
    }

    async fn show_history_file(&mut self) -> Result<()> {
        let Some(sel) = &self.history_sel else {
            self.sink.send(Event::Diff(DiffDoc::default()));
            return Ok(());
        };
        let Some(f) = sel.file.and_then(|i| sel.files.get(i)).cloned() else {
            self.sink.send(Event::Diff(DiffDoc {
                title: "No file changes".into(),
                ..Default::default()
            }));
            return Ok(());
        };
        let (id, parent, blame) = (sel.id.clone(), sel.parent.clone(), sel.blame);
        let exists = !f.new_blob.is_empty();
        let doc = if blame && exists {
            DiffDoc {
                title: format!("Blame — {} at {}", f.path, short(&id)),
                lines: self.blame_lines(Some(&id), &f.path).await?,
                gutter: true,
                blame_toggle: Some(true),
                path: Some(f.path.clone()),
                ..Default::default()
            }
        } else {
            let patch = self
                .ws()?
                .diff_revs(&parent, &id, vec![f.path.clone()])
                .await?;
            let mut lines = parse_patch(&patch, MAX_DIFF_LINES);
            highlight(&mut lines, None);
            DiffDoc {
                title: f.path.clone(),
                lines,
                blame_toggle: exists.then_some(false),
                path: Some(f.path.clone()),
                ..Default::default()
            }
        };
        self.sink.send(Event::Diff(doc));
        Ok(())
    }
}
