use notify_rust::{Notification, Urgency};

/// A notification with the app's name and icon.
fn notification(summary: &str, body: &str) -> Notification {
    let mut notification = Notification::new();
    notification
        .appname("Nextcloud")
        .icon("folder-remote")
        .summary(summary)
        .body(body);
    notification
}

/// Shows a desktop notification. Failures only get logged, the daemon works without one.
pub async fn notify(summary: &str, body: &str) {
    if let Err(err) = notification(summary, body).show_async().await {
        tracing::debug!("desktop notification: {err}");
    }
}

/// Asks the user to log in; clicking the notification opens the login page.
pub async fn login_prompt(url: String) {
    let result = notification(
        "Log in to Nextcloud",
        "File sync needs access to your account.",
    )
    .action("default", "Log in")
    .action("login", "Log in")
    .urgency(Urgency::Critical)
    .show_async()
    .await;
    match result {
        Ok(handle) => {
            tokio::task::spawn_blocking(move || {
                handle.wait_for_action(|action| {
                    if action == "default" || action == "login" {
                        open(&url);
                    }
                })
            });
        }
        Err(err) => tracing::debug!("desktop notification: {err}"),
    }
}

/// Opens `url` in the browser. The child is not awaited; tokio reaps it in the background.
pub fn open(url: &str) {
    if let Err(err) = tokio::process::Command::new("xdg-open").arg(url).spawn() {
        tracing::warn!("opening the browser: {err}");
    }
}
