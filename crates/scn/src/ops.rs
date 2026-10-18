//! Client operations. Each method is one `scn` command; the CLI only parses
//! arguments and formats output, so tests drive these directly.

use crate::cache::Cache;
use crate::state::{ConflictEntry, FileVersion, State, DIR};
use crate::worktree::{self, LocalChange};
use anyhow::{anyhow, bail, Context, Result};
use scion_core::{join, text::unified_patch, validate_path, FileMode, ObjectId, PathFilter};
use scion_proto::{connect, has_blobs, mode_from_pb, mode_to_pb, parse_id, pb, put_blobs, Client};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const UPLOAD_BATCH_BYTES: usize = 32 << 20;
const FETCH_BATCH: usize = 512;
const TREE_BATCH: usize = 1024;

pub fn default_author() -> String {
    std::env::var("SCN_AUTHOR")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".into())
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM` (UTC) from unix seconds.
pub fn format_timestamp(ts: i64) -> String {
    let (days, secs) = (ts.div_euclid(86400), ts.rem_euclid(86400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60
    )
}

/// Converts a user-supplied path (relative to `cwd`) into a repo path.
pub fn repo_path(root: &Path, cwd: &Path, arg: &str) -> Result<String> {
    fn normalize(p: &Path) -> Result<Vec<String>> {
        let mut parts = Vec::new();
        for c in p.components() {
            match c {
                Component::Normal(s) => {
                    parts.push(s.to_str().context("path is not UTF-8")?.to_string())
                }
                Component::ParentDir => {
                    parts.pop();
                }
                Component::RootDir | Component::Prefix(_) => parts.clear(),
                Component::CurDir => {}
            }
        }
        Ok(parts)
    }
    let abs = normalize(&cwd.join(arg))?;
    let root = normalize(root)?;
    if !abs.starts_with(&root) {
        bail!("{arg} is outside the working copy");
    }
    Ok(abs[root.len()..].join("/"))
}

pub enum LandOutcome {
    Landed {
        id: ObjectId,
        fast_forward: bool,
        up_to_date: bool,
        attempts: u32,
        sync_error: Option<String>,
    },
    Conflicts(ConflictSet),
}

/// Result of an operation that creates one changeset (rebase, revert).
pub enum Outcome {
    Done(ObjectId),
    Conflicts(ConflictSet),
}

/// A merge the server could not finish. Pass it to
/// `Workspace::start_resolution` to finish it in the working copy.
#[derive(Clone, Debug)]
pub struct ConflictSet {
    /// Changeset the merge was computed against.
    pub target: ObjectId,
    pub conflicts: Vec<pb::Conflict>,
    /// Cleanly merged changes, relative to `target`.
    pub resolved: Vec<pb::FileChange>,
    /// Suggested message for the commit that finishes the merge.
    pub message: String,
}

impl ConflictSet {
    fn from_response(resp: pb::MergeResponse, message: String) -> Result<Self> {
        Ok(ConflictSet {
            target: parse_id(&resp.target)?,
            conflicts: resp.conflicts,
            resolved: resp.resolved,
            message,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Take the version from the changeset being merged.
    Mine,
    /// Take the version that was already landed.
    Theirs,
    /// Keep the file as edited.
    Done,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Compare {
    /// Changesets in the working copy's base not yet on the tracked branch.
    pub ahead: u64,
    /// Changesets on the tracked branch not yet in the base.
    pub behind: u64,
    pub truncated: bool,
    /// The tracked branch does not exist yet; landing creates it.
    pub unpublished: bool,
}

fn has_markers(data: &[u8]) -> bool {
    String::from_utf8_lossy(data)
        .lines()
        .any(|l| l.starts_with("<<<<<<< "))
}

fn version(blob: &str, mode: i32) -> Result<Option<FileVersion>> {
    Ok(if blob.is_empty() {
        None
    } else {
        Some(FileVersion {
            blob: parse_id(blob)?,
            mode: mode_from_pb(mode),
        })
    })
}

/// Branch names are ref names: no whitespace, `~`, leading `-`, or `..`.
pub fn validate_branch(name: &str) -> Result<()> {
    let bad = name.is_empty()
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.contains("..")
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '~');
    if bad {
        bail!("invalid branch name {name:?}");
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub to: Option<ObjectId>,
    pub written: usize,
    pub removed: usize,
}

pub struct Workspace {
    pub root: PathBuf,
    pub state: State,
    client: Client,
    cache: Cache,
}

impl Workspace {
    pub fn find_root(start: &Path) -> Result<PathBuf> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            if d.join(DIR).is_dir() {
                return Ok(d.to_path_buf());
            }
            dir = d.parent();
        }
        bail!("not inside a scn working copy (no {DIR} directory found)")
    }

    pub async fn open(root: &Path) -> Result<Self> {
        let state = State::load(root)?;
        let client = connect(&state.server).await?;
        Ok(Workspace {
            root: root.to_path_buf(),
            cache: Cache::new(root),
            state,
            client,
        })
    }

    /// Creates a working copy of `track` (which may not exist yet, for a new
    /// repository), materializing only the `sparse` directories if given.
    pub async fn clone(server: &str, dest: &Path, track: &str, sparse: &[String]) -> Result<Self> {
        if dest.exists() && std::fs::read_dir(dest)?.next().is_some() {
            bail!("{} already exists and is not empty", dest.display());
        }
        let sparse = normalize_sparse(sparse)?;
        let mut client = connect(server).await?;
        let base = match client
            .resolve(pb::ResolveRequest {
                rev: track.to_string(),
            })
            .await
        {
            Ok(r) => Some(parse_id(&r.into_inner().id)?),
            Err(s) if s.code() == tonic::Code::NotFound => None,
            Err(s) => return Err(s.into()),
        };
        std::fs::create_dir_all(dest.join(DIR))?;
        let root = dest.canonicalize()?;
        let state = State {
            server: server.to_string(),
            track: track.to_string(),
            base: None,
            sparse,
            ..Default::default()
        };
        state.save(&root)?;
        let mut ws = Workspace {
            cache: Cache::new(&root),
            root,
            state,
            client,
        };
        if let Some(base) = base {
            ws.move_to(base).await?;
        }
        Ok(ws)
    }

    pub fn base(&self) -> Option<ObjectId> {
        self.state.base
    }

    /// Ref that `sync` and `land` target by default.
    pub fn tracked(&self) -> &str {
        &self.state.track
    }

    pub fn server(&self) -> &str {
        &self.state.server
    }

    fn save(&self) -> Result<()> {
        self.state.save(&self.root)
    }

    /// Default revision for queries: the working copy's base, else the tracked ref.
    fn current_rev(&self) -> String {
        self.state
            .base
            .map(|b| b.to_hex())
            .unwrap_or_else(|| self.state.track.clone())
    }

    pub fn status(&self) -> Result<Vec<LocalChange>> {
        worktree::scan(&self.root, &self.state)
    }

    pub async fn resolve(&mut self, rev: &str) -> Result<ObjectId> {
        let r = self
            .client
            .resolve(pb::ResolveRequest {
                rev: rev.to_string(),
            })
            .await?;
        parse_id(&r.into_inner().id)
    }

    /// Uploads changed files and creates a changeset on the server. The
    /// working copy moves to it; no ref moves (use `land`).
    pub async fn commit(&mut self, message: &str, author: &str) -> Result<ObjectId> {
        self.commit_paths(message, author, None).await
    }

    /// Like `commit`, but only the changes to `paths` (exact file paths);
    /// other local changes stay uncommitted.
    pub async fn commit_paths(
        &mut self,
        message: &str,
        author: &str,
        paths: Option<&[String]>,
    ) -> Result<ObjectId> {
        let unresolved = self.unresolved()?;
        if !unresolved.is_empty() {
            bail!("resolve conflicts first: {}", unresolved.join(", "));
        }
        let mut changes = self.status()?;
        if let Some(paths) = paths {
            let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
            changes.retain(|c| wanted.contains(c.path.as_str()));
        }
        if changes.is_empty() {
            bail!("nothing to commit");
        }
        let upserts: Vec<_> = changes
            .iter()
            .filter_map(|c| c.new.clone().map(|n| (c.path.clone(), n)))
            .collect();
        let ids: Vec<ObjectId> = upserts.iter().map(|(_, n)| n.blob).collect();
        let present = has_blobs(&mut self.client, &ids).await?;
        let mut batch = Vec::new();
        let mut batch_bytes = 0;
        for ((path, st), present) in upserts.iter().zip(present) {
            let data = worktree::read_content(&self.root.join(path), st.mode)?;
            if ObjectId::hash(&data) != st.blob {
                bail!("{path} changed while committing; try again");
            }
            self.cache.put(&st.blob, &data)?;
            if present {
                continue;
            }
            batch_bytes += data.len();
            batch.push((st.blob, data));
            if batch_bytes >= UPLOAD_BATCH_BYTES {
                put_blobs(&mut self.client, std::mem::take(&mut batch)).await?;
                batch_bytes = 0;
            }
        }
        put_blobs(&mut self.client, batch).await?;

        let pb_changes = changes
            .iter()
            .map(|c| match &c.new {
                Some(n) => pb::FileChange {
                    path: c.path.clone(),
                    delete: false,
                    blob: n.blob.to_hex(),
                    mode: mode_to_pb(n.mode),
                },
                None => pb::FileChange {
                    path: c.path.clone(),
                    delete: true,
                    ..Default::default()
                },
            })
            .collect();
        let resp = self
            .client
            .create_changeset(pb::CreateChangesetRequest {
                parents: self.state.base.iter().map(|b| b.to_hex()).collect(),
                author: author.to_string(),
                message: message.to_string(),
                timestamp: now(),
                changes: pb_changes,
            })
            .await?
            .into_inner();
        let id = parse_id(&resp.id)?;
        for c in changes {
            self.state.conflicts.remove(&c.path);
            match c.new {
                Some(n) => self.state.files.insert(c.path, n),
                None => self.state.files.remove(&c.path),
            };
        }
        // "Use theirs" leaves nothing to commit; drop those entries too.
        self.state.conflicts.retain(|_, e| !e.resolved);
        if self.state.conflicts.is_empty() {
            self.state.pending_message = None;
        }
        self.state.base = Some(id);
        self.save()?;
        Ok(id)
    }

    /// Restores files to their base version. Added files are deleted only
    /// with `remove_added`. Returns how many files changed.
    pub async fn discard(&mut self, paths: &[String], remove_added: bool) -> Result<usize> {
        let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let mut restore = Vec::new();
        let mut n = 0;
        for c in self.status()? {
            if !wanted.contains(c.path.as_str()) {
                continue;
            }
            match self.state.files.get(&c.path) {
                Some(f) => restore.push((c.path.clone(), f.blob, f.mode)),
                None if remove_added => {
                    worktree::remove_file(&self.root, &c.path)?;
                    n += 1;
                }
                None => continue,
            }
            self.state.conflicts.remove(&c.path);
        }
        n += self.materialize(restore).await?;
        if self.state.conflicts.is_empty() {
            self.state.pending_message = None;
        }
        self.save()?;
        Ok(n)
    }

    /// Ahead/behind counts of the base against the tracked branch, or `None`
    /// in an empty working copy.
    pub async fn compare(&mut self) -> Result<Option<Compare>> {
        let Some(base) = self.state.base else {
            return Ok(None);
        };
        let req = pb::CompareRequest {
            a: base.to_hex(),
            b: self.state.track.clone(),
        };
        match self.client.compare(req).await {
            Ok(r) => {
                let r = r.into_inner();
                Ok(Some(Compare {
                    ahead: r.ahead,
                    behind: r.behind,
                    truncated: r.truncated,
                    unpublished: false,
                }))
            }
            Err(s) if s.code() == tonic::Code::NotFound => Ok(Some(Compare {
                unpublished: true,
                ..Default::default()
            })),
            Err(s) => Err(s.into()),
        }
    }

    /// Moves the working copy to `rev` (default: the tracked ref).
    pub async fn sync(&mut self, rev: Option<&str>) -> Result<SyncReport> {
        let rev = rev.unwrap_or(&self.state.track).to_string();
        let target = self.resolve(&rev).await?;
        self.move_to(target).await
    }

    /// Lands the committed base onto a ref on the server (rebase by
    /// default, or a merge changeset), then syncs to the result.
    pub async fn land(&mut self, onto: Option<&str>, merge: bool) -> Result<LandOutcome> {
        let base = self
            .state
            .base
            .ok_or_else(|| anyhow!("nothing to land: no changesets yet"))?;
        let strategy = if merge {
            pb::MergeStrategy::Merge
        } else {
            pb::MergeStrategy::Rebase
        };
        let resp = self
            .client
            .merge(pb::MergeRequest {
                target_ref: onto.unwrap_or(&self.state.track).to_string(),
                source: base.to_hex(),
                strategy: strategy as i32,
                author: default_author(),
                message: String::new(),
            })
            .await?
            .into_inner();
        if !resp.conflicts.is_empty() {
            let message = self.message_of(&base.to_hex()).await?;
            return Ok(LandOutcome::Conflicts(ConflictSet::from_response(
                resp, message,
            )?));
        }
        self.finish_land(resp).await
    }

    /// Merges another branch into the current one on the server, then syncs.
    pub async fn merge_branch(&mut self, source: &str, merge: bool) -> Result<LandOutcome> {
        if source == self.state.track {
            bail!("cannot merge {source} into itself");
        }
        let message = format!("Merge {source} into {}", self.state.track);
        let strategy = if merge {
            pb::MergeStrategy::Merge
        } else {
            pb::MergeStrategy::Rebase
        };
        let resp = self
            .client
            .merge(pb::MergeRequest {
                target_ref: self.state.track.clone(),
                source: source.to_string(),
                strategy: strategy as i32,
                author: default_author(),
                message: message.clone(),
            })
            .await?
            .into_inner();
        if !resp.conflicts.is_empty() {
            return Ok(LandOutcome::Conflicts(ConflictSet::from_response(
                resp, message,
            )?));
        }
        self.finish_land(resp).await
    }

    async fn message_of(&mut self, rev: &str) -> Result<String> {
        let req = pb::GetChangesetRequest {
            rev: rev.to_string(),
        };
        Ok(self.client.get_changeset(req).await?.into_inner().message)
    }

    async fn finish_land(&mut self, resp: pb::MergeResponse) -> Result<LandOutcome> {
        let id = parse_id(&resp.id)?;
        let sync_error = self.move_to(id).await.err().map(|e| format!("{e:#}"));
        Ok(LandOutcome::Landed {
            id,
            fast_forward: resp.fast_forward,
            up_to_date: resp.up_to_date,
            attempts: resp.attempts,
            sync_error,
        })
    }

    /// Rebases the committed base onto `onto` (default: tracked ref) on the
    /// server, then moves the working copy to the result.
    pub async fn rebase(&mut self, onto: Option<&str>) -> Result<Outcome> {
        let base = self
            .state
            .base
            .ok_or_else(|| anyhow!("nothing to rebase: no changesets yet"))?;
        let resp = self
            .client
            .rebase(pb::RebaseRequest {
                source: base.to_hex(),
                onto: onto.unwrap_or(&self.state.track).to_string(),
            })
            .await?
            .into_inner();
        if !resp.conflicts.is_empty() {
            let message = self.message_of(&base.to_hex()).await?;
            return Ok(Outcome::Conflicts(ConflictSet::from_response(
                resp, message,
            )?));
        }
        let id = parse_id(&resp.id)?;
        self.move_to(id).await?;
        Ok(Outcome::Done(id))
    }

    /// Creates a changeset on top of the base that undoes `rev`, and moves
    /// the working copy to it (land it to publish).
    pub async fn revert(&mut self, rev: &str) -> Result<Outcome> {
        let base = self
            .state
            .base
            .ok_or_else(|| anyhow!("nothing to revert: no changesets yet"))?;
        let resp = self
            .client
            .revert(pb::RevertRequest {
                changeset: rev.to_string(),
                onto: base.to_hex(),
                author: default_author(),
            })
            .await?
            .into_inner();
        if !resp.conflicts.is_empty() {
            let message = format!("Revert {rev}");
            return Ok(Outcome::Conflicts(ConflictSet::from_response(
                resp, message,
            )?));
        }
        let id = parse_id(&resp.id)?;
        self.move_to(id).await?;
        Ok(Outcome::Done(id))
    }

    /// Finishes a conflicted merge in the working copy: syncs to the target,
    /// writes every cleanly merged file, and writes each conflicted file
    /// (with markers for content conflicts, else the merged-in version).
    /// Resolve the files, then commit as usual. Returns conflicted paths.
    pub async fn start_resolution(&mut self, set: &ConflictSet) -> Result<Vec<String>> {
        if !self.status()?.is_empty() {
            bail!("commit or discard local changes before resolving conflicts");
        }
        let filter = PathFilter::new(&self.state.sparse);
        let outside = set
            .resolved
            .iter()
            .map(|c| &c.path)
            .chain(set.conflicts.iter().map(|c| &c.path))
            .find(|p| !p.is_empty() && !filter.includes(p));
        if let Some(p) = outside {
            bail!("the merge touches {p}, outside the sparse checkout; widen it first");
        }
        if let Some(c) = set.conflicts.iter().find(|c| c.path.is_empty()) {
            bail!(
                "cannot resolve a file/directory conflict here: {}",
                c.detail
            );
        }
        self.move_to(set.target).await?;

        let mut writes = Vec::new();
        for c in &set.resolved {
            if c.delete {
                worktree::remove_file(&self.root, &c.path)?;
            } else {
                writes.push((c.path.clone(), parse_id(&c.blob)?, mode_from_pb(c.mode)));
            }
        }
        let mut paths = Vec::new();
        for c in &set.conflicts {
            let entry = ConflictEntry {
                kind: c.kind.clone(),
                target: version(&c.target_blob, c.target_mode)?,
                source: version(&c.source_blob, c.source_mode)?,
                resolved: false,
            };
            if c.kind == "content" {
                let data = c.detail.as_bytes();
                let mode = entry
                    .source
                    .or(entry.target)
                    .map(|v| v.mode)
                    .unwrap_or_default();
                worktree::write_file(&self.root, &c.path, ObjectId::hash(data), mode, data)?;
            } else {
                match entry.source {
                    Some(v) => writes.push((c.path.clone(), v.blob, v.mode)),
                    None => worktree::remove_file(&self.root, &c.path)?,
                }
            }
            self.state.conflicts.insert(c.path.clone(), entry);
            paths.push(c.path.clone());
        }
        self.write_files(&writes).await?;
        self.state.pending_message = Some(set.message.clone());
        self.save()?;
        Ok(paths)
    }

    /// Conflicted paths that still need attention.
    pub fn unresolved(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for (path, e) in &self.state.conflicts {
            let pending = if e.resolved {
                false
            } else if e.kind == "content" {
                std::fs::read(self.root.join(path)).is_ok_and(|d| has_markers(&d))
            } else {
                true
            };
            if pending {
                out.push(path.clone());
            }
        }
        Ok(out)
    }

    pub async fn resolve_conflict(&mut self, path: &str, choice: Resolution) -> Result<()> {
        let entry = self
            .state
            .conflicts
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("{path} is not conflicted"))?;
        let take = match choice {
            Resolution::Mine => Some(entry.source),
            Resolution::Theirs => Some(entry.target),
            Resolution::Done => None,
        };
        match take {
            Some(Some(v)) => {
                self.write_files(&[(path.to_string(), v.blob, v.mode)])
                    .await?
            }
            Some(None) => worktree::remove_file(&self.root, path)?,
            None => {
                let data = std::fs::read(self.root.join(path)).unwrap_or_default();
                if entry.kind == "content" && has_markers(&data) {
                    bail!("{path} still contains conflict markers");
                }
            }
        }
        if let Some(e) = self.state.conflicts.get_mut(path) {
            e.resolved = true;
        }
        self.save()
    }

    /// Abandons a resolution: restores every changed file to the base.
    pub async fn abort_resolution(&mut self) -> Result<()> {
        let paths: Vec<String> = self.status()?.into_iter().map(|c| c.path).collect();
        self.discard(&paths, true).await?;
        self.state.conflicts.clear();
        self.state.pending_message = None;
        self.save()
    }

    /// Creates a branch at the working copy's base, optionally switching to it.
    pub async fn create_branch(&mut self, name: &str, switch: bool) -> Result<ObjectId> {
        validate_branch(name)?;
        let base = self.state.base.ok_or_else(|| {
            anyhow!("cannot create a branch in an empty repository; commit first")
        })?;
        if !self.update_ref(name, "", &base.to_hex()).await?.ok {
            bail!("branch {name} already exists");
        }
        if switch {
            self.state.track = name.to_string();
            self.save()?;
        }
        Ok(base)
    }

    /// Switches the working copy to another branch. Refuses while the base
    /// has unlanded changesets, which would otherwise be left behind.
    pub async fn switch_branch(&mut self, name: &str) -> Result<SyncReport> {
        if name == self.state.track {
            return Ok(SyncReport {
                to: self.state.base,
                ..Default::default()
            });
        }
        if !self.state.conflicts.is_empty() {
            bail!("finish or abort the conflict resolution first");
        }
        let target = self.branch_target(name).await?;
        if let Some(c) = self.compare().await? {
            if c.ahead > 0 || c.unpublished {
                bail!(
                    "{} has {} unlanded changeset(s); land them or create a branch for them first",
                    self.state.track,
                    c.ahead
                );
            }
        }
        let report = self.move_to(target).await?;
        self.state.track = name.to_string();
        self.save()?;
        Ok(report)
    }

    pub async fn rename_branch(&mut self, old: &str, new: &str) -> Result<()> {
        validate_branch(new)?;
        let target = self.branch_target(old).await?.to_hex();
        if !self.update_ref(new, "", &target).await?.ok {
            bail!("branch {new} already exists");
        }
        if !self.update_ref(old, &target, "").await?.ok {
            bail!("{old} moved while renaming; {new} was created at its previous position");
        }
        if self.state.track == old {
            self.state.track = new.to_string();
            self.save()?;
        }
        Ok(())
    }

    pub async fn delete_branch(&mut self, name: &str) -> Result<()> {
        if name == self.state.track {
            bail!("cannot delete the current branch");
        }
        let target = self.branch_target(name).await?.to_hex();
        if !self.update_ref(name, &target, "").await?.ok {
            bail!("{name} moved while deleting it; try again");
        }
        Ok(())
    }

    async fn branch_target(&mut self, name: &str) -> Result<ObjectId> {
        let r = self.refs().await?.into_iter().find(|r| r.name == name);
        parse_id(&r.ok_or_else(|| anyhow!("no branch named {name}"))?.target)
    }

    async fn update_ref(
        &mut self,
        name: &str,
        expected: &str,
        new: &str,
    ) -> Result<pb::UpdateRefResponse> {
        let req = pb::UpdateRefRequest {
            name: name.to_string(),
            expected: expected.to_string(),
            new_target: new.to_string(),
        };
        Ok(self.client.update_ref(req).await?.into_inner())
    }

    pub async fn log(
        &mut self,
        rev: Option<&str>,
        path: Option<&str>,
        limit: u32,
    ) -> Result<Vec<pb::ChangesetInfo>> {
        let rev = rev
            .map(str::to_string)
            .unwrap_or_else(|| self.current_rev());
        let req = pb::LogRequest {
            rev,
            path: path.unwrap_or_default().to_string(),
            limit,
        };
        Ok(self.client.log(req).await?.into_inner().changesets)
    }

    pub async fn blame(&mut self, rev: Option<&str>, path: &str) -> Result<Vec<pb::BlameLine>> {
        let rev = rev
            .map(str::to_string)
            .unwrap_or_else(|| self.current_rev());
        Ok(self
            .client
            .blame(pb::BlameRequest {
                rev,
                path: path.to_string(),
            })
            .await?
            .into_inner()
            .lines)
    }

    /// Patch of uncommitted changes against the base.
    pub async fn diff_local(&mut self) -> Result<String> {
        let mut out = String::new();
        for c in &self.status()? {
            out.push_str(&self.diff_change(c).await?);
        }
        Ok(out)
    }

    /// Patch of one uncommitted change (from `status`) against the base.
    pub async fn diff_change(&mut self, c: &LocalChange) -> Result<String> {
        let old = match self.state.files.get(&c.path).map(|f| f.blob) {
            Some(blob) => {
                self.ensure_cached(vec![blob]).await?;
                Some(
                    self.cache
                        .get(&blob)
                        .with_context(|| format!("missing base content for {}", c.path))?,
                )
            }
            None => None,
        };
        let new = match &c.new {
            Some(n) => Some(worktree::read_content(&self.root.join(&c.path), n.mode)?),
            None => None,
        };
        Ok(unified_patch(&c.path, old.as_deref(), new.as_deref()))
    }

    /// Server-computed patch between two revisions; no checkout needed.
    pub async fn diff_revs(&mut self, from: &str, to: &str, paths: Vec<String>) -> Result<String> {
        let req = pb::DiffRequest {
            from: from.to_string(),
            to: to.to_string(),
            paths,
            patch: true,
        };
        let files = self.client.diff(req).await?.into_inner().files;
        Ok(files.into_iter().map(|f| f.patch).collect())
    }

    /// Files that differ between two revisions (no patches); `from` empty
    /// means the empty tree.
    pub async fn changed_files(&mut self, from: &str, to: &str) -> Result<Vec<pb::FileDiff>> {
        let req = pb::DiffRequest {
            from: from.to_string(),
            to: to.to_string(),
            paths: vec![],
            patch: false,
        };
        Ok(self.client.diff(req).await?.into_inner().files)
    }

    pub async fn changeset(&mut self, rev: &str) -> Result<pb::ChangesetInfo> {
        let req = pb::GetChangesetRequest {
            rev: rev.to_string(),
        };
        Ok(self.client.get_changeset(req).await?.into_inner())
    }

    pub async fn show(&mut self, rev: Option<&str>) -> Result<(pb::ChangesetInfo, String)> {
        let rev = rev
            .map(str::to_string)
            .unwrap_or_else(|| self.current_rev());
        let info = self
            .client
            .get_changeset(pb::GetChangesetRequest { rev })
            .await?
            .into_inner();
        let from = info.parents.first().cloned().unwrap_or_default();
        let patch = self.diff_revs(&from, &info.id, vec![]).await?;
        Ok((info, patch))
    }

    pub async fn refs(&mut self) -> Result<Vec<pb::Ref>> {
        Ok(self
            .client
            .list_refs(pb::ListRefsRequest {})
            .await?
            .into_inner()
            .refs)
    }

    /// Compare-and-swap a ref. Without `expect`, the expected value is the
    /// ref's current target as read just before.
    pub async fn set_ref(
        &mut self,
        name: &str,
        rev: &str,
        expect: Option<&str>,
    ) -> Result<pb::UpdateRefResponse> {
        let expected = match expect {
            Some(e) => self.resolve(e).await?.to_hex(),
            None => self
                .refs()
                .await?
                .into_iter()
                .find(|r| r.name == name)
                .map(|r| r.target)
                .unwrap_or_default(),
        };
        let req = pb::UpdateRefRequest {
            name: name.to_string(),
            expected,
            new_target: rev.to_string(),
        };
        Ok(self.client.update_ref(req).await?.into_inner())
    }

    /// Changes the sparse prefixes (empty = full checkout), adding and
    /// removing files to match.
    pub async fn set_sparse(&mut self, prefixes: &[String]) -> Result<SyncReport> {
        let prefixes = normalize_sparse(prefixes)?;
        let filter = PathFilter::new(&prefixes);
        let dirty: HashSet<String> = self.status()?.into_iter().map(|c| c.path).collect();
        let to_remove: Vec<String> = self
            .state
            .files
            .keys()
            .filter(|p| !filter.includes(p))
            .cloned()
            .collect();
        if let Some(p) = to_remove.iter().find(|p| dirty.contains(*p)) {
            bail!("{p} has uncommitted changes; commit or revert before narrowing the checkout");
        }
        let mut to_add = Vec::new();
        if let Some(base) = self.state.base {
            let root_tree = self.root_tree(base).await?;
            for f in self.list_files(root_tree, &filter).await? {
                if self.state.files.contains_key(&f.0) {
                    continue;
                }
                if worktree::read_meta(&self.root.join(&f.0))?.is_some() {
                    bail!("untracked file {} is in the way", f.0);
                }
                to_add.push(f);
            }
        }
        for p in &to_remove {
            worktree::remove_file(&self.root, p)?;
            self.state.files.remove(p);
        }
        let written = self.materialize(to_add).await;
        self.state.sparse = prefixes;
        self.save()?;
        Ok(SyncReport {
            to: self.state.base,
            written: written?,
            removed: to_remove.len(),
        })
    }

    async fn root_tree(&mut self, id: ObjectId) -> Result<ObjectId> {
        let r = self
            .client
            .resolve(pb::ResolveRequest { rev: id.to_hex() })
            .await?;
        parse_id(&r.into_inner().root_tree)
    }

    /// Applies the server-computed diff from the current base to `target`,
    /// refusing to overwrite local changes.
    async fn move_to(&mut self, target: ObjectId) -> Result<SyncReport> {
        if self.state.base == Some(target) {
            return Ok(SyncReport {
                to: Some(target),
                ..Default::default()
            });
        }
        let dirty: HashSet<String> = self.status()?.into_iter().map(|c| c.path).collect();
        let req = pb::DiffRequest {
            from: self.state.base.map(|b| b.to_hex()).unwrap_or_default(),
            to: target.to_hex(),
            paths: self.state.sparse.clone(),
            patch: false,
        };
        let files = self.client.diff(req).await?.into_inner().files;
        let blocked: Vec<&str> = files
            .iter()
            .filter(|f| dirty.contains(&f.path))
            .map(|f| f.path.as_str())
            .collect();
        if !blocked.is_empty() {
            bail!(
                "local changes would be overwritten (commit or revert them first): {}",
                blocked.join(", ")
            );
        }
        let mut removed = 0;
        let mut writes = Vec::new();
        for f in files {
            if f.new_blob.is_empty() {
                worktree::remove_file(&self.root, &f.path)?;
                self.state.files.remove(&f.path);
                removed += 1;
            } else {
                writes.push((f.path, parse_id(&f.new_blob)?, mode_from_pb(f.new_mode)));
            }
        }
        let written = self.materialize(writes).await;
        if written.is_ok() {
            self.state.base = Some(target);
        }
        self.save()?;
        self.cache.evict()?;
        Ok(SyncReport {
            to: Some(target),
            written: written?,
            removed,
        })
    }

    /// Walks trees from the root, one round trip per directory level,
    /// descending only into directories selected by `filter`.
    async fn list_files(
        &mut self,
        root_tree: ObjectId,
        filter: &PathFilter,
    ) -> Result<Vec<(String, ObjectId, FileMode)>> {
        let mut out = Vec::new();
        let mut frontier = vec![(String::new(), root_tree)];
        while !frontier.is_empty() {
            let mut next = Vec::new();
            for chunk in frontier.chunks(TREE_BATCH) {
                let req = pb::GetTreesRequest {
                    ids: chunk.iter().map(|(_, id)| id.to_hex()).collect(),
                };
                let trees = self.client.get_trees(req).await?.into_inner().trees;
                for ((prefix, _), tree) in chunk.iter().zip(trees) {
                    for e in tree.entries {
                        let path = join(prefix, &e.name);
                        if e.is_dir {
                            if filter.may_contain(&path) {
                                next.push((path, parse_id(&e.id)?));
                            }
                        } else if filter.includes(&path) {
                            out.push((path, parse_id(&e.id)?, mode_from_pb(e.mode)));
                        }
                    }
                }
            }
            frontier = next;
        }
        Ok(out)
    }

    async fn ensure_cached(&mut self, ids: Vec<ObjectId>) -> Result<()> {
        let mut missing: Vec<ObjectId> = ids.into_iter().filter(|id| !self.cache.has(id)).collect();
        missing.sort();
        missing.dedup();
        for chunk in missing.chunks(FETCH_BATCH) {
            let req = pb::GetBlobsRequest {
                ids: chunk.iter().map(|i| i.to_hex()).collect(),
            };
            let mut stream = self.client.get_blobs(req).await?.into_inner();
            while let Some(b) = stream.message().await? {
                let id = parse_id(&b.id)?;
                if ObjectId::hash(&b.data) != id {
                    bail!("server sent corrupt blob {id}");
                }
                self.cache.put(&id, &b.data)?;
            }
        }
        Ok(())
    }

    /// Writes files into the working copy without recording them as part of
    /// the base (they show up as local changes).
    async fn write_files(&mut self, files: &[(String, ObjectId, FileMode)]) -> Result<()> {
        self.ensure_cached(files.iter().map(|f| f.1).collect())
            .await?;
        for (path, blob, mode) in files {
            let data = self
                .cache
                .get(blob)
                .with_context(|| format!("blob {blob} missing from cache"))?;
            worktree::write_file(&self.root, path, *blob, *mode, &data)?;
        }
        Ok(())
    }

    async fn materialize(&mut self, files: Vec<(String, ObjectId, FileMode)>) -> Result<usize> {
        self.ensure_cached(files.iter().map(|f| f.1).collect())
            .await?;
        let n = files.len();
        for (path, blob, mode) in files {
            let data = self
                .cache
                .get(&blob)
                .with_context(|| format!("blob {blob} missing from cache"))?;
            let st = worktree::write_file(&self.root, &path, blob, mode, &data)?;
            self.state.files.insert(path, st);
        }
        Ok(n)
    }
}

fn normalize_sparse(prefixes: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = prefixes
        .iter()
        .map(|p| p.trim_matches('/').to_string())
        .filter(|p| !p.is_empty())
        .collect();
    for p in &out {
        validate_path(p).map_err(|e| anyhow!(e))?;
    }
    out.sort();
    out.dedup();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_paths() {
        let root = Path::new("/w/repo");
        assert_eq!(
            repo_path(root, Path::new("/w/repo/src"), "main.rs").unwrap(),
            "src/main.rs"
        );
        assert_eq!(
            repo_path(root, Path::new("/w/repo/src"), "../README").unwrap(),
            "README"
        );
        assert_eq!(repo_path(root, Path::new("/w/repo"), ".").unwrap(), "");
        assert!(repo_path(root, Path::new("/w/repo"), "../x").is_err());
    }
}
