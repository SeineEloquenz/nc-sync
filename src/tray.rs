//! The status icon. The daemon reports its state through [`Indicator`]; the menu
//! starts syncs through the trigger queue and flips the [`Pause`] switch.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Handle, ToolTip, TrayMethods};
use tokio::sync::mpsc::Sender;
use url::Url;

use crate::config::Config;
use crate::daemon::{Pause, Trigger};
use crate::desktop;

/// How many conflict copies the menu lists.
const RECENT_CONFLICTS: usize = 5;

/// The menu shows "… min ago", so it is rebuilt this often.
const REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    Starting,
    Syncing,
    InSync,
    /// The last sync did not finish; the daemon retries on its own.
    Failed(String),
    /// The server cannot be reached; the daemon retries every minute.
    Offline,
    LoginNeeded,
}

/// Forwards the daemon's state to the tray, or does nothing without one.
pub struct Indicator(Option<Handle<NcTray>>);

impl Indicator {
    pub async fn spawn(config: &Config, triggers: Sender<Trigger>, pause: Pause) -> Self {
        if !config.tray {
            return Self(None);
        }
        let tray = NcTray {
            state: State::Starting,
            last_sync: None,
            conflicts: VecDeque::new(),
            local_dir: config.local_dir.clone(),
            server: config.server.clone(),
            triggers,
            pause,
        };
        // the session may start before the shell's tray host does, so a missing host
        // is no error; ksni registers once it appears
        match tray.assume_sni_available(true).spawn().await {
            Ok(handle) => {
                let ticker = handle.clone();
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(REFRESH);
                    loop {
                        interval.tick().await;
                        if ticker.update(|_| ()).await.is_none() {
                            return;
                        }
                    }
                });
                Self(Some(handle))
            }
            Err(err) => {
                tracing::warn!("no tray icon: {err}");
                Self(None)
            }
        }
    }

    pub async fn set(&self, state: State) {
        if let Some(handle) = &self.0 {
            handle.update(|tray| tray.state = state).await;
        }
    }

    /// A finished sync; `conflicts` are relative to the synced folder.
    pub async fn synced(&self, conflicts: &[PathBuf]) {
        if let Some(handle) = &self.0 {
            handle
                .update(|tray| {
                    tray.state = State::InSync;
                    tray.last_sync = Some(Instant::now());
                    for conflict in conflicts {
                        tray.conflicts.retain(|known| known != conflict);
                        tray.conflicts.push_front(conflict.clone());
                    }
                    tray.conflicts.truncate(RECENT_CONFLICTS);
                })
                .await;
        }
    }
}

struct NcTray {
    state: State,
    last_sync: Option<Instant>,
    /// Newest first.
    conflicts: VecDeque<PathBuf>,
    local_dir: PathBuf,
    server: Url,
    triggers: Sender<Trigger>,
    pause: Pause,
}

impl NcTray {
    /// What the icon and the menu show. Pausing takes effect after a running sync, and
    /// a missing login matters more than anything else.
    fn shown(&self) -> Shown<'_> {
        match &self.state {
            State::LoginNeeded => Shown::LoginNeeded,
            State::Syncing => Shown::Syncing,
            _ if self.pause.is_paused() => Shown::Paused,
            State::Starting => Shown::Starting,
            State::InSync => Shown::InSync,
            State::Failed(reason) => Shown::Failed(reason),
            State::Offline => Shown::Offline,
        }
    }

    fn summary(&self) -> String {
        let since = self
            .last_sync
            .map(|at| format!(" · {}", ago(at.elapsed())))
            .unwrap_or_default();
        match self.shown() {
            Shown::Starting => "Starting".into(),
            Shown::Syncing if self.pause.is_paused() => "Syncing, pausing afterwards".into(),
            Shown::Syncing => "Syncing…".into(),
            Shown::InSync => format!("In sync{since}"),
            Shown::Paused => format!("Paused{since}"),
            Shown::Failed(reason) => format!("Sync failed, retrying: {}", shorten(reason)),
            Shown::Offline => format!("Can't reach the server{since}"),
            Shown::LoginNeeded => "Log in to continue syncing".into(),
        }
    }
}

enum Shown<'a> {
    Starting,
    Syncing,
    InSync,
    Paused,
    Failed(&'a str),
    Offline,
    LoginNeeded,
}

impl ksni::Tray for NcTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        env!("CARGO_PKG_NAME").into()
    }

    fn title(&self) -> String {
        "Nextcloud".into()
    }

    fn icon_name(&self) -> String {
        match self.shown() {
            Shown::Starting | Shown::InSync => "folder-remote-symbolic",
            Shown::Syncing => "view-refresh-symbolic",
            Shown::Paused => "media-playback-pause-symbolic",
            Shown::Failed(_) => "dialog-warning-symbolic",
            Shown::Offline => "network-offline-symbolic",
            Shown::LoginNeeded => "dialog-password-symbolic",
        }
        .into()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Nextcloud".into(),
            description: self.summary(),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let paused = self.pause.is_paused();
        let can_sync = !paused
            && matches!(
                self.state,
                State::InSync | State::Failed(_) | State::Offline
            );
        let mut menu = vec![
            StandardItem {
                label: escape(&self.summary()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Sync now".into(),
                icon_name: "view-refresh-symbolic".into(),
                enabled: can_sync,
                activate: Box::new(|tray: &mut Self| {
                    // a full queue already holds a trigger, which starts the same sync
                    let _ = tray.triggers.try_send(Trigger::Manual);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if paused {
                    "Resume syncing"
                } else {
                    "Pause syncing"
                }
                .into(),
                icon_name: if paused {
                    "media-playback-start-symbolic"
                } else {
                    "media-playback-pause-symbolic"
                }
                .into(),
                activate: Box::new(|tray: &mut Self| tray.pause.set(!tray.pause.is_paused())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open folder".into(),
                icon_name: "folder-symbolic".into(),
                activate: Box::new(|tray: &mut Self| open_path(&tray.local_dir)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Open Nextcloud".into(),
                icon_name: "web-browser-symbolic".into(),
                activate: Box::new(|tray: &mut Self| desktop::open(tray.server.as_str())),
                ..Default::default()
            }
            .into(),
        ];

        if !self.conflicts.is_empty() {
            menu.push(MenuItem::Separator);
            menu.push(
                StandardItem {
                    label: "Recent conflicts".into(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
            for conflict in &self.conflicts {
                let folder = self
                    .local_dir
                    .join(conflict)
                    .parent()
                    .map_or_else(|| self.local_dir.clone(), Path::to_path_buf);
                menu.push(
                    StandardItem {
                        label: escape(&conflict.to_string_lossy()),
                        icon_name: "text-x-generic-symbolic".into(),
                        activate: Box::new(move |_: &mut Self| open_path(&folder)),
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        menu
    }
}

fn open_path(path: &Path) {
    desktop::open(&path.to_string_lossy());
}

/// Menu labels treat `_` as an access key marker; a literal one is written twice.
fn escape(label: &str) -> String {
    label.replace('_', "__")
}

/// Keeps a long rclone message from stretching the menu.
fn shorten(text: &str) -> String {
    const MAX: usize = 60;
    let first_line = text.lines().next().unwrap_or_default();
    match first_line.char_indices().nth(MAX) {
        Some((end, _)) => format!("{}…", &first_line[..end]),
        None => first_line.to_string(),
    }
}

fn ago(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        0..60 => "just now".into(),
        secs @ 60..3600 => format!("{} min ago", secs / 60),
        secs @ 3600..86400 => format!("{} h ago", secs / 3600),
        secs => format!("{} d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_times() {
        assert_eq!(ago(Duration::from_secs(5)), "just now");
        assert_eq!(ago(Duration::from_secs(150)), "2 min ago");
        assert_eq!(ago(Duration::from_secs(7300)), "2 h ago");
        assert_eq!(ago(Duration::from_secs(3 * 86400)), "3 d ago");
    }

    #[test]
    fn labels_keep_underscores() {
        assert_eq!(escape("my_file.txt.conflict1"), "my__file.txt.conflict1");
    }

    #[test]
    fn long_reasons_are_cut() {
        let reason = format!("{}\nsecond line", "x".repeat(80));
        let short = shorten(&reason);
        assert_eq!(short.chars().count(), 61);
        assert!(short.ends_with('…'));
        assert_eq!(shorten("short\nmore"), "short");
    }
}
