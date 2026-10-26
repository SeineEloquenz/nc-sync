use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;
use url::Url;

/// Patterns that never sync, in rclone filter syntax. Mirrors the official
/// client's built-in excludes plus its own state files, so a folder handed over
/// from it does not upload the old journal.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "*~",
    "~$*",
    ".~lock.*",
    "~*.tmp",
    "*.~*",
    ".DS_Store",
    "Thumbs.db",
    "._*",
    ".*.sw?",
    ".*.*sw?",
    ".Trash-*/**",
    ".directory",
    "*.part",
    "*.partial",
    "*.filepart",
    "*.crdownload",
    "*.kate-swp",
    ".fuse_hidden*",
    ".nfs*",
    ".sync_*.db*",
    "._sync_*.db*",
    ".csync_journal.db*",
    ".owncloudsync.log*",
    ".nextcloudsync.log*",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Base URL of the Nextcloud instance, e.g. https://cloud.example.com
    pub server: Url,

    /// Local folder that mirrors the remote one.
    pub local_dir: PathBuf,

    /// Folder on the server to sync, relative to the user's root.
    #[serde(default = "default_remote_dir")]
    pub remote_dir: String,

    /// Full sync interval in seconds, as a fallback for missed events.
    #[serde(default = "default_interval")]
    pub interval: u64,

    /// Seconds without further changes before a sync starts.
    #[serde(default = "default_debounce")]
    pub debounce: u64,

    /// Extra exclude patterns, appended to the defaults.
    #[serde(default)]
    pub exclude: Vec<String>,

    /// rclone binary to run.
    #[serde(default = "default_rclone")]
    pub rclone: PathBuf,

    /// Extra arguments passed to `rclone bisync`.
    #[serde(default)]
    pub rclone_args: Vec<String>,

    /// Where bisync listings and the filter file live; defaults to the XDG state dir.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,

    /// Show a status icon in the system tray.
    #[serde(default = "default_true")]
    pub tray: bool,

    /// Hold syncs while NetworkManager reports a metered connection.
    #[serde(default = "default_true")]
    pub pause_on_metered: bool,
}

fn default_true() -> bool {
    true
}

fn default_remote_dir() -> String {
    "/".into()
}

fn default_interval() -> u64 {
    300
}

fn default_debounce() -> u64 {
    3
}

fn default_rclone() -> PathBuf {
    "rclone".into()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.local_dir = expand_home(&config.local_dir);
        config.state_dir = config.state_dir.as_deref().map(expand_home);
        Ok(config)
    }

    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| expand_home(Path::new("~/.config")))
            .join("nc-sync/config.toml")
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval)
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_secs(self.debounce)
    }

    /// Where bisync listings, the filter file and the empty rclone config live.
    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| expand_home(Path::new("~/.local/state")))
                .join("nc-sync")
        })
    }

    /// Exists while syncing is paused.
    pub fn pause_file(&self) -> PathBuf {
        self.state_dir().join("paused")
    }

    pub fn filter_rules(&self) -> String {
        DEFAULT_EXCLUDES
            .iter()
            .copied()
            .chain(self.exclude.iter().map(String::as_str))
            .map(|pattern| format!("- {pattern}\n"))
            .collect()
    }
}

fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), dirs::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_gets_defaults() {
        let config: Config =
            toml::from_str("server = \"https://cloud.example.com\"\nlocal_dir = \"/tmp/nc\"")
                .unwrap();
        assert_eq!(config.remote_dir, "/");
        assert_eq!(config.interval, 300);
        assert_eq!(config.debounce, 3);
        assert!(config.tray);
        assert!(config.pause_on_metered);
    }

    #[test]
    fn extra_excludes_follow_defaults() {
        let config: Config = toml::from_str(
            "server = \"https://cloud.example.com\"\nlocal_dir = \"/tmp/nc\"\nexclude = [\"*.iso\"]",
        )
        .unwrap();
        let rules = config.filter_rules();
        assert!(rules.starts_with("- *~\n"));
        assert!(rules.ends_with("- *.iso\n"));
    }
}
