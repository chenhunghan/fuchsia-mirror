// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_hardware_bluetooth::{
    EmulatorError, EmulatorProxy, EmulatorServiceMarker, EmulatorSettings,
    VirtualControllerServiceMarker,
};
use fidl_fuchsia_io::DirectoryProxy;
use fuchsia_async::{DurationExt as _, TimeoutExt as _};
use fuchsia_bluetooth::constants::INTEGRATION_TIMEOUT as WATCH_TIMEOUT;
use fuchsia_component::client::Service;
use futures::{TryFutureExt as _, TryStreamExt as _};
use log::error;

pub mod types;

/// Represents a bt-hci device emulator. Instances of this type can be used to manage the
/// bt-hci-virtual emulator within the test hierarchy. The associated emulator instance gets
/// destroyed when `destroy_and_wait()` resolves successfully.
/// `destroy_and_wait()` MUST be called for proper clean up of the emulator device.
pub struct Emulator {
    /// This will have a value when the emulator is instantiated and will be reset to None
    /// in `destroy_and_wait()`. This is so the destructor can assert that the TestDevice has been
    /// destroyed.
    dev: Option<TestDevice>,
}

impl Emulator {
    /// Returns the default settings.
    // TODO(armansito): Consider defining a library type for EmulatorSettings.
    pub fn default_settings() -> EmulatorSettings {
        EmulatorSettings {
            address: None,
            hci_config: None,
            extended_advertising: None,
            acl_buffer_settings: None,
            le_acl_buffer_settings: None,
            ..Default::default()
        }
    }

    /// Publish a new bt-emulator instance and return a handle to it. No corresponding bt-hci
    /// service instance will be published; to do so it must be explicitly configured and created
    /// with a call to `publish()`.
    pub async fn create(svc_directory: &DirectoryProxy) -> Result<Emulator, Error> {
        let dev = TestDevice::create(svc_directory).await.context("Error creating test device")?;
        Ok(Emulator { dev: Some(dev) })
    }

    /// Publish a bt-emulator and a bt-hci service instance using the default emulator settings.
    pub async fn create_and_publish(svc_directory: &DirectoryProxy) -> Result<Emulator, Error> {
        let fake_dev = Self::create(svc_directory).await?;
        fake_dev.publish(Self::default_settings()).await?;
        Ok(fake_dev)
    }

    /// Sends a publish message to the emulator. This is a convenience method that internally
    /// handles the FIDL binding error.
    pub async fn publish(&self, settings: EmulatorSettings) -> Result<(), Error> {
        let dev = self.dev.as_ref().expect("emulator device accessed after it was destroyed!");
        dev.emulator
            .publish(&settings)
            .await
            .context("publish transport")?
            .map_err(|e: EmulatorError| format_err!("failed to publish bt-hci device: {:#?}", e))
    }

    /// Closes the owner connection to the test device and waits for the emulator instance to
    /// shut down.
    pub async fn destroy_and_wait(&mut self) -> Result<(), Error> {
        self.dev
            .take()
            .expect("attempted to destroy an already destroyed emulator device")
            .destroy_and_wait()
            .await
    }

    pub fn instance_name(&self) -> &str {
        let dev = self.dev.as_ref().expect("emulator device accessed after it was destroyed!");
        &dev.instance_name
    }

    pub fn emulator(&self) -> &EmulatorProxy {
        &self.dev.as_ref().unwrap().emulator
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        if self.dev.is_some() {
            error!("Did not call destroy_and_wait() on Emulator");
        }
    }
}

/// Represents the test device. `destroy_and_wait()` MUST be called explicitly to remove the
/// device.
struct TestDevice {
    owner: Option<EmulatorProxy>,
    emulator: EmulatorProxy,
    instance_name: String,
}

impl TestDevice {
    /// Creates a new emulator instance via `VirtualControllerService` and connects to its
    /// `EmulatorService` instance.
    async fn create(svc_directory: &DirectoryProxy) -> Result<TestDevice, Error> {
        let controller = Service::open_from_dir(svc_directory, VirtualControllerServiceMarker)
            .context("failed to open VirtualControllerService")?
            .watch_for_any()
            .on_timeout(WATCH_TIMEOUT.after_now(), || {
                Err(format_err!("timed out waiting for VirtualControllerService instance"))
            })
            .await
            .context("failed to find VirtualControllerService instance")?
            .connect_to_device()
            .context("failed to connect to VirtualController protocol")?;

        let name = controller
            .create_emulator()
            .map_err(Error::from)
            .on_timeout(WATCH_TIMEOUT.after_now(), || {
                Err(format_err!("timed out waiting for emulator to create test device"))
            })
            .await?
            .map_err(zx::Status::err_from_raw)?
            .ok_or_else(|| {
                format_err!("name absent from VirtualController::CreateEmulator FIDL response")
            })?;

        // Wait until the matching EmulatorService instance is published.
        let mut watcher = Service::open_from_dir(svc_directory, EmulatorServiceMarker)
            .context("failed to open EmulatorService")?
            .watch()
            .await
            .context("failed to watch EmulatorService")?;
        let instance = async {
            while let Some(instance) = watcher.try_next().await? {
                if instance.instance_name() == name {
                    return Ok(instance);
                }
            }
            Err(format_err!("EmulatorService watcher closed before {name} appeared"))
        }
        .on_timeout(WATCH_TIMEOUT, || Err(format_err!("timed out waiting for device to appear")))
        .await?;

        // The first Emulator connection is the owner connection: closing it triggers
        // EmulatorDevice::Shutdown() and closes all other Emulator bindings. Keep `owner`
        // private and uncloned so `destroy_and_wait()` can deterministically close it even when
        // callers clone `self.emulator`.
        let owner = instance.connect_to_device().context("failed to connect owner to Emulator")?;
        let emulator = instance.connect_to_device().context("failed to connect to Emulator")?;

        Ok(Self { owner: Some(owner), emulator, instance_name: name })
    }

    /// Drops the owner connection and waits for the emulator to close the shared channel.
    pub async fn destroy_and_wait(&mut self) -> Result<(), Error> {
        drop(self.owner.take());
        let _: zx::Signals = self.emulator.as_channel().on_closed().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_hardware_bluetooth::ServiceMarker;
    use fuchsia_component_test::{Capability, ChildOptions, RealmBuilder, Ref, Route};

    const BT_HCI_VIRTUAL_URL: &str = "virtual#meta/bt-hci-virtual.cm";
    const BT_HCI_VIRTUAL_MONIKER: &str = "bt-hci-virtual";

    fn default_settings() -> EmulatorSettings {
        EmulatorSettings {
            address: None,
            hci_config: None,
            extended_advertising: None,
            acl_buffer_settings: None,
            le_acl_buffer_settings: None,
            ..Default::default()
        }
    }

    // Tests that creating, publishing, and destroying an Emulator instance creates and closes
    // its EmulatorService instance.
    #[fuchsia::test]
    async fn test_publish_lifecycle() {
        let builder = RealmBuilder::new().await.expect("realm builder");
        let bt_hci_virtual = builder
            .add_child(BT_HCI_VIRTUAL_MONIKER, BT_HCI_VIRTUAL_URL, ChildOptions::new())
            .await
            .expect("add bt-hci-virtual");
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                    .from(Ref::parent())
                    .to(&bt_hci_virtual),
            )
            .await
            .expect("route LogSink");
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<VirtualControllerServiceMarker>())
                    .capability(Capability::service::<EmulatorServiceMarker>())
                    .capability(Capability::service::<ServiceMarker>())
                    .from(&bt_hci_virtual)
                    .to(Ref::parent()),
            )
            .await
            .expect("route bluetooth services");
        let realm = builder.build().await.expect("failed to build realm");

        let svc_dir = realm.root.get_exposed_dir();
        let mut fake_dev = Emulator::create(svc_dir).await.expect("Failed to construct Emulator");

        // An EmulatorService instance matching `fake_dev.instance_name()` should already exist.
        let mut watcher = Service::open_from_dir(svc_dir, EmulatorServiceMarker)
            .expect("open EmulatorService")
            .watch()
            .await
            .expect("watch EmulatorService");
        let emul_dev = async {
            while let Some(instance) = watcher.try_next().await.expect("watch next") {
                if instance.instance_name() == fake_dev.instance_name() {
                    return instance.connect_to_device().expect("failed to connect to device");
                }
            }
            panic!("EmulatorService watcher closed before instance appeared");
        }
        .on_timeout(WATCH_TIMEOUT, || panic!("timed out waiting for device to appear"))
        .await;

        // Send a publish message to the device. This call should succeed and result in a new
        // bt-hci service instance.
        let () = fake_dev
            .publish(default_settings())
            .await
            .expect("Failed to send Publish message to emulator device");

        // Once a device is published, it should not be possible to publish again while the
        // Emulator is open.
        let result = fake_dev
            .emulator()
            .publish(&default_settings())
            .await
            .expect("Failed to send second Publish message to emulator device");
        assert_eq!(Err(EmulatorError::HciAlreadyPublished), result);

        fake_dev.destroy_and_wait().await.expect("Expected test device to be removed");

        // Observer Emulator connection should be closed when `fake_dev` is destroyed.
        let _ = emul_dev
            .as_channel()
            .on_closed()
            .on_timeout(WATCH_TIMEOUT, || panic!("timed out waiting for device to close"))
            .await
            .expect("on closed");
    }
}
