// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::errors::UtilsError;
use fidl_fuchsia_component_runtime as fruntime;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::runtime::{Dictionary, DirConnector};
use std::collections::HashMap;

/// A key that `import_dictionary` looks up to check that a handle references a dictionary. Whether
/// or not the key is present doesn't matter.
const IMPORT_PROBE_KEY: &str = "driver_manager.dictionary_probe";

/// Opens a new connection to `fuchsia.component.runtime.Capabilities`.
pub type CapabilitiesConnector = Box<dyn Fn() -> Result<fruntime::CapabilitiesProxy, UtilsError>>;

/// Performs the `fuchsia.component.runtime.Capabilities` operations that driver manager needs.
///
/// Every failure is returned as an error. Driver manager is a critical process, so this calls the
/// FIDL protocol directly instead of using the `fuchsia_component::runtime` helpers, which panic
/// when an operation fails.
pub struct DictionaryUtil {
    capabilities_proxy: fruntime::CapabilitiesProxy,
    connect_capabilities: CapabilitiesConnector,
}

impl DictionaryUtil {
    pub fn new(capabilities_proxy: fruntime::CapabilitiesProxy) -> Self {
        Self::new_with_connector(
            capabilities_proxy,
            Box::new(|| {
                connect_to_protocol::<fruntime::CapabilitiesMarker>()
                    .map_err(|e| UtilsError::Connect(format!("{e:#}")))
            }),
        )
    }

    /// `connect_capabilities` opens the dedicated connection that each route operation uses.
    pub fn new_with_connector(
        capabilities_proxy: fruntime::CapabilitiesProxy,
        connect_capabilities: CapabilitiesConnector,
    ) -> Self {
        Self { capabilities_proxy, connect_capabilities }
    }

    /// Checks that `dictionary` references a dictionary that driver manager can use, and wraps it.
    pub async fn import_dictionary(
        &self,
        dictionary: zx::EventPair,
    ) -> Result<Dictionary, UtilsError> {
        // Every later operation duplicates the handle, so check that it can be duplicated now.
        let probe = dictionary.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let (_value, value_peer) = zx::EventPair::create();
        match self.capabilities_proxy.dictionary_get(probe, IMPORT_PROBE_KEY, value_peer).await? {
            Ok(_) | Err(fruntime::CapabilitiesError::NoSuchCapability) => {}
            Err(e) => return Err(UtilsError::InvalidDictionary(e)),
        }
        Ok(Dictionary { handle: dictionary, capabilities_proxy: self.capabilities_proxy.clone() })
    }

    /// Creates a shallow copy of `dictionary`. Component manager drains the dictionary that it is
    /// given in `fuchsia.component/CreateChildArgs.additional_inputs`, so each component is given
    /// a copy.
    pub async fn copy_dictionary(
        &self,
        dictionary: &Dictionary,
    ) -> Result<zx::EventPair, UtilsError> {
        let (copy, copy_peer) = zx::EventPair::create();
        self.capabilities_proxy
            .dictionary_copy(
                dictionary.handle.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
                copy_peer,
            )
            .await??;
        Ok(copy)
    }

    pub async fn dictionary_dir_connector_route(
        &self,
        dictionary: &Dictionary,
        service_name: &str,
    ) -> Result<DirConnector, UtilsError> {
        let (router, router_peer) = zx::EventPair::create();
        let capability_type = self
            .capabilities_proxy
            .dictionary_get(
                dictionary.handle.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
                service_name,
                router_peer,
            )
            .await??;
        if capability_type != fruntime::CapabilityType::DirConnectorRouter {
            return Err(UtilsError::UnexpectedCapabilityType(
                "DirConnectorRouter".to_string(),
                format!("{capability_type:?}"),
            ));
        }

        // Component manager serves each `Capabilities` connection one request at a time and waits
        // for routes to finish before reading the next request. Routing on the shared connection
        // would let a slow route stall every other dictionary operation, so each route gets its
        // own connection.
        let route_proxy = (self.connect_capabilities)()?;
        // The instance token identifies driver manager as the component the route is performed
        // for.
        let (instance_token, instance_token_peer) = zx::EventPair::create();
        route_proxy.instance_token_create(instance_token_peer).await??;
        let (connector, connector_peer) = zx::EventPair::create();
        // An empty request makes the router use the request defined by its declaration.
        let response = route_proxy
            .dir_connector_router_route(
                router,
                &fruntime::RouteRequest::default(),
                instance_token,
                connector_peer,
            )
            .await??;
        match response {
            fruntime::RouterResponse::Success => Ok(DirConnector {
                handle: connector,
                capabilities_proxy: self.capabilities_proxy.clone(),
            }),
            fruntime::RouterResponse::Unavailable => {
                Err(UtilsError::Unavailable(service_name.to_string()))
            }
            other => Err(UtilsError::UnexpectedCapabilityType(
                "RouterResponse::Success".to_string(),
                format!("{other:?}"),
            )),
        }
    }

    pub async fn create_aggregate_dictionary(
        &self,
        sources: HashMap<String, Vec<fruntime::AggregateSource>>,
    ) -> Result<Dictionary, UtilsError> {
        let (dictionary, dictionary_peer) = zx::EventPair::create();
        self.capabilities_proxy.dictionary_create(dictionary_peer).await??;

        for (service_name, sources) in sources.into_iter() {
            let (aggregate, aggregate_peer) = zx::EventPair::create();
            self.capabilities_proxy.service_aggregate_create(aggregate_peer, sources).await??;
            self.capabilities_proxy
                .dictionary_insert(
                    dictionary.duplicate_handle(zx::Rights::SAME_RIGHTS)?,
                    &service_name,
                    aggregate,
                )
                .await??;
        }

        Ok(Dictionary { handle: dictionary, capabilities_proxy: self.capabilities_proxy.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use futures::StreamExt;
    use std::cell::RefCell;

    /// Returns a connector that hands out `proxy` once.
    fn connector_for(proxy: fruntime::CapabilitiesProxy) -> CapabilitiesConnector {
        let proxy = RefCell::new(Some(proxy));
        Box::new(move || {
            proxy.borrow_mut().take().ok_or_else(|| UtilsError::Connect("already used".to_string()))
        })
    }

    /// Returns a connector that must not be used.
    fn unused_connector() -> CapabilitiesConnector {
        Box::new(|| -> Result<fruntime::CapabilitiesProxy, UtilsError> {
            panic!("unexpected connection to fuchsia.component.runtime.Capabilities")
        })
    }

    /// Imports a dictionary using a server that answers the import's probe with `probe_result`.
    async fn import_with_probe_result(
        probe_result: Result<fruntime::CapabilityType, fruntime::CapabilitiesError>,
    ) -> Result<Dictionary, UtilsError> {
        let (proxy, mut stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy, unused_connector());
        let server = fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DictionaryGet {
                    key, responder, ..
                })) => {
                    assert_eq!(key, IMPORT_PROBE_KEY);
                    responder.send(probe_result).unwrap();
                }
                other => panic!("expected DictionaryGet, got {other:?}"),
            }
        });
        let (dictionary, _dictionary_peer) = zx::EventPair::create();
        let result = util.import_dictionary(dictionary).await;
        server.await;
        result
    }

    #[fuchsia::test]
    async fn test_import_dictionary() {
        let (proxy, mut stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy, unused_connector());
        let (dictionary, dictionary_peer) = zx::EventPair::create();
        let koid = dictionary.basic_info().unwrap().koid;
        let server = fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DictionaryGet {
                    dictionary,
                    responder,
                    ..
                })) => {
                    // The probe is performed on a duplicate of the imported handle.
                    assert_eq!(
                        dictionary.basic_info().unwrap().related_koid,
                        dictionary_peer.basic_info().unwrap().koid
                    );
                    responder.send(Err(fruntime::CapabilitiesError::NoSuchCapability)).unwrap();
                }
                other => panic!("expected DictionaryGet, got {other:?}"),
            }
        });

        let dict = util.import_dictionary(dictionary).await.expect("Import failed");
        assert_eq!(dict.handle.basic_info().unwrap().koid, koid);
        server.await;
    }

    #[fuchsia::test]
    async fn test_import_dictionary_probe_key_present() {
        assert!(import_with_probe_result(Ok(fruntime::CapabilityType::Data)).await.is_ok());
    }

    #[fuchsia::test]
    async fn test_import_dictionary_rejects_unregistered_handle() {
        assert!(matches!(
            import_with_probe_result(Err(
                fruntime::CapabilitiesError::HandleDoesNotReferenceCapability
            ))
            .await,
            Err(UtilsError::InvalidDictionary(
                fruntime::CapabilitiesError::HandleDoesNotReferenceCapability
            ))
        ));
    }

    #[fuchsia::test]
    async fn test_import_dictionary_rejects_non_dictionary() {
        assert!(matches!(
            import_with_probe_result(Err(fruntime::CapabilitiesError::InvalidCapabilityType)).await,
            Err(UtilsError::InvalidDictionary(fruntime::CapabilitiesError::InvalidCapabilityType))
        ));
    }

    #[fuchsia::test]
    async fn test_import_dictionary_requires_duplicate_right() {
        let (proxy, _stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy, unused_connector());
        let (dictionary, _dictionary_peer) = zx::EventPair::create();
        let dictionary = dictionary.replace_handle(zx::Rights::TRANSFER).unwrap();
        assert!(matches!(
            util.import_dictionary(dictionary).await,
            Err(UtilsError::ZxStatus(zx::Status::ACCESS_DENIED))
        ));
    }

    #[fuchsia::test]
    async fn test_copy_dictionary() {
        let (proxy, mut stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy.clone(), unused_connector());
        let (dictionary, dictionary_peer) = zx::EventPair::create();
        let dict = Dictionary { handle: dictionary, capabilities_proxy: proxy };

        let server = fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DictionaryCopy {
                    dictionary,
                    copy,
                    responder,
                    ..
                })) => {
                    assert_eq!(
                        dictionary.basic_info().unwrap().related_koid,
                        dictionary_peer.basic_info().unwrap().koid
                    );
                    responder.send(Ok(())).unwrap();
                    copy
                }
                other => panic!("expected DictionaryCopy, got {other:?}"),
            }
        });

        let copy = util.copy_dictionary(&dict).await.expect("Copy failed");
        let copy_peer = server.await;
        assert_eq!(copy.basic_info().unwrap().related_koid, copy_peer.basic_info().unwrap().koid);
    }

    #[fuchsia::test]
    async fn test_copy_dictionary_error() {
        let (proxy, mut stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy.clone(), unused_connector());
        let dict = Dictionary { handle: zx::EventPair::create().0, capabilities_proxy: proxy };

        let server = fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DictionaryCopy { responder, .. })) => {
                    responder.send(Err(fruntime::CapabilitiesError::InvalidCapabilityType)).unwrap()
                }
                other => panic!("expected DictionaryCopy, got {other:?}"),
            }
        });

        assert!(matches!(
            util.copy_dictionary(&dict).await,
            Err(UtilsError::Capabilities(fruntime::CapabilitiesError::InvalidCapabilityType))
        ));
        server.await;
    }

    /// Serves the requests that `dictionary_dir_connector_route` sends on the shared connection
    /// for an entry of type `capability_type`, and fails if anything else is sent on it.
    fn serve_main_connection(
        mut stream: fruntime::CapabilitiesRequestStream,
        capability_type: fruntime::CapabilityType,
    ) -> fuchsia_async::Task<()> {
        fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DictionaryGet {
                    key, responder, ..
                })) => {
                    assert_eq!(key, "service");
                    responder.send(Ok(capability_type)).unwrap();
                }
                other => panic!("expected DictionaryGet, got {other:?}"),
            }
            if let Some(request) = stream.next().await {
                panic!("unexpected request on the shared connection: {request:?}");
            }
        })
    }

    /// Serves the requests that `dictionary_dir_connector_route` sends on its dedicated routing
    /// connection, answering the route with `route_result`.
    fn serve_route_connection(
        mut stream: fruntime::CapabilitiesRequestStream,
        route_result: Result<fruntime::RouterResponse, i32>,
    ) -> fuchsia_async::Task<()> {
        fuchsia_async::Task::local(async move {
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::InstanceTokenCreate {
                    responder, ..
                })) => responder.send(Ok(())).unwrap(),
                other => panic!("expected InstanceTokenCreate, got {other:?}"),
            }
            match stream.next().await {
                Some(Ok(fruntime::CapabilitiesRequest::DirConnectorRouterRoute {
                    responder,
                    ..
                })) => responder.send(route_result).unwrap(),
                other => panic!("expected DirConnectorRouterRoute, got {other:?}"),
            }
        })
    }

    #[fuchsia::test]
    async fn test_dictionary_dir_connector_route() {
        let (proxy, stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let (route_proxy, route_stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy.clone(), connector_for(route_proxy));
        let main_server =
            serve_main_connection(stream, fruntime::CapabilityType::DirConnectorRouter);
        let route_server =
            serve_route_connection(route_stream, Ok(fruntime::RouterResponse::Success));

        let dict = Dictionary { handle: zx::EventPair::create().0, capabilities_proxy: proxy };
        let connector =
            util.dictionary_dir_connector_route(&dict, "service").await.expect("Route failed");
        route_server.await;

        // Dropping every user of the shared connection lets its server confirm that nothing else
        // was sent on it.
        drop((connector, dict, util));
        main_server.await;
    }

    #[fuchsia::test]
    async fn test_dictionary_dir_connector_route_unavailable() {
        let (proxy, stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let (route_proxy, route_stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy.clone(), connector_for(route_proxy));
        let main_server =
            serve_main_connection(stream, fruntime::CapabilityType::DirConnectorRouter);
        let route_server =
            serve_route_connection(route_stream, Ok(fruntime::RouterResponse::Unavailable));

        let dict = Dictionary { handle: zx::EventPair::create().0, capabilities_proxy: proxy };
        assert!(matches!(
            util.dictionary_dir_connector_route(&dict, "service").await,
            Err(UtilsError::Unavailable(name)) if name == "service"
        ));
        route_server.await;
        drop((dict, util));
        main_server.await;
    }

    #[fuchsia::test]
    async fn test_dictionary_dir_connector_route_wrong_type() {
        let (proxy, stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy.clone(), unused_connector());
        let main_server = serve_main_connection(stream, fruntime::CapabilityType::DirConnector);

        let dict = Dictionary { handle: zx::EventPair::create().0, capabilities_proxy: proxy };
        assert!(matches!(
            util.dictionary_dir_connector_route(&dict, "service").await,
            Err(UtilsError::UnexpectedCapabilityType(_, _))
        ));
        drop((dict, util));
        main_server.await;
    }

    #[fuchsia::test]
    async fn test_create_aggregate_dictionary() {
        let (proxy, mut stream) = create_proxy_and_stream::<fruntime::CapabilitiesMarker>();
        let util = DictionaryUtil::new_with_connector(proxy, unused_connector());

        let task = fuchsia_async::Task::local(async move {
            // dictionary_create
            if let Some(Ok(fruntime::CapabilitiesRequest::DictionaryCreate {
                dictionary: _,
                responder,
            })) = stream.next().await
            {
                responder.send(Ok(())).unwrap();
            } else {
                panic!("Expected DictionaryCreate");
            }

            // service_aggregate_create
            if let Some(Ok(fruntime::CapabilitiesRequest::ServiceAggregateCreate {
                dir_connector: _,
                sources: _,
                responder,
            })) = stream.next().await
            {
                responder.send(Ok(())).unwrap();
            } else {
                panic!("Expected ServiceAggregateCreate");
            }

            // dictionary_insert
            if let Some(Ok(fruntime::CapabilitiesRequest::DictionaryInsert {
                dictionary: _,
                key,
                value: _,
                responder,
            })) = stream.next().await
            {
                assert_eq!(key, "service");
                responder.send(Ok(())).unwrap();
            } else {
                panic!("Expected DictionaryInsert");
            }
        });

        let mut sources = HashMap::new();
        sources.insert(
            "service".to_string(),
            vec![fruntime::AggregateSource {
                dir_connector: Some(zx::EventPair::create().0),
                ..Default::default()
            }],
        );
        let _dict =
            util.create_aggregate_dictionary(sources).await.expect("Create aggregate failed");
        task.await;
    }
}
