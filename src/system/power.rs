use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::watch;

use crate::wallframe::routing::Router;

#[zbus::proxy(
    interface = "org.freedesktop.UPower",
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower"
)]
trait UPower {
    #[zbus(property)]
    fn on_battery(&self) -> zbus::Result<bool>;
}

pub async fn run(router: Arc<Router>, mut shutdown: watch::Receiver<bool>) {
    let connection = match zbus::Connection::system().await {
        Ok(connection) => connection,
        Err(error) => {
            log::debug!("power_monitor: system bus unavailable: {error}");
            return;
        }
    };
    // Failed cached reads can outlive an unavailable UPower service.
    let proxy = match UPowerProxy::builder(&connection)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
    {
        Ok(proxy) => proxy,
        Err(error) => {
            log::warn!("power_monitor: failed to create proxy: {error}");
            return;
        }
    };
    let mut owners = match proxy.inner().receive_owner_changed().await {
        Ok(owners) => owners,
        Err(error) => {
            log::warn!("power_monitor: cannot watch UPower: {error}");
            return;
        }
    };
    let properties = match zbus::fdo::PropertiesProxy::new(
        &connection,
        "org.freedesktop.UPower",
        "/org/freedesktop/UPower",
    )
    .await
    {
        Ok(properties) => properties,
        Err(error) => {
            log::warn!("power_monitor: failed to create properties proxy: {error}");
            return;
        }
    };
    let mut changes = match properties.receive_properties_changed().await {
        Ok(changes) => changes,
        Err(error) => {
            log::warn!("power_monitor: cannot watch properties: {error}");
            return;
        }
    };
    refresh(&router, &proxy).await;

    loop {
        tokio::select! {
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            owner = owners.next() => {
                match owner {
                    Some(Some(_)) => refresh(&router, &proxy).await,
                    Some(None) => router.update_power_state(false).await,
                    None => break,
                }
            }
            changed = changes.next() => {
                let Some(changed) = changed else { break };
                let args = match changed.args() {
                    Ok(args) => args,
                    Err(error) => {
                        log::warn!("power_monitor: invalid property change: {error}");
                        continue;
                    }
                };
                if args.interface_name.as_str() == "org.freedesktop.UPower"
                    && (args.changed_properties.contains_key("OnBattery")
                        || args.invalidated_properties.contains(&"OnBattery"))
                {
                    refresh(&router, &proxy).await;
                }
            }
        }
    }
    router.update_power_state(false).await;
}

async fn refresh(router: &Arc<Router>, proxy: &UPowerProxy<'_>) {
    let on_battery = match proxy.on_battery().await {
        Ok(value) => value,
        Err(error) => {
            log::debug!("power_monitor: UPower unavailable: {error}");
            false
        }
    };
    router.update_power_state(on_battery).await;
}
