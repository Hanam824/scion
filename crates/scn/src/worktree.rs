//! Working copy on disk: scanning, change detection, and file writes.

use crate::state::{FileState, State, DIR};
use anyhow::{bail, Context, Result};
use scion_core::{join, FileMode, ObjectId, PathFilter};
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::UNIX_EPOCH;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Disk {
    pub mode: FileMode,
    pub size: u64,
    pub mtime_ns: u64,
}

pub fn read_meta(abs: &Path) -> std::io::Result<Option<Disk>> {
    let m = match fs::symlink_metadata(abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mode = if m.file_type().is_symlink() {
        FileMode::Symlink
    } else if m.is_file() {
        if m.permissions().mode() & 0o111 != 0 {
            FileMode::Executable
        } else {
            FileMode::Regular
        }
    } else {
        return Ok(None);
    };
    let mtime_ns = m
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    Ok(Some(Disk {
        mode,
        size: m.len(),
        mtime_ns,
    }))
}

pub fn read_content(abs: &Path, mode: FileMode) -> Result<Vec<u8>> {
    Ok(match mode {
        FileMode::Symlink => fs::read_link(abs)?.as_os_str().as_bytes().to_vec(),
        _ => fs::read(abs).with_context(|| format!("reading {}", abs.display()))?,
    })
}

/// Writes a file (replacing any existing file or symlink) and returns its
/// on-disk state.
pub fn write_file(
    root: &Path,
    path: &str,
    blob: ObjectId,
    mode: FileMode,
    data: &[u8],
) -> Result<FileState> {
    let abs = root.join(path);
    if let Some(dir) = abs.parent() {
        fs::create_dir_all(dir)?;
    }
    match fs::symlink_metadata(&abs) {
        Ok(m) if m.is_dir() => bail!("cannot write {path}: a directory is in the way"),
        Ok(_) => fs::remove_file(&abs)?,
        Err(_) => {}
    }
    if mode == FileMode::Symlink {
        std::os::unix::fs::symlink(OsStr::from_bytes(data), &abs)?;
    } else {
        fs::write(&abs, data)?;
        let perm = if mode == FileMode::Executable {
            0o755
        } else {
            0o644
        };
        fs::set_permissions(&abs, fs::Permissions::from_mode(perm))?;
    }
    let d = read_meta(&abs)?.with_context(|| format!("{path} vanished after write"))?;
    Ok(FileState {
        blob,
        mode,
        size: d.size,
        mtime_ns: d.mtime_ns,
    })
}

/// Removes a file and any parent directories left empty.
pub fn remove_file(root: &Path, path: &str) -> Result<()> {
    let abs = root.join(path);
    match fs::remove_file(&abs) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut dir = abs.parent();
    while let Some(d) = dir {
        if d == root || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Path components to skip, from `.scnignore` (one name per line, `#` for
/// comments). `.scn` is always ignored.
pub struct Ignore {
    names: HashSet<String>,
}

impl Ignore {
    pub fn load(root: &Path) -> Self {
        let mut names: HashSet<String> = [DIR.to_string()].into();
        if let Ok(s) = fs::read_to_string(root.join(".scnignore")) {
            names.extend(
                s.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(|l| l.trim_matches('/').to_string()),
            );
        }
        Ignore { names }
    }

    /// Whether any component of the repo-relative `rel` is ignored.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        rel.components().any(|c| {
            c.as_os_str()
                .to_str()
                .is_some_and(|s| self.names.contains(s))
        })
    }
}

/// Files on disk selected by `filter`, keyed by repo path.
pub fn walk(root: &Path, filter: &PathFilter, ignore: &Ignore) -> Result<BTreeMap<String, Disk>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        for entry in fs::read_dir(root.join(&rel))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if ignore.names.contains(&name) {
                continue;
            }
            let path = join(&rel, &name);
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if filter.may_contain(&path) {
                    stack.push(path);
                }
            } else if filter.includes(&path) {
                if let Some(d) = read_meta(&entry.path())? {
                    out.insert(path, d);
                }
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug)]
pub struct LocalChange {
    pub path: String,
    pub kind: ChangeKind,
    /// New state for added/modified files.
    pub new: Option<FileState>,
}

/// Uncommitted changes relative to `state`. Files whose size and mtime are
/// unchanged are assumed unmodified and never re-hashed.
pub fn scan(root: &Path, state: &State) -> Result<Vec<LocalChange>> {
    let filter = PathFilter::new(&state.sparse);
    let disk = walk(root, &filter, &Ignore::load(root))?;
    let mut out = Vec::new();
    for path in state.files.keys() {
        // Tracked files hidden by .scnignore still count as present.
        if !disk.contains_key(path) && read_meta(&root.join(path))?.is_none() {
            out.push(LocalChange {
                path: path.clone(),
                kind: ChangeKind::Deleted,
                new: None,
            });
        }
    }
    for (path, d) in disk {
        let prev = state.files.get(&path);
        if let Some(p) = prev {
            if p.size == d.size && p.mtime_ns == d.mtime_ns && p.mode == d.mode {
                continue;
            }
        }
        let blob = ObjectId::hash(&read_content(&root.join(&path), d.mode)?);
        let new = FileState {
            blob,
            mode: d.mode,
            size: d.size,
            mtime_ns: d.mtime_ns,
        };
        let kind = match prev {
            None => ChangeKind::Added,
            Some(p) if p.blob != blob || p.mode != d.mode => ChangeKind::Modified,
            Some(_) => continue,
        };
        out.push(LocalChange {
            path,
            kind,
            new: Some(new),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}
