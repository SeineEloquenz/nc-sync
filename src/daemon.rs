use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::sync::mpsc::{self, Receiver};
use tokio::task::JoinSet;
use tokio::time::Instant;
use url::Url;

use crate::config::Config;
use crate::credentials::{self, Credentials};
use crate::desktop;
use crate::nextcloud::{self, LoginFlow};
use crate::push;
use crate::rclone::{Bisync, Mode, Outcome};
use crate::watch;

/// A sync starts at the latest this long after the first change, even if changes keep coming.
const MAX_DEBOUNCE: Duration = Duration::from_secs(60);

/// Triggers carry no data, so a short queue is enough; senders drop extras.
const TRIGGER_QUEUE: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Local,
    Remote,
    /// Nothing happened for the sync interval.
    Timer,
    /// The server rejected the app password.
    Unauthorized,
}

pub async fn run(config: Config, client: Client) -> Result<()> {
    loop {
        let credentials = match credentials::load(&config.server).await? {
            Some(credentials) => credentials,
            None => login(&client, &config.server, Prompt::Notification).await?,
        };
        session(&config, &client, credentials).await?;
        // the only way a session ends without an error is a rejected app password
        desktop::notify(
            "Nextcloud access was revoked",
            "Log in again to continue syncing.",
        )
        .await;
        credentials::delete(&config.server).await?;
    }
}

#[derive(Clone, Copy)]
pub enum Prompt {
    /// Opens the browser right away; for interactive use.
    Browser,
    /// Opens the browser when the user clicks a notification; for the daemon.
    Notification,
}

/// Runs Login Flow v2 and stores the app password. From the command line it gives up
/// after one attempt; the daemon keeps asking until the user grants access.
pub async fn login(client: &Client, server: &Url, prompt: Prompt) -> Result<Credentials> {
    loop {
        let attempt = async {
            let flow = LoginFlow::start(client, server).await?;
            tracing::info!("log in at {}", flow.login_url);
            match prompt {
                Prompt::Browser => desktop::open(&flow.login_url),
                Prompt::Notification => desktop::login_prompt(flow.login_url.clone()).await,
            }
            flow.wait(client, server).await
        };
        match (attempt.await, prompt) {
            (Ok(credentials), _) => {
                credentials::store(&credentials).await?;
                return Ok(credentials);
            }
            (Err(err), Prompt::Browser) => return Err(err),
            (Err(err), Prompt::Notification) => {
                tracing::warn!("{err:#}, asking again in 30s");
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

/// Syncs on start and on every trigger until the server rejects the credentials.
async fn session(config: &Config, client: &Client, credentials: Credentials) -> Result<()> {
    let bisync = Bisync::nextcloud(config, &credentials).await?;
    // the session keeps a sender, so the queue stays open even if every source stops
    let (triggers, mut events) = mpsc::channel(TRIGGER_QUEUE);

    // adding a watch per directory walks the whole tree, so keep it off the runtime
    let (dir, sender) = (config.local_dir.clone(), triggers.clone());
    let _watcher = tokio::task::spawn_blocking(move || watch::local(&dir, sender))
        .await?
        .inspect_err(|err| {
            tracing::warn!("{err:#}, relying on the sync interval for local changes")
        })
        .ok();
    let mut tasks = JoinSet::new();
    tasks.spawn(push::listen(
        client.clone(),
        credentials.clone(),
        triggers.clone(),
    ));

    let mut problem_reported = false;
    loop {
        match sync(&bisync, Mode::Normal, client, &credentials).await? {
            Outcome::Synced { conflicts } => {
                problem_reported = false;
                if conflicts > 0 {
                    desktop::notify(
                        "Sync conflicts",
                        &format!(
                            "{conflicts} file(s) changed on both sides. The newer version won, \
                             the older one was kept with a .conflict suffix."
                        ),
                    )
                    .await;
                }
            }
            Outcome::Unauthorized => return Ok(()),
            Outcome::SafetyAbort { reason, forceable } if !problem_reported => {
                problem_reported = true;
                let hint = if forceable {
                    "\nRun `nc-sync sync --force` if that is intended."
                } else {
                    ""
                };
                desktop::notify("Nextcloud sync paused", &format!("{reason}{hint}")).await;
            }
            Outcome::NeedsResync | Outcome::Failed(_) if !problem_reported => {
                problem_reported = true;
                desktop::notify(
                    "Nextcloud sync failed",
                    "See `journalctl --user -u nc-sync`.",
                )
                .await;
            }
            _ => {}
        }
        let trigger = next_trigger(&mut events, config.debounce(), config.interval()).await;
        if trigger == Trigger::Unauthorized {
            return Ok(());
        }
    }
}

/// Waits for a trigger, or `idle` without one, then for things to settle.
/// Returns early on `Unauthorized`.
async fn next_trigger(events: &mut Receiver<Trigger>, quiet: Duration, idle: Duration) -> Trigger {
    let first = match tokio::time::timeout(idle, events.recv()).await {
        Ok(event) => event.expect("the session holds a sender"),
        Err(_) => Trigger::Timer,
    };
    tracing::debug!("triggered by {first:?}");
    if first == Trigger::Unauthorized {
        return first;
    }
    let deadline = Instant::now() + MAX_DEBOUNCE;
    loop {
        let wait = quiet.min(deadline.saturating_duration_since(Instant::now()));
        match tokio::time::timeout(wait, events.recv()).await {
            Ok(Some(Trigger::Unauthorized)) => return Trigger::Unauthorized,
            Ok(_) => continue,
            Err(_) => return first,
        }
    }
}

/// Runs bisync and logs the outcome. rclone reports a rejected password like any
/// other error, so after a failure the server is asked directly.
pub async fn sync(
    bisync: &Bisync,
    mode: Mode,
    client: &Client,
    credentials: &Credentials,
) -> Result<Outcome> {
    let mut outcome = bisync.sync(mode).await.context("running bisync")?;
    if matches!(outcome, Outcome::Failed(_))
        && matches!(nextcloud::authorized(client, credentials).await, Ok(false))
    {
        outcome = Outcome::Unauthorized;
    }
    match &outcome {
        Outcome::Synced { conflicts } => tracing::info!("in sync ({conflicts} conflicts)"),
        Outcome::NeedsResync => tracing::error!("bisync still needs a resync after one"),
        Outcome::SafetyAbort { reason, .. } => tracing::error!("{reason}"),
        Outcome::Unauthorized => tracing::error!("the server rejected the app password"),
        Outcome::Failed(err) => tracing::error!("sync failed: {err}"),
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_secs(3);
    const IDLE: Duration = Duration::from_secs(300);

    #[tokio::test(start_paused = true)]
    async fn bursts_collapse_into_one_trigger() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Local).await.unwrap();
        tx.send(Trigger::Local).await.unwrap();
        tx.send(Trigger::Remote).await.unwrap();
        assert_eq!(next_trigger(&mut rx, QUIET, IDLE).await, Trigger::Local);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn unauthorized_cuts_the_wait_short() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Local).await.unwrap();
        tx.send(Trigger::Unauthorized).await.unwrap();
        assert_eq!(
            next_trigger(&mut rx, QUIET, IDLE).await,
            Trigger::Unauthorized
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_interval_fires_the_timer() {
        let (_tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        let start = Instant::now();
        assert_eq!(next_trigger(&mut rx, QUIET, IDLE).await, Trigger::Timer);
        assert!(start.elapsed() >= IDLE);
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_gives_up_after_the_cap() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Local).await.unwrap();
        let feeder = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if tx.send(Trigger::Local).await.is_err() {
                    return;
                }
            }
        });
        let start = Instant::now();
        next_trigger(&mut rx, QUIET, IDLE).await;
        assert!(start.elapsed() <= MAX_DEBOUNCE + Duration::from_secs(1));
        feeder.abort();
    }
}
