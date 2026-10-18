use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use scion_proto::pb;
use scn::ops::{
    default_author, format_timestamp, repo_path, ConflictSet, LandOutcome, Outcome, Resolution,
    SyncReport,
};
use scn::worktree::ChangeKind;
use scn::Workspace;
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

/// Scion client. Commits, merges and rebases run on the server.
#[derive(Parser)]
#[command(name = "scn", version)]
struct Cli {
    /// Machine-readable output (status, log, blame, land, ref list).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a working copy, optionally limited to some directories.
    Clone {
        server: String,
        dir: PathBuf,
        #[arg(long, default_value = "main")]
        track: String,
        #[arg(long = "sparse", value_name = "DIR")]
        sparse: Vec<String>,
    },
    /// Show uncommitted changes.
    Status,
    /// Create a changeset on the server from local changes (no ref moves).
    /// With paths, commit only those files.
    Commit {
        #[arg(short, long)]
        message: Option<String>,
        #[arg(long, env = "SCN_AUTHOR")]
        author: Option<String>,
        paths: Vec<String>,
    },
    /// Discard local changes to files (restore their base version).
    Restore {
        #[arg(required = true)]
        paths: Vec<String>,
        /// Also delete added (untracked) files.
        #[arg(long)]
        added: bool,
    },
    /// Move the working copy to a revision (default: tracked ref).
    Sync { rev: Option<String> },
    /// Land the committed changeset onto a ref (rebase by default).
    Land {
        #[arg(long)]
        onto: Option<String>,
        /// Create a merge changeset instead of rebasing.
        #[arg(long)]
        merge: bool,
        /// On conflicts, write them into the working copy to resolve.
        #[arg(long)]
        resolve: bool,
    },
    /// Create a changeset that undoes a revision (land it to publish).
    Revert { rev: String },
    /// Finish a conflict: keep the file as edited, or take one side.
    Resolve {
        path: String,
        /// Take the version from your changeset.
        #[arg(long, conflicts_with = "theirs")]
        mine: bool,
        /// Take the version that was already landed.
        #[arg(long)]
        theirs: bool,
    },
    /// Abandon an in-progress conflict resolution.
    Abort,
    #[command(subcommand)]
    Branch(BranchCmd),
    /// Rebase the committed changeset onto a revision on the server.
    Rebase {
        #[arg(long)]
        onto: Option<String>,
    },
    /// History, optionally of a path.
    Log {
        path: Option<String>,
        #[arg(long)]
        rev: Option<String>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: u32,
    },
    /// Per-line attribution of a file.
    Blame {
        path: String,
        #[arg(long)]
        rev: Option<String>,
    },
    /// Uncommitted changes, or `diff A [B]` between revisions (server-side).
    Diff { revs: Vec<String> },
    /// A changeset and its patch.
    Show { rev: Option<String> },
    #[command(subcommand)]
    Ref(RefCmd),
    #[command(subcommand)]
    Sparse(SparseCmd),
}

#[derive(Subcommand)]
enum BranchCmd {
    List,
    /// Create a branch at the current base.
    Create {
        name: String,
        #[arg(long)]
        switch: bool,
    },
    Switch {
        name: String,
    },
    Rename {
        old: String,
        new: String,
    },
    Delete {
        name: String,
    },
    /// Merge another branch into the current one (server-side).
    Merge {
        name: String,
        #[arg(long)]
        merge_commit: bool,
        #[arg(long)]
        resolve: bool,
    },
}

#[derive(Subcommand)]
enum RefCmd {
    List,
    /// Compare-and-swap a ref to a revision.
    Set {
        name: String,
        rev: String,
        #[arg(long)]
        expect: Option<String>,
    },
}

#[derive(Subcommand)]
enum SparseCmd {
    List,
    Add {
        dirs: Vec<String>,
    },
    Remove {
        dirs: Vec<String>,
    },
    /// Replace the sparse set; no dirs means a full checkout.
    Set {
        dirs: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

fn print_conflicts(conflicts: &[pb::Conflict], json: bool) {
    if json {
        let c: Vec<_> = conflicts
            .iter()
            .map(|c| json!({"path": c.path, "kind": c.kind, "detail": c.detail}))
            .collect();
        println!("{}", json!({"status": "conflicts", "conflicts": c}));
        return;
    }
    for c in conflicts {
        eprintln!(
            "CONFLICT ({}): {}",
            c.kind,
            if c.path.is_empty() { "<tree>" } else { &c.path }
        );
        if c.kind == "content" {
            eprintln!("{}", c.detail);
        } else {
            eprintln!("  {}", c.detail);
        }
    }
}

/// Prints conflicts and, with `resolve`, puts them in the working copy.
async fn conflicted(
    ws: &mut Workspace,
    set: &ConflictSet,
    resolve: bool,
    json: bool,
) -> Result<ExitCode> {
    print_conflicts(&set.conflicts, json);
    if resolve {
        let paths = ws.start_resolution(set).await?;
        eprintln!(
            "\nConflicts written to the working copy: {}.\nEdit them (or `scn resolve --mine/--theirs PATH`), then `scn commit` and `scn land`.",
            paths.join(", ")
        );
    } else if !json {
        eprintln!("\nRun again with --resolve to resolve these in the working copy.");
    }
    Ok(ExitCode::FAILURE)
}

fn print_sync(r: &SyncReport, verb: &str) {
    match r.to {
        Some(id) => println!(
            "{verb} {} ({} written, {} removed)",
            id.short(),
            r.written,
            r.removed
        ),
        None => println!("{verb}: repository is empty"),
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    let cwd = std::env::current_dir()?;
    if let Cmd::Clone {
        server,
        dir,
        track,
        sparse,
    } = &cli.cmd
    {
        let ws = Workspace::clone(server, dir, track, sparse).await?;
        match ws.base() {
            Some(b) => println!(
                "Cloned {track} at {} into {} ({} files)",
                b.short(),
                dir.display(),
                ws.state.files.len()
            ),
            None => println!(
                "Cloned empty repository into {} (tracking {track})",
                dir.display()
            ),
        }
        return Ok(ExitCode::SUCCESS);
    }
    let root = Workspace::find_root(&cwd)?;
    let mut ws = Workspace::open(&root).await?;
    let rp = |p: &str| repo_path(&root, &cwd, p);

    match cli.cmd {
        Cmd::Clone { .. } => unreachable!(),
        Cmd::Status => {
            let changes = ws.status()?;
            if json {
                let c: Vec<_> = changes
                    .iter()
                    .map(|c| json!({"path": c.path, "kind": c.kind}))
                    .collect();
                println!(
                    "{}",
                    json!({"base": ws.base().map(|b| b.to_hex()), "changes": c})
                );
            } else {
                println!(
                    "On {} (branch {})",
                    ws.base()
                        .map(|b| b.short())
                        .unwrap_or_else(|| "<empty>".into()),
                    ws.state.track
                );
                if let Some(c) = ws.compare().await? {
                    if c.ahead > 0 || c.behind > 0 {
                        println!("{} to land, {} to sync", c.ahead, c.behind);
                    }
                }
                let unresolved = ws.unresolved()?;
                if changes.is_empty() {
                    println!("nothing to commit, working copy clean");
                }
                for c in changes {
                    let k = if unresolved.contains(&c.path) {
                        'C'
                    } else {
                        match c.kind {
                            ChangeKind::Added => 'A',
                            ChangeKind::Modified => 'M',
                            ChangeKind::Deleted => 'D',
                        }
                    };
                    println!("{k} {}", c.path);
                }
                if !unresolved.is_empty() {
                    println!(
                        "\n{} unresolved conflict(s); see `scn resolve --help`",
                        unresolved.len()
                    );
                }
            }
        }
        Cmd::Commit {
            message,
            author,
            paths,
        } => {
            let message = message
                .or_else(|| ws.state.pending_message.clone())
                .context("a message is required (-m)")?;
            let paths = paths.iter().map(|p| rp(p)).collect::<Result<Vec<_>>>()?;
            let only = (!paths.is_empty()).then_some(paths.as_slice());
            let id = ws
                .commit_paths(&message, &author.unwrap_or_else(default_author), only)
                .await?;
            println!(
                "Created changeset {} (run `scn land` to publish it to {})",
                id.short(),
                ws.state.track
            );
        }
        Cmd::Sync { rev } => print_sync(&ws.sync(rev.as_deref()).await?, "Synced to"),
        Cmd::Restore { paths, added } => {
            let paths = paths.iter().map(|p| rp(p)).collect::<Result<Vec<_>>>()?;
            let n = ws.discard(&paths, added).await?;
            println!("Restored {n} file(s)");
        }
        Cmd::Revert { rev } => match ws.revert(&rev).await? {
            Outcome::Done(id) => println!(
                "Created revert changeset {} (run `scn land` to publish it)",
                id.short()
            ),
            Outcome::Conflicts(c) => return conflicted(&mut ws, &c, true, json).await,
        },
        Cmd::Resolve { path, mine, theirs } => {
            let choice = if mine {
                Resolution::Mine
            } else if theirs {
                Resolution::Theirs
            } else {
                Resolution::Done
            };
            ws.resolve_conflict(&rp(&path)?, choice).await?;
            let left = ws.unresolved()?;
            match left.len() {
                0 => println!("All conflicts resolved; run `scn commit` then `scn land`"),
                n => println!("{n} conflict(s) left: {}", left.join(", ")),
            }
        }
        Cmd::Abort => {
            ws.abort_resolution().await?;
            println!("Resolution abandoned; working copy restored");
        }
        Cmd::Branch(cmd) => match cmd {
            BranchCmd::List => {
                for r in ws.refs().await? {
                    let mark = if r.name == ws.state.track { '*' } else { ' ' };
                    println!("{mark} {}  {}", r.name, short(&r.target));
                }
            }
            BranchCmd::Create { name, switch } => {
                let at = ws.create_branch(&name, switch).await?;
                println!(
                    "Created {name} at {}{}",
                    at.short(),
                    if switch { "; switched to it" } else { "" }
                );
            }
            BranchCmd::Switch { name } => print_sync(
                &ws.switch_branch(&name).await?,
                &format!("Switched to {name} at"),
            ),
            BranchCmd::Rename { old, new } => {
                ws.rename_branch(&old, &new).await?;
                println!("Renamed {old} to {new}");
            }
            BranchCmd::Delete { name } => {
                ws.delete_branch(&name).await?;
                println!("Deleted {name}");
            }
            BranchCmd::Merge {
                name,
                merge_commit,
                resolve,
            } => match ws.merge_branch(&name, merge_commit).await? {
                LandOutcome::Conflicts(c) => return conflicted(&mut ws, &c, resolve, json).await,
                LandOutcome::Landed { id, .. } => {
                    println!("Merged {name} into {} at {}", ws.state.track, id.short())
                }
            },
        },
        Cmd::Land {
            onto,
            merge,
            resolve,
        } => {
            let target = onto.clone().unwrap_or_else(|| ws.state.track.clone());
            if !ws.status()?.is_empty() && !json {
                eprintln!("note: uncommitted changes are not included in the land");
            }
            match ws.land(onto.as_deref(), merge).await? {
                LandOutcome::Conflicts(c) => return conflicted(&mut ws, &c, resolve, json).await,
                LandOutcome::Landed {
                    id,
                    fast_forward,
                    up_to_date,
                    attempts,
                    sync_error,
                } => {
                    if json {
                        println!(
                            "{}",
                            json!({"status": "landed", "id": id.to_hex(), "fast_forward": fast_forward,
                            "up_to_date": up_to_date, "attempts": attempts, "sync_error": sync_error})
                        );
                    } else if up_to_date {
                        println!("Already landed: {target} is at {}", id.short());
                    } else {
                        let how = if fast_forward {
                            "fast-forward"
                        } else if merge {
                            "merged"
                        } else {
                            "rebased"
                        };
                        println!(
                            "Landed {} onto {target} ({how}, {attempts} attempt(s))",
                            id.short()
                        );
                    }
                    if let Some(e) = sync_error {
                        eprintln!("warning: landed, but could not update the working copy: {e}");
                    }
                }
            }
        }
        Cmd::Rebase { onto } => match ws.rebase(onto.as_deref()).await? {
            Outcome::Done(id) => println!("Rebased to {}", id.short()),
            Outcome::Conflicts(c) => return conflicted(&mut ws, &c, false, json).await,
        },
        Cmd::Log { path, rev, limit } => {
            let path = path.as_deref().map(rp).transpose()?;
            let entries = ws.log(rev.as_deref(), path.as_deref(), limit).await?;
            if json {
                let e: Vec<_> = entries
                    .iter()
                    .map(|c| json!({"id": c.id, "parents": c.parents, "author": c.author, "message": c.message,
                        "timestamp": c.timestamp, "generation": c.generation}))
                    .collect();
                println!("{}", serde_json::Value::Array(e));
            } else {
                for c in entries {
                    println!(
                        "{}  {}  {}",
                        short(&c.id),
                        format_timestamp(c.timestamp),
                        c.author
                    );
                    for line in c.message.lines() {
                        println!("    {line}");
                    }
                }
            }
        }
        Cmd::Blame { path, rev } => {
            let lines = ws.blame(rev.as_deref(), &rp(&path)?).await?;
            if json {
                let l: Vec<_> = lines
                    .iter()
                    .enumerate()
                    .map(|(i, l)| json!({"line": i + 1, "changeset": l.changeset, "author": l.author, "text": l.text}))
                    .collect();
                println!("{}", serde_json::Value::Array(l));
            } else {
                for (i, l) in lines.iter().enumerate() {
                    println!(
                        "{} {:<12.12} {:>4} | {}",
                        short(&l.changeset),
                        l.author,
                        i + 1,
                        l.text
                    );
                }
            }
        }
        Cmd::Diff { revs } => {
            let out = match revs.as_slice() {
                [] => ws.diff_local().await?,
                [a] => {
                    let to = ws
                        .base()
                        .map(|b| b.to_hex())
                        .context("working copy has no base")?;
                    ws.diff_revs(a, &to, vec![]).await?
                }
                [a, b] => ws.diff_revs(a, b, vec![]).await?,
                _ => anyhow::bail!("usage: scn diff [FROM [TO]]"),
            };
            print!("{out}");
        }
        Cmd::Show { rev } => {
            let (info, patch) = ws.show(rev.as_deref()).await?;
            println!("changeset {}", info.id);
            if info.parents.len() > 1 {
                println!(
                    "Merge: {}",
                    info.parents
                        .iter()
                        .map(|p| short(p))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            println!(
                "Author: {}\nDate:   {}\n",
                info.author,
                format_timestamp(info.timestamp)
            );
            for line in info.message.lines() {
                println!("    {line}");
            }
            println!();
            print!("{patch}");
        }
        Cmd::Ref(RefCmd::List) => {
            let refs = ws.refs().await?;
            if json {
                let r: Vec<_> = refs
                    .iter()
                    .map(|r| json!({"name": r.name, "target": r.target}))
                    .collect();
                println!("{}", serde_json::Value::Array(r));
            } else {
                for r in refs {
                    println!("{}  {}", short(&r.target), r.name);
                }
            }
        }
        Cmd::Ref(RefCmd::Set { name, rev, expect }) => {
            let r = ws.set_ref(&name, &rev, expect.as_deref()).await?;
            if !r.ok {
                eprintln!(
                    "ref {name} was not updated: it is at {}",
                    if r.current.is_empty() {
                        "<none>"
                    } else {
                        short(&r.current)
                    }
                );
                return Ok(ExitCode::FAILURE);
            }
            println!("{name} -> {}", short(&r.current));
        }
        Cmd::Sparse(cmd) => {
            let current = ws.state.sparse.clone();
            let to_repo =
                |dirs: Vec<String>| dirs.iter().map(|d| rp(d)).collect::<Result<Vec<_>>>();
            let new = match cmd {
                SparseCmd::List => {
                    if current.is_empty() {
                        println!("(full checkout)");
                    }
                    for p in current {
                        println!("{p}");
                    }
                    return Ok(ExitCode::SUCCESS);
                }
                SparseCmd::Set { dirs } => to_repo(dirs)?,
                SparseCmd::Add { dirs } => {
                    if current.is_empty() {
                        anyhow::bail!(
                            "this is a full checkout; use `scn sparse set DIR...` to narrow it"
                        );
                    }
                    current.into_iter().chain(to_repo(dirs)?).collect()
                }
                SparseCmd::Remove { dirs } => {
                    let remove = to_repo(dirs)?;
                    let rest: Vec<String> = current
                        .into_iter()
                        .filter(|p| !remove.contains(p))
                        .collect();
                    if rest.is_empty() {
                        anyhow::bail!("removing every sparse directory would mean a full checkout; use `scn sparse set`");
                    }
                    rest
                }
            };
            print_sync(&ws.set_sparse(&new).await?, "Updated sparse checkout at");
        }
    }
    Ok(ExitCode::SUCCESS)
}
