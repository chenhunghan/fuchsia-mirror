// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_power_battery::{
    BatteryInfoWatcherMarker, BatteryInfoWatcherRequest, BatteryManagerMarker, BatteryManagerProxy,
    ChargeSource,
};
use fidl_fuchsia_power_system as _;
use fuchsia_component_client::connect_to_protocol;
use futures::stream::{BoxStream, StreamExt};
use fxfs::filesystem::{PowerManager, WakeLease};
use fxfs::log::*;
use std::sync::Arc;

pub struct FuchsiaPowerManager {}

impl FuchsiaPowerManager {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {})
    }

    fn watch_battery_with_proxy(
        proxy: BatteryManagerProxy,
    ) -> BoxStream<'static, (bool, WakeLease)> {
        let (watcher_client, watcher_stream) =
            fidl::endpoints::create_request_stream::<BatteryInfoWatcherMarker>();

        if let Err(error) = proxy.watch(watcher_client) {
            error!(error:?; "Failed to register battery watcher");
            return futures::stream::empty().boxed();
        }

        futures::stream::unfold(watcher_stream, |mut stream| async move {
            match stream.next().await {
                Some(Ok(BatteryInfoWatcherRequest::OnChangeBatteryInfo {
                    info,
                    wake_lease,
                    responder,
                })) => {
                    let _ = responder.send();
                    // We check charge_source rather than charge_status because charge_status
                    // might be FULL even when the device has just been taken off charge.
                    // charge_source will indicate the actual source of power for the system.
                    let on_battery = !matches!(
                        info.charge_source,
                        Some(ChargeSource::AcAdapter)
                            | Some(ChargeSource::Usb)
                            | Some(ChargeSource::Wireless)
                    );
                    let handle =
                        wake_lease.map_or(zx::NullableHandle::invalid(), |l| l.into_handle());
                    Some(((on_battery, handle), stream))
                }
                Some(Err(error)) => {
                    error!(error:?; "Battery watcher stream error");
                    None
                }
                None => None,
            }
        })
        .boxed()
    }
}

impl PowerManager for FuchsiaPowerManager {
    fn watch_battery(self: Arc<Self>) -> BoxStream<'static, (bool, WakeLease)> {
        match connect_to_protocol::<BatteryManagerMarker>() {
            Ok(proxy) => Self::watch_battery_with_proxy(proxy),
            Err(error) => {
                warn!(error:?; "Failed to connect to BatteryManager");
                futures::stream::empty().boxed()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_power_battery::{BatteryInfo, BatteryManagerRequest};

    #[fuchsia::test]
    async fn test_watch_battery_charge_sources_and_lease() {
        let _manager = FuchsiaPowerManager::new();
        let (proxy, mut manager_stream) =
            fidl::endpoints::create_proxy_and_stream::<BatteryManagerMarker>();

        let mut battery_stream = FuchsiaPowerManager::watch_battery_with_proxy(proxy);

        let watcher_proxy = match manager_stream.next().await {
            Some(Ok(BatteryManagerRequest::Watch { watcher, .. })) => watcher.into_proxy(),
            other => panic!("Unexpected request: {:?}", other),
        };

        let server_task = fuchsia_async::Task::spawn(async move {
            // AC adapter -> on_battery = false
            watcher_proxy
                .on_change_battery_info(
                    &BatteryInfo {
                        charge_source: Some(ChargeSource::AcAdapter),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();

            // USB -> on_battery = false, with a wake lease
            let (ep1, _ep2) = zx::EventPair::create();
            watcher_proxy
                .on_change_battery_info(
                    &BatteryInfo { charge_source: Some(ChargeSource::Usb), ..Default::default() },
                    Some(ep1),
                )
                .await
                .unwrap();

            // Wireless -> on_battery = false
            watcher_proxy
                .on_change_battery_info(
                    &BatteryInfo {
                        charge_source: Some(ChargeSource::Wireless),
                        ..Default::default()
                    },
                    None,
                )
                .await
                .unwrap();

            // ChargeSource::None -> on_battery = true
            watcher_proxy
                .on_change_battery_info(
                    &BatteryInfo { charge_source: Some(ChargeSource::None), ..Default::default() },
                    None,
                )
                .await
                .unwrap();

            // None -> on_battery = true
            watcher_proxy.on_change_battery_info(&BatteryInfo::default(), None).await.unwrap();
        });

        let (on_battery, lease) = battery_stream.next().await.unwrap();
        assert!(!on_battery);
        assert!(lease.is_invalid());

        let (on_battery, lease) = battery_stream.next().await.unwrap();
        assert!(!on_battery);
        assert!(!lease.is_invalid());

        let (on_battery, lease) = battery_stream.next().await.unwrap();
        assert!(!on_battery);
        assert!(lease.is_invalid());

        let (on_battery, lease) = battery_stream.next().await.unwrap();
        assert!(on_battery);
        assert!(lease.is_invalid());

        let (on_battery, lease) = battery_stream.next().await.unwrap();
        assert!(on_battery);
        assert!(lease.is_invalid());

        server_task.await;
        assert!(battery_stream.next().await.is_none());
    }

    #[fuchsia::test]
    async fn test_watch_battery_disconnected_proxy() {
        let (proxy, manager_stream) =
            fidl::endpoints::create_proxy_and_stream::<BatteryManagerMarker>();
        drop(manager_stream);

        let mut battery_stream = FuchsiaPowerManager::watch_battery_with_proxy(proxy);
        assert!(battery_stream.next().await.is_none());
    }
}
