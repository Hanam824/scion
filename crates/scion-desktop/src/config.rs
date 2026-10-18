use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_RECENT: usize = 10;

/// Persisted app settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub recent: Vec<PathBuf>,
    #[serde(default)]
    pub settings: Settings,
    /// Sidebar width in logical pixels; 0 means the default.
    #[serde(default)]
    pub sidebar_width: f32,
}

/// User-editable preferences (the Settings dialog).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// Commit author; empty means `scn::ops::default_author()`.
    #[serde(default)]
    pub author: String,
    /// Editor command or app name; empty means the system default app.
    #[serde(default)]
    pub editor: String,
    /// Terminal app or command; empty means the platform default.
    #[serde(default)]
    pub terminal: String,
    /// Land with a merge changeset instead of rebasing.
    #[serde(default)]
    pub merge: bool,
    /// "system", "light" or "dark".
    #[serde(default)]
    pub theme: String,
}

fn dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "scion", "Scion Desktop")
}

/// Platform config file, e.g. `~/Library/Application Support/dev.scion.Scion-Desktop/config.json`.
pub fn default_path() -> Option<PathBuf> {
    dirs().map(|d| d.config_dir().join("config.json"))
}

/// Platform data directory (holds the built-in server's data).
pub fn data_dir() -> PathBuf {
    dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".scion-desktop"))
}

impl Config {
    /// Missing or unreadable config yields defaults.
    pub fn load(path: Option<&Path>) -> Self {
        path.and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn add_recent(&mut self, p: PathBuf) {
        self.recent.retain(|x| x != &p);
        self.recent.insert(0, p);
        self.recent.truncate(MAX_RECENT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_is_mru_and_bounded() {
        let mut c = Config::default();
        for i in 0..12 {
            c.add_recent(PathBuf::from(format!("/r{i}")));
        }
        c.add_recent(PathBuf::from("/r5"));
        assert_eq!(c.recent.len(), MAX_RECENT);
        assert_eq!(c.recent[0], PathBuf::from("/r5"));
        assert_eq!(c.recent.iter().filter(|p| p.ends_with("r5")).count(), 1);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/config.json");
        let mut c = Config::default();
        c.add_recent(PathBuf::from("/x"));
        c.settings.editor = "code -r".into();
        c.save(&path).unwrap();
        let loaded = Config::load(Some(&path));
        assert_eq!(loaded.recent, vec![PathBuf::from("/x")]);
        assert_eq!(loaded.settings.editor, "code -r");
        assert!(Config::load(Some(&dir.path().join("missing")))
            .recent
            .is_empty());
    }

    #[test]
    fn old_config_files_still_load() {
        let c: Config = serde_json::from_str(r#"{"recent":["/a"]}"#).unwrap();
        assert_eq!(c.settings, Settings::default());
    }
}
