use std::collections::HashMap;
use std::fmt;

use anyhow::{Context, Result};
use oo7::Keyring;
use url::Url;

use crate::nextcloud;

/// An app password issued by Nextcloud for this device.
#[derive(Clone)]
pub struct Credentials {
    pub server: Url,
    pub login_name: String,
    pub app_password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("server", &self.server.as_str())
            .field("login_name", &self.login_name)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// WebDAV root of the user's files.
    pub fn dav_url(&self) -> String {
        nextcloud::url(
            &self.server,
            &["remote.php", "dav", "files", &self.login_name],
        )
        .into()
    }
}

/// The unlocked default collection of the Secret Service.
async fn keyring() -> Result<Keyring> {
    let keyring = Keyring::new().await.context("opening the keyring")?;
    keyring.unlock().await.context("unlocking the keyring")?;
    Ok(keyring)
}

fn lookup(server: &Url) -> [(&'static str, String); 2] {
    [
        ("application", "nc-sync".to_string()),
        ("server", server.as_str().trim_end_matches('/').to_string()),
    ]
}

pub async fn load(server: &Url) -> Result<Option<Credentials>> {
    let items = keyring().await?.search_items(&lookup(server)).await?;
    let Some(item) = items.into_iter().next() else {
        return Ok(None);
    };
    let attributes: HashMap<String, String> = item.attributes().await?;
    let Some(login_name) = attributes.get("login").cloned() else {
        return Ok(None);
    };
    let secret = item.secret().await?;
    let app_password = String::from_utf8(secret.to_vec()).context("app password is not UTF-8")?;
    Ok(Some(Credentials {
        server: server.clone(),
        login_name,
        app_password,
    }))
}

pub async fn store(credentials: &Credentials) -> Result<()> {
    let [application, server] = lookup(&credentials.server);
    let attributes = [
        application,
        server,
        ("login", credentials.login_name.clone()),
    ];
    let label = format!(
        "Nextcloud ({})",
        credentials.server.host_str().unwrap_or("server")
    );
    keyring()
        .await?
        .create_item(&label, &attributes, credentials.app_password.as_str(), true)
        .await
        .context("saving the app password")?;
    Ok(())
}

pub async fn delete(server: &Url) -> Result<()> {
    keyring().await?.delete(&lookup(server)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dav_url_escapes_login_name() {
        let credentials = Credentials {
            server: Url::parse("https://cloud.example.com/").unwrap(),
            login_name: "jane doe@example.com".into(),
            app_password: String::new(),
        };
        assert_eq!(
            credentials.dav_url(),
            "https://cloud.example.com/remote.php/dav/files/jane%20doe@example.com"
        );
    }
}
