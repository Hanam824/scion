// No console window for release builds on Windows.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use scion_desktop::config::{self, Settings};
use scion_desktop::model::{self, ParsedLine, SplitRow};
use scion_desktop::worker::{self, BranchItem, Command, DiffDoc, Event, FileItem, LogItem, Sink};
use scn::ops::Resolution;
use slint::{ModelRc, SharedString, VecModel};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;

slint::include_modules!();

const MONO_FONT: &str = if cfg!(target_os = "macos") {
    "Menlo"
} else if cfg!(windows) {
    "Consolas"
} else {
    "DejaVu Sans Mono"
};

const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Show in Finder"
} else if cfg!(windows) {
    "Show in Explorer"
} else {
    "Show in file manager"
};

const THEMES: [&str; 3] = ["system", "light", "dark"];

/// Horizontal scroll width is capped so minified files don't explode it.
const MAX_SCROLL_CHARS: usize = 1000;

/// Which text-input dialog is open.
enum Input {
    NewBranch,
    Rename(String),
}

/// UI-thread state that isn't stored in Slint properties.
#[derive(Default)]
struct UiState {
    files: Vec<FileItem>,
    /// Unchecked files (new files start checked, as in GitHub Desktop).
    excluded: HashSet<String>,
    /// Indices into `files` shown after the Changes filter, in order.
    visible: Vec<usize>,
    selected_path: Option<String>,
    diff: Vec<ParsedLine>,
    diff_path: Option<String>,
    history: Vec<LogItem>,
    branches: Vec<BranchItem>,
    /// Recent repositories as (name, path), most recent first.
    recent: Vec<(String, String)>,
    /// Root of the open repository, for highlighting it in the flyout.
    current_root: String,
    /// Command waiting for the confirmation dialog.
    pending: Option<Command>,
    input: Option<Input>,
}

thread_local! {
    static STATE: RefCell<UiState> = RefCell::new(UiState::default());
}

fn with_state<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Delivers worker events onto the UI thread.
struct UiSink(slint::Weak<AppWindow>);

impl Sink for UiSink {
    fn send(&self, ev: Event) {
        let _ = self.0.upgrade_in_event_loop(move |ui| apply(&ui, ev));
    }
}

fn model<T: Clone + 'static>(rows: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(rows))
}

fn num(n: Option<u32>) -> SharedString {
    n.map(|n| n.to_string().into()).unwrap_or_default()
}

fn spans(line: &ParsedLine, highlight: bool) -> ModelRc<Span> {
    let spans = if highlight { &line.spans[..] } else { &[] };
    model(
        spans
            .iter()
            .map(|s| Span {
                text: s.text.as_str().into(),
                cls: s.class as i32,
            })
            .collect(),
    )
}

/// Changed files after the text and kind filters (`file-kind-filter`
/// indices match the menu in app.slint).
fn render_files(ui: &AppWindow) {
    let text = ui.get_file_filter().to_lowercase();
    let kind = ui.get_file_kind_filter();
    with_state(|st| {
        let excluded = &st.excluded;
        let keep = |f: &FileItem| {
            let included = !excluded.contains(&f.path);
            let by_kind = match kind {
                1 => included,
                2 => !included,
                3 => f.kind == "A",
                4 => f.kind == "M",
                5 => f.kind == "D",
                6 => f.conflict,
                _ => true,
            };
            by_kind && (text.is_empty() || f.path.to_lowercase().contains(&text))
        };
        st.visible = (0..st.files.len())
            .filter(|&i| keep(&st.files[i]))
            .collect();
        let rows: Vec<FileRow> = st
            .visible
            .iter()
            .map(|&i| {
                let f = &st.files[i];
                FileRow {
                    path: f.path.as_str().into(),
                    kind: f.kind.into(),
                    checked: !excluded.contains(&f.path),
                    conflict: f.conflict,
                }
            })
            .collect();
        let checked = st
            .files
            .iter()
            .filter(|f| !excluded.contains(&f.path))
            .count();
        ui.set_total_files(st.files.len() as i32);
        ui.set_checked_count(checked as i32);
        let selected = st
            .selected_path
            .as_ref()
            .and_then(|p| st.visible.iter().position(|&i| &st.files[i].path == p));
        ui.set_selected_file(selected.map_or(-1, |i| i as i32));
        ui.set_files(model(rows));
    });
}

/// Path of the `i`th visible changed file.
fn visible_path(i: i32) -> Option<String> {
    with_state(|st| {
        let idx = *st.visible.get(usize::try_from(i).ok()?)?;
        st.files.get(idx).map(|f| f.path.clone())
    })
}

/// "Ada Lovelace <ada@x.io>" -> "AL"
fn initials(name: &str) -> String {
    let name = name.split('<').next().unwrap_or(name);
    let letters: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters
    }
}

/// Rebuilds the diff models from the last document and the view options.
fn render_diff(ui: &AppWindow) {
    let (split, wrap) = (ui.get_split_view(), ui.get_wrap());
    with_state(|st| {
        let lines = &st.diff;
        let chars = lines
            .iter()
            .map(|l| l.text.chars().count())
            .max()
            .unwrap_or(0);
        ui.set_diff_chars(chars.min(MAX_SCROLL_CHARS) as i32);
        // Wrapped rows use a single Text, so spans (which can't wrap) are dropped.
        let hl = !wrap;
        ui.set_diff(model(
            lines
                .iter()
                .map(|l| DiffLine {
                    kind: l.kind as i32,
                    old_no: num(l.old_no),
                    new_no: num(l.new_no),
                    text: l.text.as_str().into(),
                    gutter: l.gutter.as_str().into(),
                    spans: spans(l, hl),
                })
                .collect(),
        ));
        let split_rows = if split {
            model::to_split(lines)
        } else {
            Vec::new()
        };
        ui.set_split_diff(model(
            split_rows
                .into_iter()
                .map(|r| match r {
                    SplitRow::Full(l) => SplitLine {
                        full: true,
                        kind: l.kind as i32,
                        text: l.text.into(),
                        ..Default::default()
                    },
                    SplitRow::Pair(left, right) => {
                        let side = |l: &Option<ParsedLine>, old: bool| match l {
                            Some(l) => (
                                l.kind as i32,
                                num(if old { l.old_no } else { l.new_no }),
                                l.text.as_str().into(),
                                spans(l, hl),
                            ),
                            None => (-1, SharedString::new(), SharedString::new(), model(vec![])),
                        };
                        let (lk, ln, lt, ls) = side(&left, true);
                        let (rk, rn, rt, rs) = side(&right, false);
                        SplitLine {
                            full: false,
                            kind: 0,
                            text: SharedString::new(),
                            left_kind: lk,
                            left_no: ln,
                            left_text: lt,
                            left_spans: ls,
                            right_kind: rk,
                            right_no: rn,
                            right_text: rt,
                            right_spans: rs,
                        }
                    }
                })
                .collect(),
        ));
    });
}

/// Repository flyout rows: the open repository under "Current repository",
/// then the rest under "Recent", filtered by name or path.
fn render_recent(ui: &AppWindow) {
    let filter = ui.get_repo_filter().to_lowercase();
    with_state(|st| {
        let matches = |name: &str, path: &str| {
            filter.is_empty()
                || name.to_lowercase().contains(&filter)
                || path.to_lowercase().contains(&filter)
        };
        let row = |name: &str, path: &str, current: bool, section: &str| RepoRow {
            name: name.into(),
            path: path.into(),
            current,
            section: section.into(),
        };
        let mut rows = Vec::new();
        if let Some((name, path)) = st.recent.iter().find(|(_, p)| *p == st.current_root) {
            if matches(name, path) {
                rows.push(row(name, path, true, "Current repository"));
            }
        }
        let others = st
            .recent
            .iter()
            .filter(|(n, p)| *p != st.current_root && matches(n, p));
        for (i, (name, path)) in others.enumerate() {
            rows.push(row(name, path, false, if i == 0 { "Recent" } else { "" }));
        }
        ui.set_recent(model(rows));
    });
}

fn render_branches(ui: &AppWindow) {
    let filter = ui.get_branch_filter().to_lowercase();
    with_state(|st| {
        let rows: Vec<BranchRow> = st
            .branches
            .iter()
            .filter(|b| filter.is_empty() || b.name.to_lowercase().contains(&filter))
            .map(|b| BranchRow {
                name: b.name.as_str().into(),
                short: b.short.as_str().into(),
                current: b.current,
            })
            .collect();
        ui.set_branches(model(rows));
    });
}

fn apply(ui: &AppWindow, ev: Event) {
    match ev {
        Event::Busy(b) => ui.set_busy(b),
        Event::Status { text, error } => {
            ui.set_status_text(text.into());
            ui.set_status_error(error);
        }
        Event::Error(e) => {
            ui.set_status_text(e.clone().into());
            ui.set_status_error(true);
            ui.set_error_text(e.into());
            ui.set_show_error(true);
        }
        Event::Repo {
            name,
            root,
            tracked,
            base,
            info,
        } => {
            with_state(|st| st.current_root = root);
            render_recent(ui);
            ui.set_has_repo(true);
            ui.set_repo_name(name.into());
            ui.set_branch(tracked.into());
            ui.set_base_short(base.into());
            ui.set_repo_info(info.into());
        }
        Event::Recent(rows) => {
            with_state(|st| st.recent = rows);
            render_recent(ui);
        }
        Event::Files { files, selected } => {
            with_state(|st| {
                // Forget exclusions for files that are no longer changed.
                st.excluded.retain(|p| files.iter().any(|f| &f.path == p));
                st.selected_path = selected.and_then(|i| files.get(i)).map(|f| f.path.clone());
                st.files = files;
            });
            render_files(ui);
        }
        Event::Diff(DiffDoc {
            title,
            lines,
            gutter,
            blame_toggle,
            conflict,
            path,
        }) => {
            ui.set_diff_title(title.into());
            ui.set_diff_gutter(gutter);
            ui.set_has_file(path.is_some());
            ui.set_blame_available(blame_toggle.is_some());
            ui.set_blame_mode(blame_toggle.unwrap_or(false));
            ui.set_conflict_path(conflict.unwrap_or_default().into());
            with_state(|st| {
                st.diff = lines;
                st.diff_path = path;
            });
            render_diff(ui);
        }
        Event::History(items) => {
            ui.set_history(model(
                items
                    .iter()
                    .map(|c| LogRow {
                        id: c.id.as_str().into(),
                        short: c.short.as_str().into(),
                        message: c.message.as_str().into(),
                        author: c.author.as_str().into(),
                        date: c.date.as_str().into(),
                    })
                    .collect(),
            ));
            let selected = ui.get_cs_id().to_string();
            ui.set_selected_change(
                items
                    .iter()
                    .position(|c| c.id == selected)
                    .map_or(-1, |i| i as i32),
            );
            with_state(|st| st.history = items);
        }
        Event::Changeset {
            id,
            message,
            author,
            date,
            files,
        } => {
            let (summary, body) = message.split_once('\n').unwrap_or((&message, ""));
            ui.set_has_changeset(true);
            ui.set_cs_summary(summary.into());
            ui.set_cs_body(body.trim().into());
            ui.set_cs_meta(format!("{author} · {date} · {}", &id[..id.len().min(12)]).into());
            ui.set_cs_id(id.into());
            ui.set_cs_files(model(
                files
                    .into_iter()
                    .map(|(path, kind)| ChangeRow {
                        path: path.into(),
                        kind: kind.into(),
                    })
                    .collect(),
            ));
            ui.set_cs_selected_file(0);
        }
        Event::Conflicts {
            rows,
            first,
            can_resolve,
        } => {
            ui.set_conflicts(model(
                rows.into_iter()
                    .map(|(path, kind)| ConflictRow {
                        path: path.into(),
                        kind: kind.into(),
                    })
                    .collect(),
            ));
            ui.set_selected_conflict(0);
            ui.set_conflict_diff(conflict_rows(first));
            ui.set_can_resolve(can_resolve);
            ui.set_show_conflicts(true);
        }
        Event::ConflictDiff(lines) => ui.set_conflict_diff(conflict_rows(lines)),
        Event::Resolution {
            active,
            unresolved,
            message,
        } => {
            ui.set_resolution_active(active);
            ui.set_unresolved(unresolved as i32);
            if let Some(m) = message {
                if active && ui.get_summary().is_empty() {
                    let (summary, body) = m.split_once('\n').unwrap_or((&m, ""));
                    ui.set_summary(summary.into());
                    ui.set_description(body.trim().into());
                }
            }
        }
        Event::Compare {
            ahead,
            behind,
            unpublished,
        } => {
            ui.set_ahead(ahead as i32);
            ui.set_behind(behind as i32);
            ui.set_unpublished(unpublished);
        }
        Event::Branches(items) => {
            with_state(|st| st.branches = items);
            render_branches(ui);
        }
        Event::Settings(s) => {
            let author = if s.author.is_empty() {
                scn::ops::default_author()
            } else {
                s.author.clone()
            };
            ui.set_author_initials(initials(&author).into());
            ui.set_set_author(s.author.into());
            ui.set_set_editor(s.editor.into());
            ui.set_set_terminal(s.terminal.into());
            ui.set_set_strategy(i32::from(s.merge));
            let theme = THEMES.iter().position(|t| *t == s.theme).unwrap_or(0) as i32;
            ui.set_set_theme(theme);
            ui.invoke_apply_theme(theme);
        }
        Event::SidebarWidth(w) => {
            if w > 0.0 {
                ui.set_sidebar_width(w);
            }
        }
        Event::CommitDone => {
            ui.set_summary(SharedString::new());
            ui.set_description(SharedString::new());
            ui.set_coauthors(SharedString::new());
            ui.set_show_coauthors(false);
            with_state(|st| st.excluded.clear());
        }
    }
}

fn conflict_rows(lines: Vec<ParsedLine>) -> ModelRc<DiffLine> {
    model(
        lines
            .into_iter()
            .map(|l| DiffLine {
                kind: l.kind as i32,
                old_no: num(l.old_no),
                new_no: num(l.new_no),
                text: l.text.into(),
                gutter: SharedString::new(),
                spans: model(vec![]),
            })
            .collect(),
    )
}

/// A picked folder that already has content gets a new subfolder suggested.
fn suggest_dest(dir: PathBuf) -> SharedString {
    let empty = std::fs::read_dir(&dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true);
    let dest = if empty { dir } else { dir.join("scion-repo") };
    dest.display().to_string().into()
}

fn confirm(ui: &AppWindow, title: &str, text: &str, button: &str, cmd: Command) {
    with_state(|st| st.pending = Some(cmd));
    ui.set_confirm_title(title.into());
    ui.set_confirm_text(text.into());
    ui.set_confirm_button(button.into());
    ui.set_show_confirm(true);
}

fn ask(ui: &AppWindow, kind: Input, title: &str, label: &str, button: &str, value: &str) {
    with_state(|st| st.input = Some(kind));
    ui.set_input_title(title.into());
    ui.set_input_label(label.into());
    ui.set_input_button(button.into());
    ui.set_input_value(value.into());
    ui.set_show_input(true);
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    ui.set_mono_font(MONO_FONT.into());
    ui.set_reveal_label(REVEAL_LABEL.into());
    let tx = worker::spawn(
        UiSink(ui.as_weak()),
        config::default_path(),
        config::data_dir(),
    );

    // Wires a callback; `$ui` is a weak handle upgraded inside the closure.
    macro_rules! on {
        ($setter:ident, |$ui:ident, $send:ident $(, $arg:ident)*| $body:block) => {{
            let weak = ui.as_weak();
            let tx: UnboundedSender<Command> = tx.clone();
            ui.$setter(move |$($arg),*| {
                let Some($ui) = weak.upgrade() else { return };
                let $send = |c: Command| {
                    let _ = tx.send(c);
                };
                // Not every callback uses both.
                let _ = (&$ui, &$send);
                $body
            });
        }};
    }

    on!(on_open_repo, |ui, send| {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Open a Scion working copy")
            .pick_folder()
        {
            ui.set_show_repos(false);
            send(Command::Open(dir));
        }
    });
    on!(on_open_path, |ui, send, path| {
        send(Command::Open(PathBuf::from(path.as_str())))
    });
    on!(on_repo_filter_changed, |ui, send, _t| {
        render_recent(&ui)
    });
    on!(on_repo_action, |ui, send, action, path| {
        send(Command::RepoAction {
            action: action.to_string(),
            path: PathBuf::from(path.as_str()),
        })
    });
    on!(on_clone_browse, |ui, send| {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title("Choose where to clone")
            .pick_folder()
        {
            ui.set_clone_dest(suggest_dest(dir));
        }
    });
    on!(on_clone_submit, |ui, send| {
        let sparse = ui
            .get_clone_sparse()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        send(Command::Clone {
            url: ui.get_clone_url().trim().to_string(),
            local: ui.get_clone_local(),
            dest: ui.get_clone_dest().to_string(),
            track: ui.get_clone_track().to_string(),
            sparse,
        });
    });
    on!(on_tab_changed, |ui, send, t| {
        ui.set_has_file(false);
        send(Command::TabChanged(t as usize));
    });
    on!(on_refresh, |ui, send| { send(Command::Refresh) });

    // Changes
    on!(on_toggle_file, |ui, send, i| {
        if let Some(path) = visible_path(i) {
            with_state(|st| {
                if !st.excluded.remove(&path) {
                    st.excluded.insert(path);
                }
            });
            render_files(&ui);
        }
    });
    on!(on_file_filter_changed, |ui, send| { render_files(&ui) });
    on!(on_open_settings, |ui, send| { ui.set_show_settings(true) });
    on!(on_toggle_all, |ui, send| {
        with_state(|st| {
            if st.excluded.is_empty() {
                st.excluded = st.files.iter().map(|f| f.path.clone()).collect();
            } else {
                st.excluded.clear();
            }
        });
        render_files(&ui);
    });
    on!(on_select_file, |ui, send, i| {
        if let Some(path) = visible_path(i) {
            with_state(|st| st.selected_path = Some(path.clone()));
            send(Command::SelectFile(path));
        }
    });
    on!(on_file_menu, |ui, send, action, i| {
        let Some(path) = visible_path(i) else {
            return;
        };
        match action.as_str() {
            "discard" => confirm(
                &ui,
                "Discard changes?",
                &format!("Discard your changes to {path}? Added files are moved to the trash."),
                "Discard changes",
                Command::Discard(vec![path]),
            ),
            "open" => send(Command::OpenInEditor(Some(path))),
            _ => send(Command::Reveal(Some(path))),
        }
    });
    on!(on_discard_all, |ui, send| {
        let paths: Vec<String> = with_state(|st| st.files.iter().map(|f| f.path.clone()).collect());
        let text = format!(
            "Discard all changes to {} file(s)? Added files are moved to the trash.",
            paths.len()
        );
        confirm(
            &ui,
            "Discard all changes?",
            &text,
            "Discard all",
            Command::Discard(paths),
        );
    });
    on!(on_commit, |ui, send| {
        let paths = with_state(|st| {
            let any_excluded = st.files.iter().any(|f| st.excluded.contains(&f.path));
            any_excluded.then(|| {
                st.files
                    .iter()
                    .filter(|f| !st.excluded.contains(&f.path))
                    .map(|f| f.path.clone())
                    .collect()
            })
        });
        // Co-authors become trailers, in the format GitHub uses.
        let mut description = ui.get_description().trim().to_string();
        let trailers: Vec<String> = ui
            .get_coauthors()
            .split(',')
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| format!("Co-authored-by: {c}"))
            .collect();
        if !trailers.is_empty() {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(&trailers.join("\n"));
        }
        send(Command::Commit {
            summary: ui.get_summary().to_string(),
            description,
            paths,
        });
    });
    on!(on_land_or_sync, |ui, send| { send(Command::LandOrSync) });
    on!(on_sync, |ui, send| { send(Command::Sync) });
    on!(on_window_minimize, |ui, send| {
        ui.window().set_minimized(true)
    });
    on!(on_window_zoom, |ui, send| {
        let w = ui.window();
        w.set_maximized(!w.is_maximized());
    });
    on!(on_open_url, |ui, send, url| {
        if let Err(e) = opener::open(url.as_str()) {
            ui.set_status_text(format!("Could not open {url}: {e}").into());
            ui.set_status_error(true);
        }
    });
    on!(on_toggle_blame, |ui, send| {
        send(Command::ToggleBlame(ui.get_blame_mode()))
    });
    on!(on_diff_options_changed, |ui, send| { render_diff(&ui) });
    on!(on_conflict_action, |ui, send, action| {
        let path = ui.get_conflict_path().to_string();
        let choice = match action.as_str() {
            "mine" => Resolution::Mine,
            "theirs" => Resolution::Theirs,
            _ => Resolution::Done,
        };
        send(Command::ResolveFile(path, choice));
    });
    on!(on_file_action, |ui, send, action| {
        let path = with_state(|st| st.diff_path.clone());
        if action.as_str() == "open" {
            send(Command::OpenInEditor(path));
        } else {
            send(Command::Reveal(path));
        }
    });
    on!(on_abort_resolution, |ui, send| {
        confirm(
            &ui,
            "Abort conflict resolution?",
            "All changes in the working copy will be discarded and added files deleted. Your original changeset stays on the server.",
            "Abort",
            Command::AbortResolution,
        );
    });

    // History
    on!(on_history_filter_changed, |ui, send, t| {
        send(Command::LoadHistory(t.to_string()))
    });
    on!(on_load_more, |ui, send| { send(Command::LoadMore) });
    on!(on_select_change, |ui, send, i| {
        if let Some(id) = with_state(|st| st.history.get(i as usize).map(|c| c.id.clone())) {
            send(Command::ShowChangeset(id));
        }
    });
    on!(on_select_cs_file, |ui, send, i| {
        send(Command::ShowChangesetFile(i as usize))
    });
    on!(on_revert_changeset, |ui, send| {
        let id = ui.get_cs_id().to_string();
        let text = format!(
            "Create a new changeset that undoes \"{}\"? It is not landed until you click Land.",
            ui.get_cs_summary()
        );
        confirm(
            &ui,
            "Revert changeset?",
            &text,
            "Revert",
            Command::Revert(id),
        );
    });

    // Branches
    on!(on_open_branches, |ui, send| { send(Command::LoadBranches) });
    on!(on_branch_filter_changed, |ui, send, _t| {
        render_branches(&ui)
    });
    on!(on_switch_branch, |ui, send, name| {
        send(Command::SwitchBranch(name.to_string()))
    });
    on!(on_new_branch, |ui, send| {
        let text = format!(
            "Name of the new branch (created at {})",
            ui.get_base_short()
        );
        ask(
            &ui,
            Input::NewBranch,
            "Create a branch",
            &text,
            "Create branch",
            "",
        );
    });
    on!(on_rename_branch, |ui, send, name| {
        ask(
            &ui,
            Input::Rename(name.to_string()),
            &format!("Rename {name}"),
            "New name",
            "Rename",
            &name,
        );
    });
    on!(on_delete_branch, |ui, send, name| {
        let text = format!("Delete the branch {name}? Its changesets stay on the server.");
        confirm(
            &ui,
            "Delete branch?",
            &text,
            "Delete",
            Command::DeleteBranch(name.to_string()),
        );
    });
    on!(on_merge_branch, |ui, send, name| {
        let text = format!("Merge {name} into {} on the server?", ui.get_branch());
        confirm(
            &ui,
            "Merge branch?",
            &text,
            "Merge",
            Command::MergeBranch(name.to_string()),
        );
    });

    // Dialogs
    on!(on_input_accepted, |ui, send, value| {
        let value = value.trim().to_string();
        match with_state(|st| st.input.take()) {
            Some(Input::NewBranch) => send(Command::CreateBranch(value)),
            Some(Input::Rename(old)) => send(Command::RenameBranch(old, value)),
            None => {}
        }
    });
    on!(on_confirm_accepted, |ui, send| {
        if let Some(cmd) = with_state(|st| st.pending.take()) {
            send(cmd);
        }
    });
    on!(on_start_resolution, |ui, send| {
        send(Command::StartResolution)
    });
    on!(on_select_conflict, |ui, send, i| {
        send(Command::SelectConflict(i as usize))
    });
    on!(on_open_editor, |ui, send| {
        send(Command::OpenInEditor(None))
    });
    on!(on_reveal_repo, |ui, send| { send(Command::Reveal(None)) });
    on!(on_open_terminal, |ui, send| { send(Command::OpenTerminal) });
    on!(on_save_settings, |ui, send| {
        send(Command::SaveSettings(Settings {
            author: ui.get_set_author().trim().to_string(),
            editor: ui.get_set_editor().trim().to_string(),
            terminal: ui.get_set_terminal().trim().to_string(),
            merge: ui.get_set_strategy() == 1,
            theme: THEMES[ui.get_set_theme().clamp(0, 2) as usize].to_string(),
        }));
    });

    on!(on_sidebar_resized, |ui, send, w| {
        send(Command::SaveSidebarWidth(w))
    });

    ui.run()
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn avatar_initials() {
        assert_eq!(initials("Ada Lovelace <ada@x.io>"), "AL");
        assert_eq!(initials("tranthehuy"), "T");
        assert_eq!(initials("grace brewster hopper"), "GB");
        assert_eq!(initials(""), "?");
    }
}
