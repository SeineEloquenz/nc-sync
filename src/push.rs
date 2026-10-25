use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use tokio::sync::mpsc::Sender;
use tokio_tungstenite::tungstenite::Message;

use crate::credentials::Credentials;
use crate::daemon::Trigger;
use crate::nextcloud::{self, PushEndpoint};

const PING_INTERVAL: Duration = Duration::from_secs(30);
const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// Listens to notify_push and sends a trigger whenever the user's files change on the server.
/// Returns only if the server has no notify_push or rejects the credentials.
pub async fn listen(client: Client, credentials: Credentials, triggers: Sender<Trigger>) {
    let mut backoff = MIN_BACKOFF;
    let mut connected_before = false;
    loop {
        let result = match nextcloud::push_endpoint(&client, &credentials).await {
            Ok(PushEndpoint::Available(url)) => {
                let mut connection = Connection {
                    triggers: &triggers,
                    backoff: &mut backoff,
                    connected_before: &mut connected_before,
                };
                connection.run(&url, &credentials).await
            }
            Ok(PushEndpoint::Unavailable) => {
                tracing::warn!("the server has no notify_push, relying on the sync interval");
                return;
            }
            Ok(PushEndpoint::Unauthorized) => {
                let _ = triggers.send(Trigger::Unauthorized).await;
                return;
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(()) => tracing::info!("push connection closed"),
            Err(err) => tracing::warn!("push connection: {err:#}"),
        }
        tracing::info!("reconnecting in {}s", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

struct Connection<'a> {
    triggers: &'a Sender<Trigger>,
    backoff: &'a mut Duration,
    connected_before: &'a mut bool,
}

impl Connection<'_> {
    async fn run(&mut self, url: &str, credentials: &Credentials) -> Result<()> {
        let (mut socket, _) = tokio_tungstenite::connect_async(url)
            .await
            .with_context(|| format!("connecting to {url}"))?;
        socket
            .send(Message::text(credentials.login_name.clone()))
            .await?;
        socket
            .send(Message::text(credentials.app_password.clone()))
            .await?;

        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.tick().await;
        let mut authenticated = false;
        loop {
            tokio::select! {
                _ = ping.tick() => socket.send(Message::Ping(Vec::new().into())).await?,
                message = socket.next() => {
                    let Some(message) = message else { return Ok(()) };
                    let Message::Text(text) = message? else { continue };
                    match text.as_str() {
                        "authenticated" => {
                            tracing::info!("listening for remote changes");
                            authenticated = true;
                            *self.backoff = MIN_BACKOFF;
                            // changes while disconnected were missed; the first
                            // connection is covered by the session's initial sync
                            if std::mem::replace(self.connected_before, true) {
                                self.trigger(Trigger::Remote);
                            }
                        }
                        text if text.starts_with("notify_file") => self.trigger(Trigger::Remote),
                        text if text.starts_with("err:") && !authenticated => {
                            bail!("push authentication failed: {text}");
                        }
                        text => tracing::debug!("push message: {text}"),
                    }
                }
            }
        }
    }

    /// A full queue already holds a trigger, so dropping this one loses nothing.
    fn trigger(&self, trigger: Trigger) {
        let _ = self.triggers.try_send(trigger);
    }
}
