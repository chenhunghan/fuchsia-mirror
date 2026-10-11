// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::emulator::EMULATOR_ROOT_DRIVER_URL;
use crate::host_realm::mpsc::Receiver;
use anyhow::{Error, format_err};
use cm_rust::push_box;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_bluetooth_host::{HostMarker, ReceiverMarker, ReceiverRequestStream};
use fidl_fuchsia_component::{CreateChildArgs, RealmMarker, RealmProxy};
use fidl_fuchsia_component_decl::{
    Child, CollectionRef, ConfigOverride, ConfigSingleValue, ConfigValue, Durability, StartupMode,
};
use fidl_fuchsia_driver_test as fdt;
use fidl_fuchsia_hardware_bluetooth as fhbt;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_logger::LogSinkMarker;
use fuchsia_async::TimeoutExt as _;
use fuchsia_bluetooth::constants::{
    BT_HOST, BT_HOST_COLLECTION, BT_HOST_URL, BT_SERVICE_DIR, INTEGRATION_TIMEOUT as WATCH_TIMEOUT,
};
use fuchsia_component::client::{Service, ServiceInstanceStream};
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
    ScopedInstance,
};
use fuchsia_driver_test::{DriverTestRealmBuilder, DriverTestRealmInstance};
use futures::channel::mpsc;
use futures::{SinkExt, StreamExt, TryFutureExt, TryStreamExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

mod constants {
    pub mod receiver {
        pub const MONIKER: &str = "receiver";
    }
}

pub async fn add_host_routes(
    builder: &RealmBuilder,
    to: impl Into<fuchsia_component_test::Ref> + Clone,
) -> Result<(), Error> {
    // Route config capabilities from root to bt-init
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.HciCommandTimeout".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(10)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LegacyPairing".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(false)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.ScoOffloadPathIndex".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint8(6)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.OverrideVendorCapabilitiesVersion".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.power.SuspendEnabled".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(false)),
        }))
        .await?;

    let le_configs = vec![
        "fuchsia.bluetooth.LeSlowAdvIntervalMin",
        "fuchsia.bluetooth.LeSlowAdvIntervalMax",
        "fuchsia.bluetooth.LeSlowAdvMaxTxPower",
        "fuchsia.bluetooth.LeFastAdvIntervalMin",
        "fuchsia.bluetooth.LeFastAdvIntervalMax",
        "fuchsia.bluetooth.LeFastAdvMaxTxPower",
        "fuchsia.bluetooth.LeVeryFastAdvIntervalMin",
        "fuchsia.bluetooth.LeVeryFastAdvIntervalMax",
        "fuchsia.bluetooth.LeVeryFastAdvMaxTxPower",
        "fuchsia.bluetooth.LeActiveScanInterval",
        "fuchsia.bluetooth.LeActiveScanWindow",
        "fuchsia.bluetooth.LeBatchedScanningEnabled",
        "fuchsia.bluetooth.LeScanBatchMaxReadDelaySeconds",
        "fuchsia.bluetooth.LeScanOffloadFiltersEnabled",
    ];

    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeSlowAdvIntervalMin".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeSlowAdvIntervalMax".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeSlowAdvMaxTxPower".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int8(127)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeFastAdvIntervalMin".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeFastAdvIntervalMax".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeFastAdvMaxTxPower".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int8(127)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeVeryFastAdvIntervalMin".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeVeryFastAdvIntervalMax".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeVeryFastAdvMaxTxPower".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Int8(127)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeActiveScanInterval".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeActiveScanWindow".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint16(0)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeBatchedScanningEnabled".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(false)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeScanBatchMaxReadDelaySeconds".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Uint8(3)),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.bluetooth.LeScanOffloadFiltersEnabled".parse()?,
            value: cm_rust::ConfigValue::Single(cm_rust::ConfigSingleValue::Bool(false)),
        }))
        .await?;

    builder
        .add_capability(cm_rust::CapabilityDecl::Dictionary(cm_rust::DictionaryDecl {
            name: "bluetooth-le-config".parse()?,
            source_path: None,
        }))
        .await?;

    for config in le_configs {
        builder
            .add_route(
                Route::new()
                    .capability(Capability::configuration(config))
                    .from(Ref::self_())
                    .to(Ref::capability("bluetooth-le-config")),
            )
            .await?;
    }

    macro_rules! add_capability_route {
        ($name:expr) => {
            builder.add_route(
                Route::new()
                    .capability(Capability::configuration($name))
                    .from(Ref::self_())
                    .to(to.clone()),
            )
        };
    }

    add_capability_route!("fuchsia.bluetooth.HciCommandTimeout").await?;
    add_capability_route!("fuchsia.bluetooth.LegacyPairing").await?;
    add_capability_route!("fuchsia.bluetooth.OverrideVendorCapabilitiesVersion").await?;
    add_capability_route!("fuchsia.bluetooth.ScoOffloadPathIndex").await?;
    add_capability_route!("fuchsia.power.SuspendEnabled").await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::dictionary("bluetooth-le-config"))
                .from(Ref::self_())
                .to(to.clone()),
        )
        .await?;
    Ok(())
}

async fn resolve_test_component(
    test_components: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<fidl_fuchsia_component_resolution::Component, Error> {
    // We need to resolve our test component manually. Eventually component framework could provide
    // an introspection way of resolving your own component.
    let resolver = fuchsia_component::client::connect_to_protocol_at_path::<
        fidl_fuchsia_component_resolution::ResolverMarker,
    >("/svc/fuchsia.component.resolution.Resolver-hermetic")?;

    let mut resolved_test_component = None;
    let mut last_err = None;

    for url in test_components {
        let url_str = url.as_ref();
        match resolver.resolve(url_str).await {
            Ok(Ok(component)) => {
                resolved_test_component = Some(component);
                break;
            }
            Ok(Err(e)) => last_err = Some(format_err!("Failed to resolve {url_str}: {e:?}")),
            Err(e) => last_err = Some(format_err!("FIDL error resolving {url_str}: {e:?}")),
        }
    }

    resolved_test_component.ok_or_else(|| {
        last_err.unwrap_or_else(|| format_err!("No test component candidate URLs provided"))
    })
}

pub struct HostRealm {
    realm: RealmInstance,
    receiver: futures::lock::Mutex<Receiver<ClientEnd<HostMarker>>>,
    service_watcher: futures::lock::Mutex<ServiceInstanceStream<fhbt::ServiceMarker>>,
    next_host_id: AtomicUsize,
}

impl HostRealm {
    pub async fn create(test_component: String) -> Result<Self, Error> {
        Self::create_with_candidates([test_component]).await
    }

    /// Attempts to create a [`HostRealm`] by resolving the test component from a list of
    /// candidate URLs, using the first URL that resolves successfully.
    pub async fn create_with_candidates(
        test_components: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, Error> {
        let resolved_test_component = resolve_test_component(test_components).await?;

        let builder = RealmBuilder::new().await?;
        let _ = builder.driver_test_realm_setup().await?;

        // Mock the fuchsia.bluetooth.host.Receiver API by creating a channel where the client end
        // of the Host protocol can be extracted from |receiver|.
        // Note: The word "receiver" is overloaded. One refers to the Receiver API, the other
        // refers to the receiver end of the mpsc channel.
        let (sender, receiver) = mpsc::channel(128);
        let host_receiver = builder
            .add_local_child(
                constants::receiver::MONIKER,
                move |handles| {
                    let sender_clone = sender.clone();
                    Box::pin(Self::fake_receiver_component(sender_clone, handles))
                },
                ChildOptions::new().eager(),
            )
            .await?;

        // Create bt-host collection
        let mut realm_decl = builder.get_realm_decl().await?;
        push_box(
            &mut realm_decl.collections,
            cm_rust::CollectionDecl {
                name: BT_HOST_COLLECTION.parse().unwrap(),
                durability: Durability::SingleRun,
                environment: None,
                allowed_offers: cm_types::AllowedOffers::StaticAndDynamic,
                allow_long_names: false,
                persistent_storage: None,
            },
        );
        builder.replace_realm_decl(realm_decl).await.unwrap();

        add_host_routes(&builder, Ref::collection(BT_HOST_COLLECTION.to_string())).await?;

        let dtr_exposes = vec![Capability::service::<fhbt::ServiceMarker>().into()];
        let _ = builder.driver_test_realm_add_dtr_exposes(&dtr_exposes).await?;

        // Route capabilities between realm components and bt-host-collection
        builder
            .add_route(
                Route::new()
                    .capability(Capability::service::<fhbt::ServiceMarker>())
                    .from(Ref::child(fuchsia_driver_test::COMPONENT_NAME))
                    .to(Ref::collection(BT_HOST_COLLECTION.to_string())),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<LogSinkMarker>())
                    .capability(Capability::dictionary("diagnostics"))
                    .from(Ref::parent())
                    .to(Ref::collection(BT_HOST_COLLECTION.to_string())),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<ReceiverMarker>())
                    .from(&host_receiver)
                    .to(Ref::collection(BT_HOST_COLLECTION.to_string())),
            )
            .await?;
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<RealmMarker>())
                    .from(Ref::framework())
                    .to(Ref::parent()),
            )
            .await?;

        let instance = builder.build().await?;

        // Start DriverTestRealm
        let args = fdt::RealmArgs {
            root_driver: Some(EMULATOR_ROOT_DRIVER_URL.to_string()),
            software_devices: Some(vec![fidl_fuchsia_driver_test::SoftwareDevice {
                device_name: "bt-hci-emulator".to_string(),
                device_id: bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_BT_HCI_EMULATOR,
            }]),
            test_component: Some(resolved_test_component),
            dtr_exposes: Some(dtr_exposes),
            ..Default::default()
        };
        instance.driver_test_realm_start(args).await?;

        let service_watcher =
            Service::open_from_dir(instance.root.get_exposed_dir(), fhbt::ServiceMarker)?
                .watch()
                .await?;

        Ok(Self {
            realm: instance,
            receiver: futures::lock::Mutex::new(receiver),
            service_watcher: futures::lock::Mutex::new(service_watcher),
            next_host_id: AtomicUsize::new(0),
        })
    }

    // Wait for a newly published fuchsia.hardware.bluetooth.Service instance, create a bt-host
    // component for it in the bt-host collection in HostRealm, and wait for the component to
    // register itself with Receiver and return the client end of the Host protocol.
    pub async fn create_bt_host_in_collection(
        realm: &Arc<HostRealm>,
    ) -> Result<ClientEnd<HostMarker>, Error> {
        let instance = realm
            .service_watcher
            .lock()
            .await
            .try_next()
            .map_err(Error::from)
            .on_timeout(WATCH_TIMEOUT, || {
                Err(format_err!(
                    "timed out waiting for fuchsia.hardware.bluetooth.Service instance"
                ))
            })
            .await?
            .ok_or_else(|| format_err!("fuchsia.hardware.bluetooth.Service watcher closed"))?;
        let instance_name = instance.instance_name();
        let id = realm.next_host_id.fetch_add(1, Ordering::SeqCst);
        let component_name = format!("{BT_HOST}_{id}_{instance_name}"); // Name must only contain [a-z0-9-_]
        let device_path = format!("{BT_SERVICE_DIR}/{instance_name}/vendor");
        let collection_ref = CollectionRef { name: BT_HOST_COLLECTION.to_owned() };
        let child_decl = Child {
            name: Some(component_name),
            url: Some(BT_HOST_URL.to_owned()),
            startup: Some(StartupMode::Lazy),
            config_overrides: Some(vec![ConfigOverride {
                key: Some("device_path".to_string()),
                value: Some(ConfigValue::Single(ConfigSingleValue::String(device_path))),
                ..ConfigOverride::default()
            }]),
            ..Default::default()
        };

        let realm_proxy: RealmProxy =
            realm.instance().connect_to_protocol_at_exposed_dir().unwrap();
        let _ = realm_proxy
            .create_child(&collection_ref, &child_decl, CreateChildArgs::default())
            .await
            .map_err(|e| format_err!("{e:?}"))?
            .map_err(|e| format_err!("{e:?}"))?;

        let host = realm.receiver.lock().await.next().await.unwrap();
        Ok(host)
    }

    async fn fake_receiver_component(
        sender: mpsc::Sender<ClientEnd<HostMarker>>,
        handles: LocalComponentHandles,
    ) -> Result<(), Error> {
        let mut fs = ServiceFs::new();
        let _ = fs.dir("svc").add_fidl_service(move |mut req_stream: ReceiverRequestStream| {
            let mut sender_clone = sender.clone();
            fuchsia_async::Task::local(async move {
                let (host_server, _) =
                    req_stream.next().await.unwrap().unwrap().into_add_host().unwrap();
                sender_clone.send(host_server).await.expect("Host sent successfully");
            })
            .detach()
        });

        let _ = fs.serve_connection(handles.outgoing_dir)?;
        fs.collect::<()>().await;
        Ok(())
    }

    pub fn instance(&self) -> &ScopedInstance {
        &self.realm.root
    }

    pub fn dev(&self) -> Result<fio::DirectoryProxy, Error> {
        self.realm.driver_test_realm_connect_to_dev()
    }
}
