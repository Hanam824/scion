/// Repository paths are `/`-separated, relative, and never contain empty,
/// `.`, `..` or `.scn` components.
pub fn validate_path(p: &str) -> Result<(), String> {
    if p.is_empty() {
        return Err("empty path".into());
    }
    for c in p.split('/') {
        if c.is_empty() || c == "." || c == ".." || c == ".scn" || c.contains('\0') {
            return Err(format!("invalid path {p:?}"));
        }
    }
    Ok(())
}

pub fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Set of directory prefixes selecting part of the tree (sparse checkouts,
/// path-limited diffs). An empty filter selects everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathFilter {
    prefixes: Vec<String>,
}

impl PathFilter {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn new<S: AsRef<str>>(prefixes: impl IntoIterator<Item = S>) -> Self {
        let mut prefixes: Vec<String> = prefixes
            .into_iter()
            .map(|p| p.as_ref().trim_matches('/').to_string())
            .filter(|p| !p.is_empty())
            .collect();
        prefixes.sort();
        prefixes.dedup();
        PathFilter { prefixes }
    }

    pub fn is_all(&self) -> bool {
        self.prefixes.is_empty()
    }

    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// Whether `path` itself is selected.
    pub fn includes(&self, path: &str) -> bool {
        self.is_all() || self.prefixes.iter().any(|p| is_under(path, p))
    }

    /// Whether the directory `dir` is selected or contains a selected prefix,
    /// i.e. whether a walk needs to descend into it.
    pub fn may_contain(&self, dir: &str) -> bool {
        dir.is_empty() || self.includes(dir) || self.prefixes.iter().any(|p| is_under(p, dir))
    }
}

fn is_under(path: &str, dir: &str) -> bool {
    path == dir || (path.starts_with(dir) && path.as_bytes().get(dir.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates() {
        assert!(validate_path("a/b.txt").is_ok());
        for bad in ["", "/a", "a/", "a//b", "./a", "a/../b", ".scn/x"] {
            assert!(validate_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn filter() {
        let f = PathFilter::new(["src/", "docs/api"]);
        assert!(f.includes("src/main.rs"));
        assert!(f.includes("src"));
        assert!(!f.includes("srcx/a"));
        assert!(!f.includes("docs/readme"));
        assert!(f.may_contain("docs"));
        assert!(f.may_contain(""));
        assert!(!f.may_contain("lib"));
        assert!(PathFilter::all().includes("anything"));
    }
}
