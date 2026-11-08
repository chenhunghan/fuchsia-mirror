// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::accessor::{ArchiveAccessorServer, ArchiveAccessorTranslator, ArchiveAccessorWriter};
use crate::configs;
use crate::diagnostics::AccessorStats;
use crate::error::Error;
use crate::pipeline::StaticHierarchyAllowlist;
use fidl::endpoints::{DiscoverableProtocolMarker, ProtocolMarker, ServerEnd};
use fidl_fuchsia_diagnostics as fdiagnostics;
use fidl_fuchsia_diagnostics_host as fdiagnostics_host;
use fidl_fuchsia_io as fio;
use fuchsia_async as fasync;
use fuchsia_component::runtime::{Connector, ConnectorReceiver, Dictionary};
use fuchsia_fs::directory;
use fuchsia_inspect as inspect;
use fuchsia_sync::RwLock;
use futures::StreamExt;
use log::{debug, warn};
use moniker::ExtendedMoniker;
use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

const ALL_PIPELINE_NAME: &str = "all";
/// Pipeline that allows reading only logs (`fuchsia.diagnostics.ArchiveAccessor.logs`).
/// Because this pipeline exposes unredacted logs prior to redaction by Feedback,
/// routing this capability to any component other than `/core/feedback` on `user`
/// builds requires a Fuchsia Privacy review.
const LOGS_PIPELINE_NAME: &str = "logs";

struct PipelineParameters {
    has_config: bool,
    logs_enabled: bool,
    inspect_enabled: bool,
    name: Cow<'static, str>,
    empty_behavior: configs::EmptyBehavior,
}

/// Overlay that mediates connections between servers and the central
/// data repository. The overlay is provided static configurations that
/// make it unique to a specific pipeline, and uses those static configurations
/// to offer filtered access to the central repository.
pub struct Pipeline {
    /// The name of the pipeline.
    name: Cow<'static, str>,

    /// Contains information about the configuration of the pipeline.
    _pipeline_node: Option<inspect::Node>,

    /// Contains information about the accessor requests done for this pipeline.
    stats: AccessorStats,

    /// The statically declared allowlist for data exfiltration on this pipeline.
    static_allowlist: RwLock<StaticHierarchyAllowlist>,

    /// Whether this pipeline supports reading logs.
    logs_enabled: bool,

    /// Whether this pipeline supports reading inspect.
    inspect_enabled: bool,
}

impl Pipeline {
    fn new(
        parameters: PipelineParameters,
        pipelines_path: &Path,
        parent_node: &inspect::Node,
        accessor_stats_node: &inspect::Node,
    ) -> Self {
        let mut _pipeline_node = None;
        let path = format!("{}/{}", pipelines_path.display(), parameters.name);
        let mut static_selectors = if parameters.inspect_enabled { None } else { Some(vec![]) };
        if parameters.has_config {
            let node = parent_node.create_child(parameters.name.as_ref());
            let mut config =
                configs::PipelineConfig::from_directory(path, parameters.empty_behavior);
            config.record_to_inspect(&node);
            _pipeline_node = Some(node);
            if !config.disable_filtering {
                static_selectors = config.take_inspect_selectors();
            }
        }
        let stats = AccessorStats::new(accessor_stats_node.create_child(parameters.name.as_ref()));
        Pipeline {
            _pipeline_node,
            stats,
            name: parameters.name,
            static_allowlist: RwLock::new(StaticHierarchyAllowlist::new(static_selectors)),
            logs_enabled: parameters.logs_enabled,
            inspect_enabled: parameters.inspect_enabled,
        }
    }

    pub fn logs_enabled(&self) -> bool {
        self.logs_enabled
    }

    pub fn inspect_enabled(&self) -> bool {
        self.inspect_enabled
    }

    pub fn protocol_name(&self) -> Cow<'_, str> {
        self.protocol_name_inner::<fdiagnostics::ArchiveAccessorMarker>()
    }

    fn host_protocol_name(&self) -> Cow<'_, str> {
        self.protocol_name_inner::<fdiagnostics_host::ArchiveAccessorMarker>()
    }

    fn protocol_name_inner<P: DiscoverableProtocolMarker>(&self) -> Cow<'_, str> {
        if self.name.as_ref() == ALL_PIPELINE_NAME {
            Cow::Borrowed(P::PROTOCOL_NAME)
        } else {
            Cow::Owned(format!("{}.{}", P::PROTOCOL_NAME, self.name))
        }
    }

    pub fn accessor_stats(&self) -> &AccessorStats {
        &self.stats
    }

    pub fn remove_component(&self, moniker: &ExtendedMoniker) {
        self.static_allowlist.write().remove_component(moniker);
    }

    pub fn add_component(&self, moniker: &ExtendedMoniker) -> Result<(), Error> {
        self.static_allowlist.write().add_component(moniker.clone())
    }

    pub fn static_hierarchy_allowlist(&self) -> StaticHierarchyAllowlist {
        // TODO(https://fxbug.dev/42159044): can we avoid cloning here? This clone is not super expensive
        // as it'll be just cloning arcs, but we could be more efficient here.
        // Due to lock semantics we can't just return a reference at the moment as it leads to
        // an ABBA lock between inspect insertion into the repo and inspect reading.
        self.static_allowlist.read().clone()
    }
}

#[cfg(test)]
impl Pipeline {
    pub fn for_test(static_selectors: Option<Vec<fdiagnostics::Selector>>) -> Self {
        let has_config = static_selectors.is_some();
        Pipeline {
            _pipeline_node: None,
            name: Cow::Borrowed("test"),
            stats: AccessorStats::new(Default::default()),
            static_allowlist: RwLock::new(StaticHierarchyAllowlist::new(static_selectors)),
            logs_enabled: !has_config,
            inspect_enabled: true,
        }
    }

    pub fn logs_only_for_test() -> Self {
        Pipeline {
            _pipeline_node: None,
            name: Cow::Borrowed(LOGS_PIPELINE_NAME),
            stats: AccessorStats::new(Default::default()),
            static_allowlist: RwLock::new(StaticHierarchyAllowlist::new(Some(vec![]))),
            logs_enabled: true,
            inspect_enabled: false,
        }
    }
}

pub struct PipelineManager {
    pipelines: Vec<Arc<Pipeline>>,
    logs_pipeline: Arc<Pipeline>,
    _pipelines_node: inspect::Node,
    _accessor_stats_node: inspect::Node,
    scope: Option<fasync::Scope>,
}

impl PipelineManager {
    pub async fn new(
        pipelines_path: PathBuf,
        pipelines_node: inspect::Node,
        accessor_stats_node: inspect::Node,
        scope: fasync::Scope,
    ) -> Self {
        let mut pipelines = vec![];
        if let Ok(dir) =
            directory::open_in_namespace(pipelines_path.to_str().unwrap(), fio::PERM_READABLE)
        {
            for entry in directory::readdir(&dir).await.expect("read dir") {
                if !matches!(entry.kind, directory::DirentKind::Directory) {
                    continue;
                }
                if entry.name == ALL_PIPELINE_NAME || entry.name == LOGS_PIPELINE_NAME {
                    warn!("Ignoring reserved pipeline configuration directory: {}", entry.name);
                    continue;
                }
                let empty_behavior = if entry.name == "feedback" || entry.name == "previous_boot" {
                    configs::EmptyBehavior::DoNotFilter
                } else {
                    configs::EmptyBehavior::Disable
                };
                let parameters = PipelineParameters {
                    has_config: true,
                    logs_enabled: false,
                    inspect_enabled: true,
                    name: Cow::Owned(entry.name),
                    empty_behavior,
                };
                pipelines.push(Arc::new(Pipeline::new(
                    parameters,
                    &pipelines_path,
                    &pipelines_node,
                    &accessor_stats_node,
                )));
            }
        }
        let logs_pipeline = Arc::new(Pipeline::new(
            PipelineParameters {
                has_config: false,
                logs_enabled: true,
                inspect_enabled: false,
                name: Cow::Borrowed(LOGS_PIPELINE_NAME),
                empty_behavior: configs::EmptyBehavior::Disable,
            },
            &pipelines_path,
            &pipelines_node,
            &accessor_stats_node,
        ));
        pipelines.push(Arc::new(Pipeline::new(
            PipelineParameters {
                has_config: false,
                logs_enabled: true,
                inspect_enabled: true,
                name: Cow::Borrowed(ALL_PIPELINE_NAME),
                empty_behavior: configs::EmptyBehavior::Disable,
            },
            &pipelines_path,
            &pipelines_node,
            &accessor_stats_node,
        )));
        Self {
            pipelines,
            logs_pipeline,
            _pipelines_node: pipelines_node,
            _accessor_stats_node: accessor_stats_node,
            scope: Some(scope),
        }
    }

    pub fn logs_pipeline(&self) -> Arc<Pipeline> {
        Arc::clone(&self.logs_pipeline)
    }

    pub fn weak_pipelines(&self) -> Vec<Weak<Pipeline>> {
        self.pipelines
            .iter()
            .filter(|p| p.inspect_enabled())
            .map(Arc::downgrade)
            .collect::<Vec<_>>()
    }

    pub async fn cancel(&mut self) {
        if let Some(scope) = self.scope.take() {
            scope.cancel().await;
        }
    }

    pub async fn serve_pipelines(&self, accessor_server: Arc<ArchiveAccessorServer>) -> Dictionary {
        let accessors_dict = Dictionary::new().await;
        debug!("Will serve {} pipelines", self.pipelines.len());
        for pipeline in &self.pipelines {
            debug!("Installing spawning receivers for {}", pipeline.name);
            let accessor_pipeline = Arc::clone(pipeline);
            // Unwrap: safe, we must not cancel before serving.
            self.scope.as_ref().unwrap().spawn(handle_receiver_requests::<
                fdiagnostics::ArchiveAccessorMarker,
            >(
                get_receiver_stream(pipeline.protocol_name(), &accessors_dict).await,
                Arc::clone(&accessor_server),
                Arc::clone(&accessor_pipeline),
            ));
            // Unwrap: safe, we must not cancel before serving.
            self.scope.as_ref().unwrap().spawn(handle_receiver_requests::<
                fdiagnostics_host::ArchiveAccessorMarker,
            >(
                get_receiver_stream(pipeline.host_protocol_name(), &accessors_dict).await,
                Arc::clone(&accessor_server),
                Arc::clone(&accessor_pipeline),
            ));
        }

        accessors_dict
    }
}

async fn get_receiver_stream(
    protocol_name: Cow<'_, str>,
    accessors_dict: &Dictionary,
) -> ConnectorReceiver {
    let (connector, receiver) = Connector::new().await;
    debug!("Added {protocol_name} to the accessors dictionary.");
    accessors_dict.insert(&protocol_name, connector).await;
    receiver
}

async fn handle_receiver_requests<P>(
    mut receiver_stream: ConnectorReceiver,
    accessor_server: Arc<ArchiveAccessorServer>,
    pipeline: Arc<Pipeline>,
) where
    P: ProtocolMarker,
    P::RequestStream: ArchiveAccessorTranslator + Send + 'static,
    <P::RequestStream as ArchiveAccessorTranslator>::InnerDataRequestChannel:
        ArchiveAccessorWriter + Send,
{
    while let Some(channel) = receiver_stream.next().await {
        debug!("Handling receive request for: {} -> {}", pipeline.name, P::DEBUG_NAME);
        let server_end = ServerEnd::<P>::new(channel);
        accessor_server
            .spawn_server::<P::RequestStream>(Arc::clone(&pipeline), server_end.into_stream());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[fuchsia::test]
    async fn pipeline_manager_ignores_reserved_pipeline_names() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(ALL_PIPELINE_NAME)).unwrap();
        fs::create_dir(dir.path().join(LOGS_PIPELINE_NAME)).unwrap();
        fs::create_dir(dir.path().join("feedback")).unwrap();

        let inspector = inspect::Inspector::default();
        let manager = PipelineManager::new(
            dir.path().to_path_buf(),
            inspector.root().create_child("pipelines"),
            inspector.root().create_child("stats"),
            fasync::Scope::new_with_name("test"),
        )
        .await;

        assert_eq!(manager.pipelines.len(), 2);
        let feedback = manager.pipelines.iter().find(|p| p.name == "feedback").unwrap();
        assert!(!feedback.logs_enabled());
        assert!(feedback.inspect_enabled());

        assert!(manager.pipelines.iter().all(|p| p.name != LOGS_PIPELINE_NAME));
        let logs_pipeline = manager.logs_pipeline();
        assert_eq!(logs_pipeline.protocol_name(), "fuchsia.diagnostics.ArchiveAccessor.logs");
        assert!(logs_pipeline.logs_enabled());
        assert!(!logs_pipeline.inspect_enabled());

        let all_pipelines: Vec<_> =
            manager.pipelines.iter().filter(|p| p.name == ALL_PIPELINE_NAME).collect();
        assert_eq!(all_pipelines.len(), 1);
        assert!(all_pipelines[0].logs_enabled());
        assert!(all_pipelines[0].inspect_enabled());
    }
}
