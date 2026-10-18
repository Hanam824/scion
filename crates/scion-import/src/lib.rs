//! Replays git history into Scion through the public RPC API, using git
//! plumbing commands. Each commit becomes a changeset listing its file
//! changes relative to its first parent; all parents are preserved.
//! Resumable: the git sha -> changeset id mapping is saved to a state file,
//! and changeset ids are deterministic, so re-importing is idempotent.

use anyhow::{bail, Context, Result};
use scion_core::{FileMode, ObjectId};
use scion_proto::{connect, has_blobs, mode_to_pb, parse_id, pb, put_blobs, Client};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const UPLOAD_BATCH_BYTES: usize = 32 << 20;
const SAVE_EVERY: usize = 100;

pub struct ImportOptions {
    pub git_dir: PathBuf,
    pub server: String,
    /// Git revision to import (branch, tag, sha).
    pub branch: String,
    /// Scion ref to point at the imported tip.
    pub target_ref: String,
    pub state_file: PathBuf,
}

#[derive(Debug)]
pub struct ImportReport {
    pub imported: usize,
    pub skipped: usize,
    pub tip: Option<ObjectId>,
}

fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("running git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

struct Commit {
    sha: String,
    parents: Vec<String>,
    author: String,
    timestamp: i64,
    message: String,
}

fn commits(dir: &Path, branch: &str) -> Result<Vec<Commit>> {
    let out = git(
        dir,
        &[
            "log",
            "--reverse",
            "--topo-order",
            "--format=%H%x00%P%x00%an <%ae>%x00%at%x00%B%x1e",
            branch,
        ],
    )?;
    let text = String::from_utf8_lossy(&out);
    let mut list = Vec::new();
    for rec in text.split('\x1e') {
        let rec = rec.trim_start_matches('\n');
        if rec.is_empty() {
            continue;
        }
        let f: Vec<&str> = rec.splitn(5, '\0').collect();
        if f.len() != 5 {
            bail!("unexpected git log record: {rec:?}");
        }
        list.push(Commit {
            sha: f[0].to_string(),
            parents: f[1].split_whitespace().map(str::to_string).collect(),
            author: f[2].to_string(),
            timestamp: f[3].parse()?,
            message: f[4].trim_end().to_string(),
        });
    }
    Ok(list)
}

enum GitChange {
    Upsert {
        path: String,
        sha: String,
        mode: FileMode,
    },
    Delete {
        path: String,
    },
}

fn file_mode(git_mode: &str) -> Option<FileMode> {
    match git_mode {
        "100755" => Some(FileMode::Executable),
        "120000" => Some(FileMode::Symlink),
        "100644" | "100664" => Some(FileMode::Regular),
        _ => None, // gitlinks (submodules) and absent entries
    }
}

/// File changes of `c` relative to its first parent (or the empty tree).
fn changes(dir: &Path, c: &Commit) -> Result<Vec<GitChange>> {
    let out = match c.parents.first() {
        Some(p) => git(
            dir,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--no-commit-id",
                p,
                &c.sha,
            ],
        )?,
        None => git(
            dir,
            &[
                "diff-tree",
                "-r",
                "-z",
                "--no-renames",
                "--no-commit-id",
                "--root",
                &c.sha,
            ],
        )?,
    };
    let mut fields = out.split(|b| *b == 0).filter(|f| !f.is_empty());
    let mut list = Vec::new();
    while let Some(header) = fields.next() {
        let header = std::str::from_utf8(header)?;
        let path = String::from_utf8(
            fields
                .next()
                .context("diff-tree record without path")?
                .to_vec(),
        )
        .context("non-UTF-8 path")?;
        // ":old_mode new_mode old_sha new_sha status"
        let h: Vec<&str> = header.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, _, new_sha, status] = h[..] else {
            bail!("bad diff-tree header {header:?}")
        };
        let (old, new) = (file_mode(old_mode), file_mode(new_mode));
        match (status, new) {
            ("D", _) | (_, None) => {
                if old.is_some() {
                    list.push(GitChange::Delete { path });
                }
            }
            (_, Some(mode)) => list.push(GitChange::Upsert {
                path,
                sha: new_sha.to_string(),
                mode,
            }),
        }
    }
    Ok(list)
}

/// Long-running `git cat-file --batch`.
struct CatFile {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl CatFile {
    fn new(dir: &Path) -> Result<Self> {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.take().context("cat-file stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("cat-file stdout")?);
        Ok(CatFile {
            child,
            stdin,
            stdout,
        })
    }

    fn read(&mut self, sha: &str) -> Result<Vec<u8>> {
        writeln!(self.stdin, "{sha}")?;
        self.stdin.flush()?;
        let mut header = String::new();
        self.stdout.read_line(&mut header)?;
        let parts: Vec<&str> = header.split_whitespace().collect();
        let [_, _, size] = parts[..] else {
            bail!("git cat-file: {}", header.trim())
        };
        let mut data = vec![0; size.parse()?];
        self.stdout.read_exact(&mut data)?;
        let mut nl = [0u8; 1];
        self.stdout.read_exact(&mut nl)?;
        Ok(data)
    }
}

impl Drop for CatFile {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn load_state(path: &Path) -> Result<BTreeMap<String, String>> {
    match std::fs::read(path) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_state(path: &Path, map: &BTreeMap<String, String>) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(map)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

async fn upload(client: &mut Client, pending: Vec<(ObjectId, Vec<u8>)>) -> Result<()> {
    let ids: Vec<ObjectId> = pending.iter().map(|p| p.0).collect();
    let present = has_blobs(client, &ids).await?;
    let mut batch = Vec::new();
    let mut bytes = 0;
    for ((id, data), present) in pending.into_iter().zip(present) {
        if present {
            continue;
        }
        bytes += data.len();
        batch.push((id, data));
        if bytes >= UPLOAD_BATCH_BYTES {
            put_blobs(client, std::mem::take(&mut batch)).await?;
            bytes = 0;
        }
    }
    put_blobs(client, batch).await
}

pub async fn import(
    opts: &ImportOptions,
    mut progress: impl FnMut(usize, usize),
) -> Result<ImportReport> {
    let mut client = connect(&opts.server).await?;
    let commits = commits(&opts.git_dir, &opts.branch)?;
    let mut mapping = load_state(&opts.state_file)?;
    let mut cat = CatFile::new(&opts.git_dir)?;
    let mut blob_ids: HashMap<String, ObjectId> = HashMap::new();
    let (mut imported, mut skipped) = (0, 0);

    for (i, c) in commits.iter().enumerate() {
        progress(i + 1, commits.len());
        if mapping.contains_key(&c.sha) {
            skipped += 1;
            continue;
        }
        let mut pending = Vec::new();
        let mut pb_changes = Vec::new();
        for ch in changes(&opts.git_dir, c)? {
            match ch {
                GitChange::Delete { path } => pb_changes.push(pb::FileChange {
                    path,
                    delete: true,
                    ..Default::default()
                }),
                GitChange::Upsert { path, sha, mode } => {
                    let id = match blob_ids.get(&sha) {
                        Some(id) => *id,
                        None => {
                            let data = cat.read(&sha)?;
                            let id = ObjectId::hash(&data);
                            blob_ids.insert(sha, id);
                            pending.push((id, data));
                            id
                        }
                    };
                    pb_changes.push(pb::FileChange {
                        path,
                        delete: false,
                        blob: id.to_hex(),
                        mode: mode_to_pb(mode),
                    });
                }
            }
        }
        upload(&mut client, pending).await?;
        let parents = c
            .parents
            .iter()
            .map(|p| {
                mapping
                    .get(p)
                    .cloned()
                    .with_context(|| format!("parent {p} of {} was not imported", c.sha))
            })
            .collect::<Result<Vec<_>>>()?;
        let resp = client
            .create_changeset(pb::CreateChangesetRequest {
                parents,
                author: c.author.clone(),
                message: c.message.clone(),
                timestamp: c.timestamp,
                changes: pb_changes,
            })
            .await
            .with_context(|| format!("importing commit {}", c.sha))?
            .into_inner();
        mapping.insert(c.sha.clone(), resp.id);
        imported += 1;
        if imported % SAVE_EVERY == 0 {
            save_state(&opts.state_file, &mapping)?;
        }
    }
    save_state(&opts.state_file, &mapping)?;

    let tip = match commits.last() {
        Some(c) => Some(parse_id(&mapping[&c.sha])?),
        None => None,
    };
    if let Some(tip) = tip {
        let current = client
            .list_refs(pb::ListRefsRequest {})
            .await?
            .into_inner()
            .refs
            .into_iter()
            .find(|r| r.name == opts.target_ref)
            .map(|r| r.target)
            .unwrap_or_default();
        let r = client
            .update_ref(pb::UpdateRefRequest {
                name: opts.target_ref.clone(),
                expected: current,
                new_target: tip.to_hex(),
            })
            .await?
            .into_inner();
        if !r.ok {
            bail!(
                "ref {} moved concurrently; it is now {}",
                opts.target_ref,
                r.current
            );
        }
    }
    Ok(ImportReport {
        imported,
        skipped,
        tip,
    })
}
