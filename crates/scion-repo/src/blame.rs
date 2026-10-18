//! Blame as lazily derived data keyed by (changeset, path). A file's blame at
//! the changeset that last modified it is derived from its blame at the
//! previous modifying changeset plus a line diff, and memoized.

use crate::{invalid, not_found, tree, HistRoot, Repo, RootTree};
use anyhow::Result;
use scion_core::text::split_lines;
use scion_core::ObjectId;
use similar::{capture_diff_slices, Algorithm, DiffOp};

const KIND: &str = "blame";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameLine {
    pub changeset: ObjectId,
    pub text: String,
}

fn key(cs: &ObjectId, path: &str) -> String {
    format!("{cs}:{path}")
}

fn text_of(repo: &Repo, blob: &ObjectId, path: &str) -> Result<String> {
    String::from_utf8(repo.get_blob(blob)?)
        .map_err(|_| invalid(format!("{path} is not a text file")))
}

fn file_at(repo: &Repo, cs: &ObjectId, path: &str) -> Result<Option<ObjectId>> {
    let root = repo.derive::<RootTree>(cs)?;
    Ok(tree::lookup_file(repo, root, path)?.map(|(b, _)| b))
}

fn last_change(repo: &Repo, cs: &ObjectId, path: &str) -> Result<Option<ObjectId>> {
    tree::last_change(repo, repo.derive::<HistRoot>(cs)?, path)
}

pub fn blame(repo: &Repo, rev: ObjectId, path: &str) -> Result<Vec<BlameLine>> {
    let blob =
        file_at(repo, &rev, path)?.ok_or_else(|| not_found(format!("file {path} at {rev}")))?;
    let last =
        last_change(repo, &rev, path)?.ok_or_else(|| not_found(format!("history of {path}")))?;
    let owners = blame_at(repo, last, path)?;
    let text = text_of(repo, &blob, path)?;
    Ok(split_lines(&text)
        .into_iter()
        .zip(owners)
        .map(|(line, changeset)| BlameLine {
            changeset,
            text: line.trim_end_matches('\n').to_string(),
        })
        .collect())
}

/// Per-line owners of `path` at `c`, where `c` last modified `path`.
fn blame_at(repo: &Repo, c: ObjectId, path: &str) -> Result<Vec<ObjectId>> {
    // Walk back through modifying changesets until a memoized one or the
    // file's creation, then derive forward.
    let mut chain: Vec<(ObjectId, ObjectId)> = Vec::new();
    let mut base: Option<(Vec<ObjectId>, ObjectId)> = None;
    let mut cur = Some(c);
    while let Some(x) = cur {
        let blob =
            file_at(repo, &x, path)?.ok_or_else(|| not_found(format!("file {path} at {x}")))?;
        if let Some(b) = repo.meta().get_derived(KIND, &key(&x, path))? {
            base = Some((serde_json::from_slice(&b)?, blob));
            break;
        }
        chain.push((x, blob));
        let parent = repo.get_changeset(&x)?.parents.first().copied();
        cur = match parent {
            Some(p) if file_at(repo, &p, path)?.is_some() => last_change(repo, &p, path)?,
            _ => None,
        };
    }

    let (mut owners, mut prev_blob) = match base {
        Some((o, b)) => (o, Some(b)),
        None => (Vec::new(), None),
    };
    for (x, blob) in chain.into_iter().rev() {
        let new_text = text_of(repo, &blob, path)?;
        let old_text = match &prev_blob {
            Some(b) => text_of(repo, b, path)?,
            None => String::new(),
        };
        let (old_lines, new_lines) = (split_lines(&old_text), split_lines(&new_text));
        let mut next = Vec::with_capacity(new_lines.len());
        for op in capture_diff_slices(Algorithm::Myers, &old_lines, &new_lines) {
            match op {
                DiffOp::Equal { old_index, len, .. } => {
                    next.extend_from_slice(&owners[old_index..old_index + len])
                }
                DiffOp::Delete { .. } => {}
                DiffOp::Insert { new_len, .. } | DiffOp::Replace { new_len, .. } => {
                    next.extend(std::iter::repeat_n(x, new_len))
                }
            }
        }
        repo.meta()
            .put_derived(KIND, &key(&x, path), &serde_json::to_vec(&next)?)?;
        owners = next;
        prev_blob = Some(blob);
    }
    Ok(owners)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn attributes_lines() {
        let (repo, _d) = repo();
        let a = commit(&repo, &[], "a", &[("f", Some("1\n2\n3\n"))]);
        let b = commit(&repo, &[a], "b", &[("other", Some("x"))]);
        let c = commit(&repo, &[b], "c", &[("f", Some("1\nTWO\n3\n4\n"))]);
        let owners = |rev| {
            blame(&repo, rev, "f")
                .unwrap()
                .into_iter()
                .map(|l| l.changeset)
                .collect::<Vec<_>>()
        };
        assert_eq!(owners(c), vec![a, c, a, c]);
        assert_eq!(owners(b), vec![a, a, a]);
        let lines = blame(&repo, c, "f").unwrap();
        assert_eq!(lines[1].text, "TWO");
        // A second call hits the memoized value.
        assert_eq!(owners(c), vec![a, c, a, c]);
    }
}
