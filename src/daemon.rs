use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::watch::Sender as Switch;
use tokio::task::JoinSet;
use tokio::time::Instant;
use url::Url;

use crate::config::Config;
use crate::credentials::{self, Credentials};
use crate::desktop;
use crate::network;
use crate::nextcloud::{self, LoginFlow};
use crate::push;
use crate::rclone::{Bisync, Mode, Outcome};
use crate::tray::{Indicator, State};
use crate::watch;

/// A sync starts at the latest this long after the first change, even if changes keep coming.
const MAX_DEBOUNCE: Duration = Duration::from_secs(60);

/// Triggers carry no data, so a short queue is enough; senders drop extras.
const TRIGGER_QUEUE: usize = 16;

/// How often a sync is retried while the server cannot be reached.
const OFFLINE_RETRY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Local,
    Remote,
    /// Nothing happened for the sync interval.
    Timer,
    /// The server rejected the app password.
    Unauthorized,
    /// The user asked for a sync; it starts without waiting for things to settle.
    Manual,
}

/// Why syncing is held back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hold {
    /// The user paused it.
    pub manual: bool,
    /// The connection is metered.
    pub metered: bool,
}

impl Hold {
    pub fn any(self) -> bool {
        self.manual || self.metered
    }
}

/// Holds syncing while the user paused it or the connection is metered. Only the user's
/// pause is kept across restarts, in a file in the state dir; whether the connection is
/// metered is looked up again on every start.
#[derive(Clone)]
pub struct Pause {
    file: PathBuf,
    switch: Arc<Switch<Hold>>,
}

impl Pause {
    pub fn load(file: PathBuf) -> Self {
        let hold = Hold {
            manual: file.exists(),
            metered: false,
        };
        Self {
            file,
            switch: Arc::new(Switch::new(hold)),
        }
    }

    pub fn hold(&self) -> Hold {
        *self.switch.borrow()
    }

    /// Whether the user paused syncing.
    pub fn is_paused(&self) -> bool {
        self.hold().manual
    }

    pub fn set_metered(&self, metered: bool) {
        let changed = self.switch.send_if_modified(|hold| {
            let changed = hold.metered != metered;
            hold.metered = metered;
            changed
        });
        if changed {
            tracing::info!(
                "{}",
                if metered {
                    "the connection is metered, holding syncs"
                } else {
                    "the connection is no longer metered"
                }
            );
        }
    }

    /// The user's pause. A failure to save only costs it its persistence, so it is
    /// just logged.
    pub fn set(&self, paused: bool) {
        let saved = if paused {
            // before the first sync the state dir may not exist yet
            self.file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&self.file, ""))
        } else {
            std::fs::remove_file(&self.file).or_else(|err| match err.kind() {
                std::io::ErrorKind::NotFound => Ok(()),
                _ => Err(err),
            })
        };
        if let Err(err) = saved {
            tracing::warn!("saving the pause in {}: {err}", self.file.display());
        }
        tracing::info!("{}", if paused { "pausing" } else { "resuming" });
        self.switch.send_modify(|hold| hold.manual = paused);
    }

    /// Waits while syncing is held. "Sync now" still runs one sync on a metered
    /// connection, but not while the user paused. True if a trigger was `Unauthorized`.
    async fn released(&self, events: &mut Receiver<Trigger>) -> bool {
        let mut hold = self.switch.subscribe();
        loop {
            let current = *hold.borrow_and_update();
            if !current.any() {
                // the coming sync covers whatever changed meanwhile
                return drain(events);
            }
            tokio::select! {
                // the sender lives in self, so the channel cannot close while waiting
                _ = hold.changed() => {}
                trigger = events.recv() => match trigger.expect("the daemon holds a sender") {
                    Trigger::Unauthorized => return true,
                    Trigger::Manual if !current.manual => return drain(events),
                    _ => {}
                },
            }
        }
    }
}

/// What outlives a session: the trigger queue the tray also feeds, the pause and the tray.
struct Control {
    triggers: Sender<Trigger>,
    events: Receiver<Trigger>,
    pause: Pause,
    indicator: Indicator,
}

pub async fn run(config: Config, client: Client) -> Result<()> {
    let (triggers, events) = mpsc::channel(TRIGGER_QUEUE);
    let pause = Pause::load(config.pause_file());
    let indicator = Indicator::spawn(&config, triggers.clone(), pause.clone()).await;
    if config.pause_on_metered {
        tokio::spawn(network::follow_metered(pause.clone(), indicator.clone()));
    }
    let mut control = Control {
        triggers,
        events,
        pause,
        indicator,
    };
    loop {
        let credentials = match credentials::load(&config.server).await? {
            Some(credentials) => credentials,
            None => {
                control.indicator.set(State::LoginNeeded).await;
                login(&client, &config.server, Prompt::Notification).await?
            }
        };
        session(&config, &client, credentials, &mut control).await?;
        // the only way a session ends without an error is a rejected app password
        control.indicator.set(State::LoginNeeded).await;
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
async fn session(
    config: &Config,
    client: &Client,
    credentials: Credentials,
    control: &mut Control,
) -> Result<()> {
    let bisync = Bisync::nextcloud(config, &credentials).await?;
    // anything queued before this session is covered by its first sync
    drain(&mut control.events);
    let Control {
        triggers,
        events,
        pause,
        indicator,
    } = control;

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

    // failed syncs in a row; problems are reported once and retried with a growing delay
    let mut failures = 0;
    loop {
        if pause.hold().any() && pause.released(events).await {
            return Ok(());
        }

        indicator.set(State::Syncing).await;
        let outcome = sync(&bisync, Mode::Normal, client, &credentials).await?;
        match &outcome {
            Outcome::Synced { conflicts } => indicator.synced(conflicts).await,
            Outcome::SafetyAbort { reason, .. } => {
                indicator.set(State::Failed(reason.clone())).await
            }
            Outcome::NeedsResync => {
                indicator
                    .set(State::Failed("bisync needs a resync".into()))
                    .await
            }
            Outcome::Failed(err) => indicator.set(State::Failed(err.clone())).await,
            Outcome::Unreachable => indicator.set(State::Offline).await,
            Outcome::Unauthorized => {}
        }
        match &outcome {
            Outcome::Synced { conflicts } if !conflicts.is_empty() => {
                desktop::notify(
                    "Sync conflicts",
                    &format!(
                        "{} file(s) changed on both sides. The newer version won, \
                         the older one was kept with a .conflict suffix.",
                        conflicts.len()
                    ),
                )
                .await;
            }
            Outcome::Synced { .. } => {}
            Outcome::Unauthorized => return Ok(()),
            Outcome::SafetyAbort { reason, forceable } if failures == 0 => {
                let hint = if *forceable {
                    "\nRun `nc-sync sync --force` if that is intended."
                } else {
                    ""
                };
                desktop::notify("Nextcloud sync paused", &format!("{reason}{hint}")).await;
            }
            Outcome::NeedsResync | Outcome::Failed(_) if failures == 0 => {
                desktop::notify(
                    "Nextcloud sync failed",
                    "See `journalctl --user -u nc-sync`.",
                )
                .await;
            }
            _ => {}
        }

        if matches!(outcome, Outcome::Synced { .. }) {
            failures = 0;
            let trigger = next_trigger(events, config.debounce(), config.interval()).await;
            if trigger == Trigger::Unauthorized {
                return Ok(());
            }
        } else if outcome == Outcome::Unreachable {
            // being offline is no failure worth reporting, and a run against an
            // unreachable server ends quickly, so it is simply tried again soon
            tracing::info!("retrying in {}s", OFFLINE_RETRY.as_secs());
            if retry_wait(events, OFFLINE_RETRY).await {
                return Ok(());
            }
        } else {
            // a failed run leaves triggers from its own transfers behind; retrying on
            // them would repeat a long failing sync back to back
            failures += 1;
            let delay = retry_delay(failures, config.interval());
            tracing::info!("retrying in {}s", delay.as_secs());
            if retry_wait(events, delay).await {
                return Ok(());
            }
        }
    }
}

/// Waits out `delay`, or less if the user asks for a sync. Other triggers are dropped,
/// the retry covers them. True if one was `Unauthorized`.
async fn retry_wait(events: &mut Receiver<Trigger>, delay: Duration) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Some(Trigger::Unauthorized)) => return true,
            Ok(Some(Trigger::Manual)) | Err(_) => return drain(events),
            Ok(_) => continue,
        }
    }
}

/// One minute after the first failure, doubling up to the sync interval.
fn retry_delay(failures: u32, max: Duration) -> Duration {
    Duration::from_secs(60)
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
        .min(max)
}

/// Drops queued triggers, as the coming sync covers them. True if one was `Unauthorized`.
fn drain(events: &mut Receiver<Trigger>) -> bool {
    let mut unauthorized = false;
    while let Ok(trigger) = events.try_recv() {
        unauthorized |= trigger == Trigger::Unauthorized;
    }
    unauthorized
}

/// Waits for a trigger, or `idle` without one, then for things to settle.
/// Returns early on `Unauthorized`.
async fn next_trigger(events: &mut Receiver<Trigger>, quiet: Duration, idle: Duration) -> Trigger {
    let first = match tokio::time::timeout(idle, events.recv()).await {
        Ok(event) => event.expect("the daemon holds a sender"),
        Err(_) => Trigger::Timer,
    };
    tracing::debug!("triggered by {first:?}");
    if matches!(first, Trigger::Unauthorized | Trigger::Manual) {
        return first;
    }
    let deadline = Instant::now() + MAX_DEBOUNCE;
    loop {
        let wait = quiet.min(deadline.saturating_duration_since(Instant::now()));
        match tokio::time::timeout(wait, events.recv()).await {
            Ok(Some(trigger @ (Trigger::Unauthorized | Trigger::Manual))) => return trigger,
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
    if matches!(outcome, Outcome::Failed(_)) {
        match nextcloud::authorized(client, credentials).await {
            Ok(false) => outcome = Outcome::Unauthorized,
            Err(err) if nextcloud::is_unreachable(&err) => outcome = Outcome::Unreachable,
            _ => {}
        }
    }
    match &outcome {
        Outcome::Synced { conflicts } => {
            tracing::info!("in sync ({} conflicts)", conflicts.len())
        }
        Outcome::NeedsResync => tracing::error!("bisync still needs a resync after one"),
        Outcome::SafetyAbort { reason, .. } => tracing::error!("{reason}"),
        Outcome::Unauthorized => tracing::error!("the server rejected the app password"),
        Outcome::Unreachable => tracing::warn!("cannot reach the server"),
        Outcome::Failed(err) => tracing::error!("sync failed: {err}"),
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_secs(3);
    const IDLE: Duration = Duration::from_secs(300);

    #[test]
    fn retry_delay_doubles_up_to_the_interval() {
        let delays: Vec<u64> = (1..=6).map(|n| retry_delay(n, IDLE).as_secs()).collect();
        assert_eq!(delays, [60, 120, 240, 300, 300, 300]);
    }

    #[test]
    fn draining_reports_unauthorized() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.try_send(Trigger::Local).unwrap();
        tx.try_send(Trigger::Unauthorized).unwrap();
        tx.try_send(Trigger::Remote).unwrap();
        assert!(drain(&mut rx));
        assert!(rx.try_recv().is_err());
        assert!(!drain(&mut rx));
    }

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

    #[tokio::test(start_paused = true)]
    async fn sync_now_skips_the_debounce() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Local).await.unwrap();
        tx.send(Trigger::Manual).await.unwrap();
        let start = Instant::now();
        assert_eq!(next_trigger(&mut rx, QUIET, IDLE).await, Trigger::Manual);
        assert!(start.elapsed() < QUIET);
    }

    #[tokio::test(start_paused = true)]
    async fn sync_now_cuts_the_retry_delay_short() {
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Local).await.unwrap();
        tx.send(Trigger::Manual).await.unwrap();
        let start = Instant::now();
        assert!(!retry_wait(&mut rx, IDLE).await);
        assert!(start.elapsed() < IDLE);

        // without one, the full delay passes
        tx.send(Trigger::Remote).await.unwrap();
        let start = Instant::now();
        assert!(!retry_wait(&mut rx, IDLE).await);
        assert!(start.elapsed() >= IDLE);
    }

    #[tokio::test]
    async fn pause_is_kept_in_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("state/paused");
        let pause = Pause::load(file.clone());
        assert!(!pause.is_paused());

        pause.set(true);
        assert!(file.exists());
        assert!(Pause::load(file.clone()).is_paused());

        let (_tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        let waiting = tokio::spawn({
            let pause = pause.clone();
            async move { pause.released(&mut rx).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        pause.set(false);
        assert!(!waiting.await.unwrap());
        assert!(!file.exists());
        assert!(!Pause::load(file).is_paused());
    }

    #[tokio::test]
    async fn metered_holds_until_unmetered_and_is_not_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("paused");
        let pause = Pause::load(file.clone());
        pause.set_metered(true);
        assert!(pause.hold().any() && !pause.is_paused());
        assert!(!file.exists());

        let (_tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        let waiting = tokio::spawn({
            let pause = pause.clone();
            async move { pause.released(&mut rx).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        pause.set_metered(false);
        assert!(!waiting.await.unwrap());
    }

    #[tokio::test]
    async fn sync_now_overrides_metered_but_not_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let pause = Pause::load(dir.path().join("paused"));
        pause.set_metered(true);
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Manual).await.unwrap();
        assert!(!pause.released(&mut rx).await);

        // paused by the user, "Sync now" is ignored until resumed
        pause.set(true);
        tx.send(Trigger::Manual).await.unwrap();
        let waiting = tokio::spawn({
            let pause = pause.clone();
            async move { pause.released(&mut rx).await }
        });
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        pause.set(false);
        pause.set_metered(false);
        assert!(!waiting.await.unwrap());
    }

    #[tokio::test]
    async fn unauthorized_ends_a_hold() {
        let dir = tempfile::tempdir().unwrap();
        let pause = Pause::load(dir.path().join("paused"));
        pause.set(true);
        let (tx, mut rx) = mpsc::channel(TRIGGER_QUEUE);
        tx.send(Trigger::Unauthorized).await.unwrap();
        assert!(pause.released(&mut rx).await);
    }
}
