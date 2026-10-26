//! Follows NetworkManager's view of whether the connection is metered, such as a phone
//! hotspot, and holds syncing while it is.

use futures_util::StreamExt;
use zbus::{Connection, Proxy};

use crate::daemon::Pause;
use crate::tray::Indicator;

/// `NMMetered` values that count as metered: "yes" and "guess yes". Hotspots of phones
/// are usually only guessed, from what their DHCP server announces.
const METERED: [u32; 2] = [1, 3];

/// Runs for the daemon's lifetime. Without NetworkManager the connection counts as
/// unmetered.
pub async fn follow_metered(pause: Pause, indicator: Indicator) {
    if let Err(err) = follow(&pause, &indicator).await {
        tracing::info!("not pausing on metered connections: {err}");
    }
    apply(&pause, &indicator, false).await;
}

async fn follow(pause: &Pause, indicator: &Indicator) -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let network_manager = Proxy::new(
        &connection,
        "org.freedesktop.NetworkManager",
        "/org/freedesktop/NetworkManager",
        "org.freedesktop.NetworkManager",
    )
    .await?;
    // the stream starts with the current value, then follows every change
    let mut changes = network_manager
        .receive_property_changed::<u32>("Metered")
        .await;
    while let Some(change) = changes.next().await {
        let metered = change.get().await?;
        tracing::debug!("NetworkManager reports metered = {metered}");
        apply(pause, indicator, METERED.contains(&metered)).await;
    }
    Ok(())
}

async fn apply(pause: &Pause, indicator: &Indicator, metered: bool) {
    pause.set_metered(metered);
    indicator.refresh().await;
}
