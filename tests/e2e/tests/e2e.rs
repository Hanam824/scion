use anyhow::Result;
use e2e::start_server;
use scion_core::ObjectId;
use scn::ops::{LandOutcome, Outcome, Resolution};
use scn::Workspace;
use std::fs;
use std::path::Path;

fn write(root: &Path, path: &str, content: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path)).unwrap()
}

async fn clone(url: &str, dir: &Path, sparse: &[&str]) -> Workspace {
    let sparse: Vec<String> = sparse.iter().map(|s| s.to_string()).collect();
    Workspace::clone(url, dir, "main", &sparse).await.unwrap()
}

fn landed(o: LandOutcome) -> ObjectId {
    match o {
        LandOutcome::Landed {
            id,
            sync_error: None,
            ..
        } => id,
        LandOutcome::Landed {
            sync_error: Some(e),
            ..
        } => panic!("sync failed: {e}"),
        LandOutcome::Conflicts(c) => panic!("unexpected conflicts: {:?}", c.conflicts),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn commit_land_clone_log_blame() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let a = tmp.path().join("a");
    let mut ws = clone(&srv.url, &a, &[]).await;
    assert_eq!(ws.base(), None, "fresh repository is empty");

    write(&a, "README.md", "hello\n");
    write(&a, "src/main.rs", "fn main() {}\n");
    assert_eq!(ws.status()?.len(), 2);
    let c1 = ws.commit("initial", "alice").await?;
    assert!(ws.status()?.is_empty());
    assert_eq!(
        srv.repo.meta().get_ref("main")?,
        None,
        "commit does not move refs"
    );
    assert_eq!(
        landed(ws.land(None, false).await?),
        c1,
        "first land creates the ref"
    );

    write(&a, "README.md", "hello\nworld\n");
    ws.commit("second", "bob").await?;
    let c2 = landed(ws.land(None, false).await?);
    assert_eq!(srv.repo.meta().get_ref("main")?, Some(c2));

    let b = tmp.path().join("b");
    let mut wb = clone(&srv.url, &b, &[]).await;
    assert_eq!(read(&b, "README.md"), "hello\nworld\n");
    assert_eq!(read(&b, "src/main.rs"), "fn main() {}\n");

    let log = wb.log(None, None, 10).await?;
    assert_eq!(
        log.iter().map(|c| c.message.as_str()).collect::<Vec<_>>(),
        ["second", "initial"]
    );
    assert_eq!(wb.log(None, Some("src"), 10).await?.len(), 1);

    let blame = wb.blame(None, "README.md").await?;
    assert_eq!(
        blame.iter().map(|l| l.author.as_str()).collect::<Vec<_>>(),
        ["alice", "bob"]
    );
    assert_eq!(blame[1].text, "world");

    let patch = wb.diff_revs("main~1", "main", vec![]).await?;
    assert!(patch.contains("+world"), "{patch}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_lands_without_lock() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut seed = clone(&srv.url, &tmp.path().join("seed"), &[]).await;
    write(
        &seed.root,
        "shared.txt",
        &(1..=12).map(|n| format!("{n}\n")).collect::<String>(),
    );
    seed.commit("seed", "s").await?;
    landed(seed.land(None, false).await?);

    // Six clients each change a different file plus a different line of a
    // shared file (every other line: adjacent edits conflict, as in git),
    // then all land at once.
    let mut clients = Vec::new();
    for i in 0..6 {
        let mut ws = clone(&srv.url, &tmp.path().join(format!("c{i}")), &[]).await;
        write(&ws.root, &format!("own/{i}.txt"), &format!("client {i}\n"));
        let mut lines: Vec<String> = (1..=12).map(|n| n.to_string()).collect();
        lines[2 * i] = format!("edited by {i}");
        write(&ws.root, "shared.txt", &(lines.join("\n") + "\n"));
        ws.commit(&format!("client {i}"), "c").await?;
        clients.push(ws);
    }
    let mut tasks = Vec::new();
    for mut ws in clients {
        tasks.push(tokio::spawn(async move {
            ws.land(None, false).await.map(|o| (o, ws))
        }));
    }
    for t in tasks {
        let (o, _) = t.await??;
        landed(o);
    }

    let mut check = clone(&srv.url, &tmp.path().join("check"), &[]).await;
    for i in 0..6 {
        assert_eq!(
            read(&check.root, &format!("own/{i}.txt")),
            format!("client {i}\n")
        );
    }
    let shared = read(&check.root, "shared.txt");
    let expected: String = (0..12)
        .map(|n| {
            if n % 2 == 0 {
                format!("edited by {}\n", n / 2)
            } else {
                format!("{}\n", n + 1)
            }
        })
        .collect();
    assert_eq!(shared, expected);
    assert_eq!(
        check.log(None, None, 100).await?.len(),
        7,
        "linear history: seed + 6 rebased lands"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicting_land_reports_and_keeps_ref() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut a = clone(&srv.url, &tmp.path().join("a"), &[]).await;
    write(&a.root, "f.txt", "base\n");
    a.commit("base", "a").await?;
    landed(a.land(None, false).await?);

    let mut b = clone(&srv.url, &tmp.path().join("b"), &[]).await;
    write(&a.root, "f.txt", "from a\n");
    write(&b.root, "f.txt", "from b\n");
    a.commit("a edit", "a").await?;
    b.commit("b edit", "b").await?;
    let a_tip = landed(a.land(None, false).await?);

    match b.land(None, false).await? {
        LandOutcome::Conflicts(set) => {
            let c = &set.conflicts;
            assert_eq!(
                (c[0].path.as_str(), c[0].kind.as_str()),
                ("f.txt", "content")
            );
            assert!(c[0].detail.contains("<<<<<<< target"), "{}", c[0].detail);
        }
        LandOutcome::Landed { .. } => panic!("expected a conflict"),
    }
    assert_eq!(
        srv.repo.meta().get_ref("main")?,
        Some(a_tip),
        "ref unchanged after conflict"
    );

    // A server-side rebase of the same changeset hits the same conflict.
    assert!(matches!(b.rebase(None).await?, Outcome::Conflicts(_)));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_checkout_and_widening() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut full = clone(&srv.url, &tmp.path().join("full"), &[]).await;
    write(&full.root, "src/lib.rs", "lib\n");
    write(&full.root, "src/deep/mod.rs", "deep\n");
    write(&full.root, "docs/guide.md", "guide\n");
    write(&full.root, "README", "readme\n");
    full.commit("tree", "x").await?;
    landed(full.land(None, false).await?);

    let sp = tmp.path().join("sparse");
    let mut ws = clone(&srv.url, &sp, &["src"]).await;
    assert!(sp.join("src/deep/mod.rs").exists());
    assert!(!sp.join("docs").exists());
    assert!(!sp.join("README").exists());
    assert_eq!(ws.state.files.len(), 2);

    // Changes outside the sparse set still flow through sync.
    write(&full.root, "docs/guide.md", "guide v2\n");
    write(&full.root, "src/lib.rs", "lib v2\n");
    full.commit("update", "x").await?;
    landed(full.land(None, false).await?);
    let r = ws.sync(None).await?;
    assert_eq!((r.written, r.removed), (1, 0));
    assert_eq!(read(&sp, "src/lib.rs"), "lib v2\n");

    ws.set_sparse(&["src".into(), "docs".into()]).await?;
    assert_eq!(read(&sp, "docs/guide.md"), "guide v2\n");
    ws.set_sparse(&["docs".into()]).await?;
    assert!(!sp.join("src").exists());
    assert!(ws.status()?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_refuses_to_clobber_local_changes() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut a = clone(&srv.url, &tmp.path().join("a"), &[]).await;
    write(&a.root, "f", "1\n");
    a.commit("one", "a").await?;
    landed(a.land(None, false).await?);
    let mut b = clone(&srv.url, &tmp.path().join("b"), &[]).await;

    write(&a.root, "f", "2\n");
    a.commit("two", "a").await?;
    landed(a.land(None, false).await?);

    write(&b.root, "f", "local edit\n");
    let err = b.sync(None).await.unwrap_err().to_string();
    assert!(err.contains("would be overwritten"), "{err}");
    assert_eq!(read(&b.root, "f"), "local edit\n");

    // A local deletion of an incoming file also blocks the sync.
    fs::remove_file(b.root.join("f"))?;
    b.sync(None).await.unwrap_err();
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn git_import_roundtrip() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let g = tmp.path().join("gitrepo");
    fs::create_dir_all(&g)?;
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&g)
            .args([
                "-c",
                "user.name=Git User",
                "-c",
                "user.email=git@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    write(&g, "a.txt", "a1\n");
    write(&g, "dir/b.txt", "b1\n");
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "first"]);
    write(&g, "a.txt", "a1\na2\n");
    fs::remove_file(g.join("dir/b.txt"))?;
    write(&g, "run.sh", "#!/bin/sh\n");
    git(&["add", "-A"]);
    git(&["update-index", "--chmod=+x", "run.sh"]);
    git(&["commit", "-q", "-m", "second"]);

    let opts = scion_import::ImportOptions {
        git_dir: g.clone(),
        server: srv.url.clone(),
        branch: "main".into(),
        target_ref: "main".into(),
        state_file: tmp.path().join("import.json"),
    };
    let r = scion_import::import(&opts, |_, _| {}).await?;
    assert_eq!((r.imported, r.skipped), (2, 0));
    let again = scion_import::import(&opts, |_, _| {}).await?;
    assert_eq!(
        (again.imported, again.skipped, again.tip),
        (0, 2, r.tip),
        "resumable and idempotent"
    );

    let co = tmp.path().join("co");
    let mut ws = clone(&srv.url, &co, &[]).await;
    assert_eq!(read(&co, "a.txt"), "a1\na2\n");
    assert!(!co.join("dir").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            fs::metadata(co.join("run.sh"))?.permissions().mode() & 0o111,
            0,
            "executable bit kept"
        );
    }
    let log = ws.log(None, None, 10).await?;
    assert_eq!(
        log.iter().map(|c| c.message.as_str()).collect::<Vec<_>>(),
        ["second", "first"]
    );
    assert_eq!(log[0].author, "Git User <git@example.com>");
    let blame = ws.blame(None, "a.txt").await?;
    assert_eq!(blame.len(), 2);
    assert_ne!(blame[0].changeset, blame[1].changeset);
    Ok(())
}

/// Seeds a repository with `files`, landed on main, and returns a clone.
async fn seeded(url: &str, dir: &Path, files: &[(&str, &str)]) -> Workspace {
    let mut ws = clone(url, dir, &[]).await;
    for (p, c) in files {
        write(dir, p, c);
    }
    ws.commit("seed", "s").await.unwrap();
    landed(ws.land(None, false).await.unwrap());
    ws
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_commit_and_discard() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut ws = seeded(
        &srv.url,
        &tmp.path().join("a"),
        &[("a", "1\n"), ("b", "1\n")],
    )
    .await;
    write(&ws.root, "a", "2\n");
    write(&ws.root, "b", "2\n");
    write(&ws.root, "new", "x\n");

    ws.commit_paths("only a", "t", Some(&["a".to_string()]))
        .await?;
    let left: Vec<String> = ws.status()?.into_iter().map(|c| c.path).collect();
    assert_eq!(left, ["b", "new"], "uncommitted files stay changed");
    let log = ws.log(None, Some("b"), 10).await?;
    assert_eq!(log.len(), 1, "b was not part of the commit");

    assert_eq!(ws.discard(&["b".into(), "new".into()], false).await?, 1);
    assert_eq!(read(&ws.root, "b"), "1\n");
    assert!(
        ws.root.join("new").exists(),
        "added files kept without remove_added"
    );
    ws.discard(&["new".into()], true).await?;
    assert!(ws.status()?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_and_compare() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut ws = seeded(&srv.url, &tmp.path().join("a"), &[("f", "base\n")]).await;
    assert_eq!(
        ws.compare().await?.map(|c| (c.ahead, c.behind)),
        Some((0, 0))
    );

    ws.create_branch("feature", true).await?;
    assert_eq!(ws.tracked(), "feature");
    write(&ws.root, "feat.txt", "feature work\n");
    ws.commit("feature work", "t").await?;
    assert_eq!(ws.compare().await?.unwrap().ahead, 1);
    let err = ws.switch_branch("main").await.unwrap_err().to_string();
    assert!(err.contains("unlanded"), "{err}");
    landed(ws.land(None, false).await?);

    ws.switch_branch("main").await?;
    assert!(
        !ws.root.join("feat.txt").exists(),
        "main does not have the feature file"
    );
    match ws.merge_branch("feature", false).await? {
        LandOutcome::Landed { .. } => {}
        LandOutcome::Conflicts(c) => panic!("{:?}", c.conflicts),
    }
    assert_eq!(read(&ws.root, "feat.txt"), "feature work\n");

    ws.rename_branch("feature", "done").await?;
    let names: Vec<String> = ws.refs().await?.into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["done", "main"]);
    assert!(
        ws.delete_branch("main").await.is_err(),
        "current branch is protected"
    );
    ws.delete_branch("done").await?;
    assert!(ws.create_branch("bad name", false).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn revert_then_land() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut ws = seeded(&srv.url, &tmp.path().join("a"), &[("f", "1\n")]).await;
    write(&ws.root, "f", "2\n");
    let bad = ws.commit("bad change", "t").await?;
    landed(ws.land(None, false).await?);
    match ws.revert(&bad.to_hex()).await? {
        Outcome::Done(_) => {}
        Outcome::Conflicts(c) => panic!("{:?}", c.conflicts),
    }
    assert_eq!(read(&ws.root, "f"), "1\n");
    landed(ws.land(None, false).await?);
    let log = ws.log(None, None, 1).await?;
    assert!(
        log[0].message.starts_with("Revert \"bad change\""),
        "{}",
        log[0].message
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn resolve_conflicts_in_working_copy() -> Result<()> {
    let srv = start_server().await;
    let tmp = tempfile::tempdir()?;
    let mut a = seeded(
        &srv.url,
        &tmp.path().join("a"),
        &[("f", "1\n2\n3\n"), ("g", "g\n"), ("h", "h\n")],
    )
    .await;
    let mut b = clone(&srv.url, &tmp.path().join("b"), &[]).await;

    write(&a.root, "f", "1\nA\n3\n");
    write(&a.root, "h", "landed h\n");
    a.commit("a", "a").await?;
    landed(a.land(None, false).await?);

    write(&b.root, "f", "1\nB\n3\n");
    write(&b.root, "g", "b's g\n");
    write(&b.root, "h", "b's h\n");
    b.commit("b's work", "b").await?;
    let LandOutcome::Conflicts(set) = b.land(None, false).await? else {
        panic!("expected conflicts")
    };
    assert_eq!(set.conflicts.len(), 2);

    let paths = b.start_resolution(&set).await?;
    assert_eq!(paths, ["f", "h"]);
    assert!(read(&b.root, "f").contains("<<<<<<< target"));
    assert_eq!(read(&b.root, "g"), "b's g\n", "clean merges are kept");
    assert_eq!(b.unresolved()?, ["f", "h"]);
    assert!(
        b.commit("x", "b").await.is_err(),
        "commit blocked while unresolved"
    );

    write(&b.root, "f", "1\nA and B\n3\n");
    b.resolve_conflict("h", Resolution::Theirs).await?;
    assert_eq!(read(&b.root, "h"), "landed h\n");
    assert!(b.unresolved()?.is_empty());
    assert_eq!(b.state.pending_message.as_deref(), Some("b's work"));

    b.commit("b's work", "b").await?;
    assert!(b.state.conflicts.is_empty());
    landed(b.land(None, false).await?);
    let check = clone(&srv.url, &tmp.path().join("check"), &[]).await;
    assert_eq!(read(&check.root, "f"), "1\nA and B\n3\n");
    assert_eq!(read(&check.root, "g"), "b's g\n");
    assert_eq!(read(&check.root, "h"), "landed h\n");
    assert!(check.status()?.is_empty());
    Ok(())
}
