use std::path::Path;

use anyhow::{Context, Result};
use notify::event::EventKind;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::Sender;

use crate::daemon::Trigger;

/// Watches the local folder and sends a trigger for every change.
/// The returned watcher stops when dropped.
pub fn local(dir: &Path, triggers: Sender<Trigger>) -> Result<RecommendedWatcher> {
    let mut watcher =
        notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
            // a full queue already holds a trigger, so dropping this one loses nothing
            Ok(event) if is_change(&event.kind) => {
                let _ = triggers.try_send(Trigger::Local);
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("file watcher: {err}"),
        })
        .context("creating the file watcher")?;
    watcher
        .watch(dir, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", dir.display()))?;
    Ok(watcher)
}

fn is_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}
