// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context as _, Error, format_err};
use async_trait::async_trait;
use bt_init_config::Config;
use fidl::endpoints::{DiscoverableProtocolMarker, Proxy};
use fidl_fuchsia_bluetooth_bredr::ProfileMarker;
use fidl_fuchsia_bluetooth_snoop::SnoopMarker;
use fidl_fuchsia_bluetooth_sys::PairingMarker;
use fidl_fuchsia_component::{CreateChildArgs, RealmMarker, RealmProxy};
use fidl_fuchsia_component_decl::{
    Child, CollectionRef, ConfigOverride, ConfigSingleValue, ConfigValue, StartupMode,
};
use fidl_fuchsia_component_runtime as fruntime;
use fidl_fuchsia_io as fio;
use fuchsia_async as fasync;
use fuchsia_bluetooth::constants::{BT_HOST, BT_HOST_COLLECTION, BT_HOST_URL, BT_SERVICE_DIR};
use fuchsia_component::{client, server};
use futures::{StreamExt, future};
use log::{debug, error, info, warn};

const BT_GAP_CHILD_NAME: &str = "bt-gap";
const BT_RFCOMM_CHILD_NAME: &str = "bt-rfcomm";
const BT_FASTPAIR_PROVIDER_CHILD_NAME: &str = "bt-fastpair-provider";

#[async_trait]
trait ComponentClientAdapter {
    async fn open_childs_exposed_directory(
        &mut self,
        child_name: String,
    ) -> Result<fio::DirectoryProxy, Error>;
}

struct ComponentClient;

#[async_trait]
impl ComponentClientAdapter for ComponentClient {
    async fn open_childs_exposed_directory(
        &mut self,
        child_name: String,
    ) -> Result<fio::DirectoryProxy, Error> {
        client::open_childs_exposed_directory(child_name, None).await
    }
}

/// Open the directory of the child which will underlie any services. The specified child is the
/// preferred service provider, if present.If it is not specified, falls back to `bt-gap`.
//
// TODO(https://fxbug.dev/42150654): A single child instance won't function correctly in the presence
// of multiple bt-host devices during its lifetime. When handling this is a priority, we will
// likely need to either launch an instance of the child per-bt-host (e.g. inside bt-gap), or
// modify the child component to accommodate this issue.
async fn open_childs_service_directory<C: ComponentClientAdapter>(
    maybe_child_name: Option<&str>,
    component_client: &mut C,
) -> Result<fio::DirectoryProxy, Error> {
    let Some(child_name) = maybe_child_name else {
        // Use bt-gap if no child is specified.
        let dir =
            component_client.open_childs_exposed_directory(BT_GAP_CHILD_NAME.to_owned()).await?;
        return Ok(dir);
    };

    let child_service = component_client.open_childs_exposed_directory(child_name.to_owned()).await;
    match child_service {
        Ok(dir) => {
            debug!("successfully opened `{child_name}` service directory");
            Ok(dir)
        }
        Err(e) => {
            warn!("Failed to open '{child_name}' service directory: {e:?}");
            Err(e)
        }
    }
}

// Use the fuchsia.component.Realm protocol to create a dynamic child instance in the collection.
async fn create_bt_host(
    realm: &RealmProxy,
    capabilities: &fruntime::CapabilitiesProxy,
    instance_name: &str,
) -> Result<(), Error> {
    let component_name = format!("{BT_HOST}_{instance_name}");
    let collection_ref = CollectionRef { name: BT_HOST_COLLECTION.to_owned() };

    let service_path = format!("{BT_SERVICE_DIR}/{instance_name}/vendor");

    info!("Creating component: {component_name:?} with service path: {service_path:?}");

    let config_value = ConfigValue::Single(ConfigSingleValue::String(service_path));
    let (data_handle, data_other_end) = zx::EventPair::create();
    capabilities
        .data_create(data_other_end, &fruntime::Data::Bytes(fidl::persist(&config_value)?))
        .await?
        .map_err(|e| format_err!("{e:?}"))?;
    let (dictionary, dictionary_other_end) = zx::EventPair::create();
    capabilities
        .dictionary_create(dictionary_other_end)
        .await?
        .map_err(|e| format_err!("{e:?}"))?;
    let dictionary_dup = dictionary.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
    capabilities
        .dictionary_insert(dictionary_dup, "fuchsia.bluetooth.DevicePath", data_handle)
        .await?
        .map_err(|e| format_err!("{e:?}"))?;

    // Retain config_overrides alongside additional_inputs for compatibility with prebuilt bt-host
    // until third_party/pigweed rolls its bt-host.cml configuration capability migration.
    let child_decl = Child {
        name: Some(component_name),
        url: Some(BT_HOST_URL.to_owned()),
        startup: Some(StartupMode::Lazy),
        config_overrides: Some(vec![ConfigOverride {
            key: Some("device_path".to_string()),
            value: Some(config_value),
            ..ConfigOverride::default()
        }]),
        ..Default::default()
    };

    let child_args = CreateChildArgs { additional_inputs: Some(dictionary), ..Default::default() };

    realm
        .create_child(&collection_ref, &child_decl, child_args)
        .await?
        .map_err(|e| format_err!("{e:?}"))?;
    Ok(())
}

/// Continuously watch for bt vendor devices being added via Service
async fn run_device_watcher() -> Result<(), Error> {
    let realm = client::connect_to_protocol::<RealmMarker>()
        .expect("failed to connect to fuchsia.component.Realm");
    let capabilities = client::connect_to_protocol::<fruntime::CapabilitiesMarker>()
        .expect("failed to connect to fuchsia.component.runtime.Capabilities");

    let service =
        fuchsia_component::client::Service::open(fidl_fuchsia_hardware_bluetooth::ServiceMarker)
            .with_context(|| format!("failed to open {BT_SERVICE_DIR}"))?;
    let mut watcher =
        service.watch().await.with_context(|| format!("failed to watch {BT_SERVICE_DIR}"))?;

    while let Some(instance) = watcher.next().await {
        match instance {
            Ok(instance) => {
                let name = instance.instance_name().to_string();
                info!("Discovered bluetooth service instance: {name}");
                if let Err(e) = create_bt_host(&realm, &capabilities, &name).await {
                    warn!("Failed to create bt-host for service instance {name}: {e:?}");
                }
            }
            Err(e) => {
                warn!("Error watching bluetooth service instances: {:?}", e);
            }
        }
    }
    Ok(())
}

#[fuchsia::main(logging_tags = ["bt-init"])]
fn main() -> Result<(), Error> {
    let cfg = Config::take_from_startup_handle();
    info!("Starting bt-init: {cfg:?}");

    let mut executor = fasync::LocalExecutorBuilder::new().build();

    // Start bt-snoop service before anything else and hold onto the connection until bt-init exits.
    let snoop_connection;
    if cfg.autostart_snoop {
        info!("starting snoop service...");
        snoop_connection = client::connect_to_protocol::<SnoopMarker>();
        if let Err(e) = snoop_connection {
            warn!("failed to start snoop service: {}", e);
        } else {
            info!("snoop service started successfully");
        }
    }

    // Watch for vendor device drivers and spawn bt-host component if any are found
    let _task = fasync::Task::spawn(async move {
        run_device_watcher().await.unwrap_or_else(|e| error!("Error watching devices: {:?}", e))
    });

    let run_bluetooth = async move {
        let mut component_client = ComponentClient;
        // Get the backing service directory of the `bt-rfcomm` and `bt-fastpair-provider` child
        // components.
        let rfcomm_child = cfg.rfcomm_enabled.then_some(BT_RFCOMM_CHILD_NAME);
        let underlying_profile_svc =
            open_childs_service_directory(rfcomm_child, &mut component_client).await?;

        let fastpair_child =
            cfg.fastpair_provider_enabled.then_some(BT_FASTPAIR_PROVIDER_CHILD_NAME);
        let underlying_pairing_svc =
            open_childs_service_directory(fastpair_child, &mut component_client).await?;

        // Expose the `bredr.Profile` and `sys.Pairing` protocols to the system.
        let mut fs = server::ServiceFs::new();
        let _ = fs
            .dir("svc")
            .add_service_at(ProfileMarker::PROTOCOL_NAME, |chan| {
                Some((ProfileMarker::PROTOCOL_NAME, chan))
            })
            .add_service_at(PairingMarker::PROTOCOL_NAME, |chan| {
                Some((PairingMarker::PROTOCOL_NAME, chan))
            });
        let _ = fs.take_and_serve_directory_handle()?;

        info!(
            "initialization complete, begin serving [{}, {}]",
            ProfileMarker::PROTOCOL_NAME,
            PairingMarker::PROTOCOL_NAME
        );
        let outer_fs = fs.for_each(move |(name, chan)| {
            let directory = match name {
                ProfileMarker::PROTOCOL_NAME => underlying_profile_svc.as_channel().as_ref(),
                PairingMarker::PROTOCOL_NAME => underlying_pairing_svc.as_channel().as_ref(),
                name => {
                    error!(
                        "Received unexpected service {} when we only expect to serve [{}, {}]",
                        name,
                        ProfileMarker::PROTOCOL_NAME,
                        PairingMarker::PROTOCOL_NAME,
                    );
                    return future::ready(());
                }
            };
            let _ = fdio::service_connect_at(directory, name, chan)
                .map_err(|e| warn!("error passing {} handle to service: {:?}", name, e));
            future::ready(())
        });
        Ok::<(), Error>(outer_fs.await)
    };

    executor
        .run_singlethreaded(run_bluetooth)
        .context("bt-init encountered an error during execution")
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::format_err;
    use fuchsia_async::Channel as AsyncChannel;

    use std::collections::HashSet;
    struct MockComponentClient {
        pub children_to_fail_for: HashSet<String>,
        pub bt_gap_channel: Option<zx::Channel>,
        pub bt_rfcomm_channel: Option<zx::Channel>,
        pub bt_fastpair_provider_channel: Option<zx::Channel>,
    }

    impl MockComponentClient {
        fn new() -> Self {
            Self {
                children_to_fail_for: HashSet::new(),
                bt_gap_channel: None,
                bt_rfcomm_channel: None,
                bt_fastpair_provider_channel: None,
            }
        }
    }

    #[async_trait]
    impl ComponentClientAdapter for MockComponentClient {
        async fn open_childs_exposed_directory(
            &mut self,
            child_name: String,
        ) -> Result<fio::DirectoryProxy, Error> {
            if self.children_to_fail_for.contains(&child_name) {
                return Err(format_err!("couldn't open {}'s directory", &child_name));
            }
            let (local, client) = zx::Channel::create();
            match child_name.as_str() {
                BT_RFCOMM_CHILD_NAME => self.bt_rfcomm_channel = Some(local),
                BT_GAP_CHILD_NAME => self.bt_gap_channel = Some(local),
                BT_FASTPAIR_PROVIDER_CHILD_NAME => self.bt_fastpair_provider_channel = Some(local),
                _ => panic!("MockComponentClient received unexpected child name: {}", child_name),
            }
            Ok(fio::DirectoryProxy::from_channel(AsyncChannel::from_channel(client)))
        }
    }

    fn assert_channels_connected(writer: &zx::Channel, reader: &zx::Channel) {
        let expected_bytes = [1, 2, 3, 4, 5];
        writer.write(&expected_bytes, &mut []).unwrap();
        let mut bytes = zx::MessageBuf::new();
        reader.read(&mut bytes).unwrap();
        assert_eq!(&expected_bytes, bytes.bytes());
    }

    #[fuchsia::test]
    async fn test_open_fastpair_provider_works() {
        let mut mock_client = MockComponentClient::new();

        // Directory should be connected to `bt-fastpair-provider`.
        let child_directory =
            open_childs_service_directory(Some(BT_FASTPAIR_PROVIDER_CHILD_NAME), &mut mock_client)
                .await
                .unwrap();
        assert!(mock_client.bt_fastpair_provider_channel.is_some());
        assert_channels_connected(
            mock_client.bt_fastpair_provider_channel.unwrap().as_ref(),
            child_directory.as_channel().as_ref(),
        );
        // `bt-gap`s directory should not be used.
        assert!(mock_client.bt_gap_channel.is_none());
    }

    #[fuchsia::test]
    async fn test_open_rfcomm_works() {
        let mut mock_client = MockComponentClient::new();

        // If opening bt-rfcomm's directory works, the directory should be connected to bt-rfcomm.
        let profile_svc =
            open_childs_service_directory(Some(BT_RFCOMM_CHILD_NAME), &mut mock_client)
                .await
                .unwrap();
        assert!(mock_client.bt_rfcomm_channel.is_some());
        assert_channels_connected(
            mock_client.bt_rfcomm_channel.unwrap().as_ref(),
            profile_svc.as_channel().as_ref(),
        );
        // `bt-gap`s directory should not be used.
        assert!(mock_client.bt_gap_channel.is_none());
    }

    #[fuchsia::test]
    async fn test_open_child_fails() {
        let mut mock_client = MockComponentClient::new();
        let _ = mock_client.children_to_fail_for.insert(BT_RFCOMM_CHILD_NAME.to_owned());
        let _ = mock_client.children_to_fail_for.insert(BT_FASTPAIR_PROVIDER_CHILD_NAME.to_owned());
        assert!(
            open_childs_service_directory(Some(BT_RFCOMM_CHILD_NAME), &mut mock_client)
                .await
                .is_err()
        );
        assert!(
            open_childs_service_directory(Some(BT_FASTPAIR_PROVIDER_CHILD_NAME), &mut mock_client)
                .await
                .is_err()
        );
    }

    #[fuchsia::test]
    async fn test_child_disabled_uses_bt_gap() {
        let mut mock_client = MockComponentClient::new();
        let _ = open_childs_service_directory(None, &mut mock_client).await.unwrap();
        assert!(mock_client.bt_gap_channel.is_some());
        assert!(mock_client.bt_rfcomm_channel.is_none());
        assert!(mock_client.bt_fastpair_provider_channel.is_none());
    }

    /// Handles the expected `DataCreate`, `DictionaryCreate`, and `DictionaryInsert` requests for
    /// `create_bt_host`, verifying the persisted `ConfigValue` and `EventPair` peer koids across
    /// requests, and returns the server end of the created dictionary `EventPair`.
    async fn handle_expected_capabilities_requests(
        capabilities_stream: &mut fruntime::CapabilitiesRequestStream,
        expected_device_path: &str,
    ) -> zx::EventPair {
        let Some(Ok(fruntime::CapabilitiesRequest::DataCreate { data_handle, data, responder })) =
            capabilities_stream.next().await
        else {
            panic!("Expected DataCreate request");
        };
        let data_create_info = data_handle.basic_info().unwrap();
        assert_ne!(data_create_info.koid, data_create_info.related_koid);
        let fruntime::Data::Bytes(bytes) = data else {
            panic!("Expected Data::Bytes");
        };
        let decoded: ConfigValue = fidl::unpersist(&bytes).expect("valid persisted ConfigValue");
        assert_eq!(
            decoded,
            ConfigValue::Single(ConfigSingleValue::String(expected_device_path.to_string()))
        );
        responder.send(Ok(())).unwrap();

        let Some(Ok(fruntime::CapabilitiesRequest::DictionaryCreate {
            dictionary: dict_server,
            responder,
        })) = capabilities_stream.next().await
        else {
            panic!("Expected DictionaryCreate request");
        };
        let dict_create_info = dict_server.basic_info().unwrap();
        assert_ne!(dict_create_info.koid, dict_create_info.related_koid);
        responder.send(Ok(())).unwrap();

        let Some(Ok(fruntime::CapabilitiesRequest::DictionaryInsert {
            dictionary: insert_dict,
            key,
            value,
            responder,
        })) = capabilities_stream.next().await
        else {
            panic!("Expected DictionaryInsert request");
        };
        assert_eq!(key, "fuchsia.bluetooth.DevicePath");

        // `insert_dict` is a duplicate of the client dictionary handle paired with `dict_server`.
        let insert_dict_info = insert_dict.basic_info().unwrap();
        assert_eq!(insert_dict_info.koid, dict_create_info.related_koid);
        assert_eq!(insert_dict_info.related_koid, dict_create_info.koid);

        // `value` is the client data handle paired with `DataCreate`'s `data_handle`.
        let value_info = value.basic_info().unwrap();
        assert_eq!(value_info.koid, data_create_info.related_koid);
        assert_eq!(value_info.related_koid, data_create_info.koid);

        responder.send(Ok(())).unwrap();
        dict_server
    }

    #[fuchsia::test]
    async fn test_create_bt_host_success() {
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<RealmMarker>();
        let (capabilities_proxy, mut capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();

        let create_fut = create_bt_host(&realm_proxy, &capabilities_proxy, "instance-1");

        let mock_fut = async move {
            let expected_path = format!("{BT_SERVICE_DIR}/instance-1/vendor");
            let dict_server =
                handle_expected_capabilities_requests(&mut capabilities_stream, &expected_path)
                    .await;
            let dict_server_info = dict_server.basic_info().unwrap();

            let Some(Ok(fidl_fuchsia_component::RealmRequest::CreateChild {
                collection,
                decl,
                args,
                responder,
            })) = realm_stream.next().await
            else {
                panic!("Expected CreateChild request");
            };

            assert_eq!(collection.name, BT_HOST_COLLECTION);
            assert_eq!(decl.name.as_deref(), Some("bt-host_instance-1"));
            assert_eq!(decl.url.as_deref(), Some(BT_HOST_URL));
            assert_eq!(decl.startup, Some(StartupMode::Lazy));
            let overrides = decl.config_overrides.expect("config overrides present");
            assert_eq!(overrides.len(), 1);
            assert_eq!(overrides[0].key.as_deref(), Some("device_path"));
            assert_eq!(
                overrides[0].value,
                Some(ConfigValue::Single(ConfigSingleValue::String(expected_path)))
            );
            let additional_inputs = args.additional_inputs.expect("additional_inputs present");
            let additional_inputs_info = additional_inputs.basic_info().unwrap();
            assert_eq!(additional_inputs_info.koid, dict_server_info.related_koid);
            assert_eq!(additional_inputs_info.related_koid, dict_server_info.koid);

            responder.send(Ok(())).unwrap();
        };

        let (result, ()) = futures::future::join(create_fut, mock_fut).await;
        assert!(result.is_ok());
    }

    #[fuchsia::test]
    async fn test_create_bt_host_component_error() {
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<RealmMarker>();
        let (capabilities_proxy, mut capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();

        let create_fut = create_bt_host(&realm_proxy, &capabilities_proxy, "instance-1");

        let mock_fut = async move {
            let expected_path = format!("{BT_SERVICE_DIR}/instance-1/vendor");
            let _dict_server =
                handle_expected_capabilities_requests(&mut capabilities_stream, &expected_path)
                    .await;

            let Some(Ok(fidl_fuchsia_component::RealmRequest::CreateChild { responder, .. })) =
                realm_stream.next().await
            else {
                panic!("Expected CreateChild request");
            };

            responder.send(Err(fidl_fuchsia_component::Error::InstanceAlreadyExists)).unwrap();
        };

        let (result, ()) = futures::future::join(create_fut, mock_fut).await;
        assert!(result.is_err());
    }

    #[fuchsia::test]
    async fn test_create_bt_host_fidl_error() {
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<RealmMarker>();
        let (capabilities_proxy, mut capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();

        let create_fut = create_bt_host(&realm_proxy, &capabilities_proxy, "instance-1");

        let mock_fut = async move {
            let expected_path = format!("{BT_SERVICE_DIR}/instance-1/vendor");
            let _dict_server =
                handle_expected_capabilities_requests(&mut capabilities_stream, &expected_path)
                    .await;

            let Some(Ok(fidl_fuchsia_component::RealmRequest::CreateChild { responder, .. })) =
                realm_stream.next().await
            else {
                panic!("Expected CreateChild request");
            };

            drop(responder);
        };

        let (result, ()) = futures::future::join(create_fut, mock_fut).await;
        assert!(result.is_err());
    }

    #[fuchsia::test]
    async fn test_create_bt_host_capabilities_error() {
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<RealmMarker>();
        let (capabilities_proxy, mut capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();

        let result = {
            let create_fut = create_bt_host(&realm_proxy, &capabilities_proxy, "instance-1");
            let mock_fut = async move {
                let Some(Ok(fruntime::CapabilitiesRequest::DataCreate { responder, .. })) =
                    capabilities_stream.next().await
                else {
                    panic!("Expected DataCreate request");
                };
                responder.send(Err(fruntime::CapabilitiesError::InvalidData)).unwrap();
            };
            let (result, ()) = futures::future::join(create_fut, mock_fut).await;
            result
        };

        assert!(result.is_err());
        drop(realm_proxy);
        assert!(realm_stream.next().await.is_none());
    }

    #[fuchsia::test]
    async fn test_create_bt_host_capabilities_fidl_error() {
        let (realm_proxy, mut realm_stream) =
            fidl::endpoints::create_proxy_and_stream::<RealmMarker>();
        let (capabilities_proxy, capabilities_stream) =
            fidl::endpoints::create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        drop(capabilities_stream);

        let result = create_bt_host(&realm_proxy, &capabilities_proxy, "instance-1").await;
        assert!(result.is_err());
        drop(realm_proxy);
        assert!(realm_stream.next().await.is_none());
    }
}
