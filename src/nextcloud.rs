use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::credentials::Credentials;

/// How long Nextcloud keeps a login flow open.
const LOGIN_FLOW_LIFETIME: Duration = Duration::from_secs(20 * 60);

pub fn client() -> Result<Client> {
    // marks requests as API calls; Nextcloud then answers OCS endpoints without CSRF checks
    let headers = HeaderMap::from_iter([(
        "OCS-APIRequest".parse().expect("valid header name"),
        HeaderValue::from_static("true"),
    )]);
    Client::builder()
        .user_agent(user_agent())
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .build()
        .context("building the HTTP client")
}

/// Shown as the device name in Nextcloud's security settings.
fn user_agent() -> String {
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|h| h.trim().to_string())
        .unwrap_or_else(|_| "unknown".into());
    format!("nc-sync ({host})")
}

/// `segments` appended to the server URL, escaped, so installs under a subpath work too.
pub fn url(server: &Url, segments: &[&str]) -> Url {
    let mut url = server.clone();
    url.path_segments_mut()
        .expect("an http(s) URL has path segments")
        .pop_if_empty()
        .extend(segments);
    url
}

/// An OCS API endpoint, asking for JSON instead of XML.
fn ocs(server: &Url, segments: &[&str]) -> Url {
    let mut url = url(server, &[&["ocs", "v2.php"], segments].concat());
    url.set_query(Some("format=json"));
    url
}

#[derive(Deserialize)]
struct FlowStart {
    poll: FlowPoll,
    login: String,
}

#[derive(Deserialize)]
struct FlowPoll {
    token: String,
    endpoint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FlowResult {
    server: String,
    login_name: String,
    app_password: String,
}

/// A started Login Flow v2: the user opens `login_url`, then `wait` returns the app password.
pub struct LoginFlow {
    pub login_url: String,
    poll: FlowPoll,
    started: Instant,
}

impl LoginFlow {
    pub async fn start(client: &Client, server: &Url) -> Result<Self> {
        let response = client
            .post(url(server, &["index.php", "login", "v2"]))
            .send()
            .await
            .context("starting the login flow")?
            .error_for_status()
            .context("starting the login flow")?;
        let start: FlowStart = response.json().await.context("reading the login flow")?;
        Ok(Self {
            login_url: start.login,
            poll: start.poll,
            started: Instant::now(),
        })
    }

    /// Polls until the user granted access in the browser, or the flow expired.
    pub async fn wait(self, client: &Client, server: &Url) -> Result<Credentials> {
        while self.started.elapsed() < LOGIN_FLOW_LIFETIME {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let response = match client
                .post(&self.poll.endpoint)
                .form(&[("token", &self.poll.token)])
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    tracing::debug!("polling the login flow: {err}");
                    continue;
                }
            };
            match response.status() {
                StatusCode::NOT_FOUND => continue,
                status if status.is_success() => {
                    let result: FlowResult =
                        response.json().await.context("reading the login result")?;
                    tracing::info!("logged in as {} on {}", result.login_name, result.server);
                    return Ok(Credentials {
                        // keep the configured URL, it is the keyring lookup key
                        server: server.clone(),
                        login_name: result.login_name,
                        app_password: result.app_password,
                    });
                }
                status => tracing::debug!("polling the login flow: {status}"),
            }
        }
        bail!("the login flow expired before access was granted")
    }
}

pub enum PushEndpoint {
    Available(String),
    Unavailable,
    Unauthorized,
}

/// Looks up the notify_push websocket, if the server runs it.
pub async fn push_endpoint(client: &Client, credentials: &Credentials) -> Result<PushEndpoint> {
    let response = client
        .get(ocs(&credentials.server, &["cloud", "capabilities"]))
        .basic_auth(&credentials.login_name, Some(&credentials.app_password))
        .send()
        .await
        .context("fetching capabilities")?;
    if response.status() == StatusCode::UNAUTHORIZED {
        return Ok(PushEndpoint::Unauthorized);
    }
    let capabilities: Value = response
        .error_for_status()
        .context("fetching capabilities")?
        .json()
        .await
        .context("reading capabilities")?;
    Ok(websocket(&capabilities).map_or(PushEndpoint::Unavailable, PushEndpoint::Available))
}

fn websocket(capabilities: &Value) -> Option<String> {
    capabilities
        .pointer("/ocs/data/capabilities/notify_push/endpoints/websocket")?
        .as_str()
        .map(str::to_string)
}

/// Whether the server still accepts the app password.
pub async fn authorized(client: &Client, credentials: &Credentials) -> Result<bool> {
    let response = client
        .get(ocs(&credentials.server, &["cloud", "user"]))
        .basic_auth(&credentials.login_name, Some(&credentials.app_password))
        .send()
        .await
        .context("checking the app password")?;
    match response.status() {
        StatusCode::UNAUTHORIZED => Ok(false),
        status if status.is_success() => Ok(true),
        status => bail!("checking the app password: {status}"),
    }
}

/// Revokes the app password on the server, so the device disappears from its session list.
pub async fn revoke(client: &Client, credentials: &Credentials) -> Result<()> {
    client
        .delete(ocs(&credentials.server, &["core", "apppassword"]))
        .basic_auth(&credentials.login_name, Some(&credentials.app_password))
        .send()
        .await
        .context("revoking the app password")?
        .error_for_status()
        .context("revoking the app password")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_keep_the_server_subpath_and_escape_segments() {
        let server = Url::parse("https://example.com/nextcloud/").unwrap();
        assert_eq!(
            url(&server, &["remote.php", "dav", "files", "jane doe"]).as_str(),
            "https://example.com/nextcloud/remote.php/dav/files/jane%20doe"
        );
        assert_eq!(
            ocs(&server, &["cloud", "user"]).as_str(),
            "https://example.com/nextcloud/ocs/v2.php/cloud/user?format=json"
        );
    }

    #[test]
    fn parses_push_capability() {
        let body = serde_json::json!({"ocs": {"meta": {}, "data": {"version": {}, "capabilities": {
            "notify_push": {"type": ["files"], "endpoints": {
                "websocket": "wss://cloud.example.com/push/ws", "pre_auth": "x"}}}}}});
        assert_eq!(
            websocket(&body).as_deref(),
            Some("wss://cloud.example.com/push/ws")
        );
    }

    #[test]
    fn missing_push_capability_is_none() {
        let body =
            serde_json::json!({"ocs": {"meta": {}, "data": {"capabilities": {"files": {}}}}});
        assert!(websocket(&body).is_none());
    }

    #[test]
    fn parses_login_result() {
        let body =
            r#"{"server":"https://cloud.example.com","loginName":"jane","appPassword":"secret"}"#;
        let parsed: FlowResult = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.login_name, "jane");
        assert_eq!(parsed.app_password, "secret");
    }
}
