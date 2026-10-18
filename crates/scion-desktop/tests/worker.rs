//! Drives the desktop worker headless against a real in-process server.

use scion_desktop::model::LineKind;
use scion_desktop::worker::{Command, DiffDoc, Event, Sink, Worker};
use scn::ops::Resolution;
use std::path::Path;
use std::sync::mpsc;
use tokio::sync::mpsc::unbounded_channel;

struct ChanSink(mpsc::Sender<Event>);

impl Sink for ChanSink {
    fn send(&self, ev: Event) {
        let _ = self.0.send(ev);
    }
}

struct Harness {
    w: Worker<ChanSink>,
    rx: mpsc::Receiver<Event>,
}

impl Harness {
    fn new(data: &Path) -> Self {
        let (etx, rx) = mpsc::channel();
        let (ctx, _crx) = unbounded_channel();
        Harness {
            w: Worker::new(ChanSink(etx), ctx, None, data.to_path_buf()),
            rx,
        }
    }

    /// Runs a command and returns its events, failing on any error.
    async fn run(&mut self, cmd: Command) -> Vec<Event> {
        self.w.run(cmd).await;
        let ev: Vec<Event> = self.rx.try_iter().collect();
        for e in &ev {
            if let Event::Error(msg) = e {
                panic!("worker error: {msg}");
            }
        }
        ev
    }

    async fn run_err(&mut self, cmd: Command) -> String {
        self.w.run(cmd).await;
        self.rx
            .try_iter()
            .find_map(|e| match e {
                Event::Error(m) => Some(m),
                _ => None,
            })
            .expect("an error")
    }
}

fn files(ev: &[Event]) -> Vec<(String, &'static str, bool)> {
    ev.iter()
        .rev()
        .find_map(|e| match e {
            Event::Files { files, .. } => Some(
                files
                    .iter()
                    .map(|f| (f.path.clone(), f.kind, f.conflict))
                    .collect(),
            ),
            _ => None,
        })
        .expect("a Files event")
}

fn history(ev: &[Event]) -> Vec<String> {
    ev.iter()
        .rev()
        .find_map(|e| match e {
            Event::History(items) => Some(items.iter().map(|i| i.message.clone()).collect()),
            _ => None,
        })
        .expect("a History event")
}

fn diff(ev: &[Event]) -> DiffDoc {
    ev.iter()
        .rev()
        .find_map(|e| match e {
            Event::Diff(d) => Some(d.clone()),
            _ => None,
        })
        .expect("a Diff event")
}

fn compare(ev: &[Event]) -> (u64, u64, bool) {
    ev.iter()
        .rev()
        .find_map(|e| match e {
            Event::Compare {
                ahead,
                behind,
                unpublished,
            } => Some((*ahead, *behind, *unpublished)),
            _ => None,
        })
        .expect("a Compare event")
}

fn commit(summary: &str, paths: Option<Vec<&str>>) -> Command {
    Command::Commit {
        summary: summary.into(),
        description: String::new(),
        paths: paths.map(|p| p.into_iter().map(String::from).collect()),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn daily_workflow() {
    let srv = e2e::start_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("repo");
    let mut h = Harness::new(&tmp.path().join("data"));

    let ev = h
        .run(Command::Clone {
            url: srv.url.clone(),
            local: false,
            dest: dest.display().to_string(),
            track: "main".into(),
            sparse: vec![],
        })
        .await;
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::Repo { tracked, .. } if tracked == "main")));
    assert!(files(&ev).is_empty());

    // Highlighted diff of an added Rust file.
    std::fs::write(
        dest.join("main.rs"),
        "fn main() {\n    println!(\"hi\");\n}\n",
    )
    .unwrap();
    std::fs::write(dest.join("notes.txt"), "draft\n").unwrap();
    h.run(Command::Refresh).await;
    let d = diff(&h.run(Command::SelectFile("main.rs".into())).await);
    assert_eq!(d.path.as_deref(), Some("main.rs"));
    assert_eq!(d.blame_toggle, None, "added files have no blame");
    assert!(
        d.lines
            .iter()
            .any(|l| l.kind == LineKind::Add && !l.spans.is_empty()),
        "highlighted"
    );

    // Partial commit: only main.rs; then land it.
    let ev = h.run(commit("add main", Some(vec!["main.rs"]))).await;
    assert!(ev.iter().any(|e| matches!(e, Event::CommitDone)));
    assert_eq!(files(&ev), vec![("notes.txt".to_string(), "A", false)]);
    assert_eq!(
        compare(&ev),
        (0, 0, true),
        "main does not exist until the first land"
    );
    let ev = h.run(Command::LandOrSync).await;
    assert_eq!(compare(&ev), (0, 0, false));
    assert_eq!(history(&ev), vec!["add main"]);
    assert!(srv.repo.meta().get_ref("main").unwrap().is_some());

    // Discard a modification (restores the base version).
    std::fs::write(dest.join("main.rs"), "broken\n").unwrap();
    h.run(Command::Refresh).await;
    h.run(Command::Discard(vec!["main.rs".into()])).await;
    assert!(std::fs::read_to_string(dest.join("main.rs"))
        .unwrap()
        .starts_with("fn main()"));

    // History detail: files of the changeset and a per-file diff.
    let id = srv.repo.meta().get_ref("main").unwrap().unwrap().to_hex();
    let ev = h.run(Command::ShowChangeset(id.clone())).await;
    let cs_files = ev
        .iter()
        .find_map(|e| match e {
            Event::Changeset { files, .. } => Some(files.clone()),
            _ => None,
        })
        .expect("a Changeset event");
    assert_eq!(cs_files, vec![("main.rs".to_string(), "A")]);
    assert_eq!(diff(&ev).path.as_deref(), Some("main.rs"));

    // Blame of that file at the changeset.
    h.run(Command::TabChanged(1)).await;
    let d = diff(&h.run(Command::ToggleBlame(true)).await);
    assert!(d.gutter && d.lines.len() == 3);

    // Branches: create (switches), commit there, land, merge back into main.
    h.run(Command::TabChanged(0)).await;
    let ev = h.run(Command::CreateBranch("feature".into())).await;
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::Repo { tracked, .. } if tracked == "feature")));
    h.run(Command::Refresh).await;
    h.run(commit("notes", None)).await;
    h.run(Command::LandOrSync).await;
    assert!(h
        .run_err(Command::SwitchBranch("nope".into()))
        .await
        .contains("no branch named nope"));
    h.run(Command::SwitchBranch("main".into())).await;
    assert!(!dest.join("notes.txt").exists());
    h.run(Command::MergeBranch("feature".into())).await;
    assert_eq!(
        std::fs::read_to_string(dest.join("notes.txt")).unwrap(),
        "draft\n"
    );

    // Revert the merge result's notes change, then land the revert.
    let tip = srv.repo.meta().get_ref("main").unwrap().unwrap().to_hex();
    let ev = h.run(Command::Revert(tip)).await;
    assert_eq!(compare(&ev), (1, 0, false), "revert waits to be landed");
    assert!(!dest.join("notes.txt").exists());
    h.run(Command::LandOrSync).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conflict_resolution_flow() {
    let srv = e2e::start_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("repo");
    let mut h = Harness::new(&tmp.path().join("data"));
    h.run(Command::Clone {
        url: srv.url.clone(),
        local: false,
        dest: dest.display().to_string(),
        track: "main".into(),
        sparse: vec![],
    })
    .await;
    std::fs::write(dest.join("f.txt"), "one\ntwo\n").unwrap();
    std::fs::write(dest.join("g.txt"), "g\n").unwrap();
    h.run(Command::Refresh).await;
    h.run(commit("seed", None)).await;
    h.run(Command::LandOrSync).await;

    // Someone else lands a conflicting edit through the CLI library.
    let other = tmp.path().join("other");
    let mut o = scn::Workspace::clone(&srv.url, &other, "main", &[])
        .await
        .unwrap();
    std::fs::write(other.join("f.txt"), "ONE\ntwo\n").unwrap();
    o.commit("theirs", "bob").await.unwrap();
    o.land(None, false).await.unwrap();

    std::fs::write(dest.join("f.txt"), "uno\ntwo\n").unwrap();
    std::fs::write(dest.join("g.txt"), "mine\n").unwrap();
    h.run(Command::Refresh).await;
    h.run(commit("mine", None)).await;
    let ev = h.run(Command::LandOrSync).await;
    let (rows, can_resolve) = ev
        .iter()
        .find_map(|e| match e {
            Event::Conflicts {
                rows, can_resolve, ..
            } => Some((rows.clone(), *can_resolve)),
            _ => None,
        })
        .expect("a Conflicts event");
    assert_eq!(rows, vec![("f.txt".to_string(), "content".to_string())]);
    assert!(can_resolve);

    let ev = h.run(Command::StartResolution).await;
    assert!(ev.iter().any(|e| matches!(e, Event::Resolution { active: true, unresolved: 1, message: Some(m) } if m == "mine")));
    let changed = files(&ev);
    assert!(
        changed.contains(&("f.txt".to_string(), "M", true)),
        "{changed:?}"
    );
    assert!(
        changed.contains(&("g.txt".to_string(), "M", false)),
        "clean merge kept: {changed:?}"
    );

    let d = diff(&h.run(Command::SelectFile("f.txt".into())).await);
    assert_eq!(d.conflict.as_deref(), Some("f.txt"));
    assert!(d
        .lines
        .iter()
        .any(|l| l.kind == LineKind::Del && l.text == "ONE"));

    assert!(h
        .run_err(commit("too early", None))
        .await
        .contains("resolve conflicts first"));
    let ev = h
        .run(Command::ResolveFile("f.txt".into(), Resolution::Mine))
        .await;
    assert!(ev
        .iter()
        .any(|e| matches!(e, Event::Resolution { unresolved: 0, .. })));
    h.run(commit("mine", None)).await;
    h.run(Command::LandOrSync).await;
    assert_eq!(
        std::fs::read_to_string(dest.join("f.txt")).unwrap(),
        "uno\ntwo\n"
    );

    let mut check = scn::Workspace::clone(&srv.url, &tmp.path().join("check"), "main", &[])
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("check/g.txt")).unwrap(),
        "mine\n"
    );
    assert_eq!(check.log(None, None, 1).await.unwrap()[0].message, "mine");
}

#[tokio::test(flavor = "multi_thread")]
async fn errors_are_reported_not_panics() {
    let tmp = tempfile::tempdir().unwrap();
    let mut h = Harness::new(&tmp.path().join("data"));
    assert!(h
        .run_err(Command::Sync)
        .await
        .contains("no repository is open"));
    assert!(h
        .run_err(Command::Open(tmp.path().to_path_buf()))
        .await
        .contains("not inside a scn working copy"));
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_repository_from_recent_list() {
    let srv = e2e::start_server().await;
    let tmp = tempfile::tempdir().unwrap();
    let mut h = Harness::new(&tmp.path().join("data"));
    for name in ["one", "two"] {
        h.run(Command::Clone {
            url: srv.url.clone(),
            local: false,
            dest: tmp.path().join(name).display().to_string(),
            track: "main".into(),
            sparse: vec![],
        })
        .await;
    }
    let recent = |ev: &[Event]| -> Vec<String> {
        ev.iter()
            .rev()
            .find_map(|e| match e {
                Event::Recent(rows) => Some(rows.iter().map(|(n, _)| n.clone()).collect()),
                _ => None,
            })
            .expect("a Recent event")
    };
    let one = tmp.path().join("one").canonicalize().unwrap();
    let ev = h
        .run(Command::RepoAction {
            action: "remove".into(),
            path: one,
        })
        .await;
    assert_eq!(recent(&ev), vec!["two"]);
}
