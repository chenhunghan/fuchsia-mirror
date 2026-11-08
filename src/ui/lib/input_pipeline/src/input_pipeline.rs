// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device_listener_registry::DeviceListenerRegistry;
use crate::display_ownership::{DisplayOwnership, OwnershipState};
use crate::focus_listener::FocusListener;
use crate::input_device::{InputEventType, InputPipelineFeatureFlags};
use crate::input_handler::Handler;
use crate::{Dispatcher, Incoming, Transport, input_device, input_handler, metrics};
use anyhow::{Context, Error, format_err};
use fidl::endpoints::Proxy;
use focus_chain_provider::FocusChainProviderPublisher;

use fuchsia_inspect::NumericProperty;
use fuchsia_inspect::health::Reporter;
use fuchsia_sync::Mutex;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::future::LocalBoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use itertools::Itertools;
use metrics_registry::*;
use sorted_vec_map::{SortedVecMap, SortedVecSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, LazyLock};
use strum::EnumCount;

/// Use a self incremental u32 unique id for device_id.
///
/// device id start from 10 to avoid conflict with default devices in Starnix.
/// Currently, Starnix using 0 and 1 as default devices' id. Starnix need to
/// use default devices to deliver events from physical devices until we have
/// API to expose device changes to UI clients.
static NEXT_DEVICE_ID: LazyLock<AtomicU32> = LazyLock::new(|| AtomicU32::new(10));

/// Each time this function is invoked, it returns the current value of its
/// internal counter (serving as a unique id for device_id) and then increments
/// that counter in preparation for the next call.
fn get_next_device_id() -> u32 {
    NEXT_DEVICE_ID.fetch_add(1, Ordering::SeqCst)
}

type BoxedInputDeviceBinding = Box<dyn input_device::InputDeviceBinding>;

pub struct InputDeviceBindingAndTask {
    pub binding: BoxedInputDeviceBinding,
    pub task: crate::dispatcher::TaskHandle<()>,
    pub instance_name: String,
    pub device_type: input_device::InputDeviceType,
    pub device_proxy: fidl_next::Client<
        fidl_next_fuchsia_input_report::InputDevice,
        crate::dispatcher::Transport,
    >,
    pub is_injected: bool,
}

/// An [`InputDeviceBindingMap`] maps an input device to one or more InputDeviceBindings and their tasks.
/// It uses unique device id as key.
pub type InputDeviceBindingMap = Arc<Mutex<SortedVecMap<u32, Vec<InputDeviceBindingAndTask>>>>;

/// Tracks bound device types per service instance filename.
pub type BoundInstances =
    Arc<Mutex<SortedVecMap<String, SortedVecSet<input_device::InputDeviceType>>>>;

pub type DeviceJoinHandle = fidl_next::HandlerJoinHandle<crate::dispatcher::Transport, ()>;

pub struct InflightDevice {
    pub device_id: u32,
    pub device_client: fidl_next::Client<
        fidl_next_fuchsia_input_report::InputDevice,
        crate::dispatcher::Transport,
    >,
    pub join_handle: Arc<Mutex<Option<DeviceJoinHandle>>>,
    pub active_count: usize,
}

/// Tracks in-flight device connections (device ID, client proxy, join handle, and active count) per service instance filename.
pub type InflightDeviceMap = Arc<Mutex<SortedVecMap<String, InflightDevice>>>;

struct InflightGuard {
    inflight_devices: InflightDeviceMap,
    filename: String,
    active: bool,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        if self.active {
            let mut inflight = self.inflight_devices.lock();
            if let Some(entry) = inflight.get_mut(&self.filename) {
                if entry.active_count > 1 {
                    entry.active_count -= 1;
                } else {
                    inflight.remove(&self.filename);
                }
            }
        }
    }
}

/// An input pipeline assembly.
///
/// Represents a partial stage of the input pipeline which accepts inputs through an asynchronous
/// sender channel, and emits outputs through an asynchronous receiver channel.  Use [new] to
/// create a new assembly.  Use [add_handler], or [add_all_handlers] to add the input pipeline
/// handlers to use.  When done, [InputPipeline::new] can be used to make a new input pipeline.
///
/// # Implementation notes
///
/// Internally, when a new [InputPipelineAssembly] is created with multiple [InputHandler]s, the
/// handlers are connected together using async queues.  This allows fully streamed processing of
/// input events, and also allows some pipeline stages to generate events spontaneously, i.e.
/// without an external stimulus.
pub struct InputPipelineAssembly {
    /// The top-level sender: send into this queue to inject an event into the input
    /// pipeline.
    sender: UnboundedSender<Vec<input_device::InputEvent>>,
    /// The bottom-level receiver: any events that fall through the entire pipeline can
    /// be read from this receiver.
    receiver: UnboundedReceiver<Vec<input_device::InputEvent>>,

    /// The input handlers that comprise the input pipeline.
    handlers: Vec<Rc<dyn input_handler::BatchInputHandler>>,

    /// The display ownership watcher task.
    display_ownership_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The focus listener task.
    focus_listener_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The metrics logger.
    metrics_logger: metrics::MetricsLogger,

    /// The display ownership handler, if configured.
    display_ownership: Option<Rc<DisplayOwnership>>,
}

impl InputPipelineAssembly {
    /// Create a new but empty [InputPipelineAssembly]. Use [add_handler] or similar
    /// to add new handlers to it.
    pub fn new(metrics_logger: metrics::MetricsLogger) -> Self {
        let (sender, receiver) = mpsc::unbounded();
        InputPipelineAssembly {
            sender,
            receiver,
            handlers: vec![],
            metrics_logger,
            display_ownership_fut: None,
            focus_listener_fut: None,
            display_ownership: None,
        }
    }

    /// Adds another [input_handler::BatchInputHandler] into the [InputPipelineAssembly]. The handlers
    /// are invoked in the order they are added. Returns `Self` for chaining.
    pub fn add_handler(mut self, handler: Rc<dyn input_handler::BatchInputHandler>) -> Self {
        self.handlers.push(handler);
        self
    }

    /// Adds all handlers into the assembly in the order they appear in `handlers`.
    pub fn add_all_handlers(self, handlers: Vec<Rc<dyn input_handler::BatchInputHandler>>) -> Self {
        handlers.into_iter().fold(self, |assembly, handler| assembly.add_handler(handler))
    }

    pub fn add_display_ownership(
        self,
        display_ownership_event: zx::Event,
        input_handlers_node: &fuchsia_inspect::Node,
    ) -> InputPipelineAssembly {
        let h = DisplayOwnership::new(
            display_ownership_event,
            input_handlers_node,
            self.metrics_logger.clone(),
        );
        self.add_display_ownership_handler(h)
    }

    pub fn add_display_ownership_handler(
        mut self,
        display_ownership: Rc<DisplayOwnership>,
    ) -> InputPipelineAssembly {
        let metrics_logger_clone = self.metrics_logger.clone();
        let h_clone = display_ownership.clone();
        let sender_clone = self.sender.clone();
        let display_ownership_fut = Box::pin(async move {
            h_clone.clone().set_handler_healthy();
            h_clone.clone()
                .handle_ownership_change(sender_clone)
                .await
                .map_err(|e| {
                    metrics_logger_clone.log_error(
                        InputPipelineErrorMetricDimensionEvent::InputPipelineDisplayOwnershipIsNotSupposedToTerminate,
                        std::format!(
                            "display ownership is not supposed to terminate - this is likely a problem: {:?}", e));
                        })
                        .unwrap();
            h_clone.set_handler_unhealthy("Receive loop terminated for handler: DisplayOwnership");
        });
        self.display_ownership_fut = Some(display_ownership_fut);
        self.display_ownership = Some(display_ownership.clone());
        self.add_handler(display_ownership)
    }

    /// Deconstructs the assembly into constituent components, used when constructing
    /// [InputPipeline].
    ///
    /// You should call [catch_unhandled] on the returned [async_channel::Receiver], and
    /// [run] on the returned [fuchsia_async::Tasks] (or supply own equivalents).
    fn into_components(
        self,
    ) -> (
        UnboundedSender<Vec<input_device::InputEvent>>,
        UnboundedReceiver<Vec<input_device::InputEvent>>,
        Vec<Rc<dyn input_handler::BatchInputHandler>>,
        metrics::MetricsLogger,
        Option<LocalBoxFuture<'static, ()>>,
        Option<LocalBoxFuture<'static, ()>>,
        Option<Rc<DisplayOwnership>>,
    ) {
        (
            self.sender,
            self.receiver,
            self.handlers,
            self.metrics_logger,
            self.display_ownership_fut,
            self.focus_listener_fut,
            self.display_ownership,
        )
    }

    pub fn add_focus_listener(
        mut self,
        incoming: &Incoming,
        focus_chain_publisher: FocusChainProviderPublisher,
    ) -> Self {
        let metrics_logger_clone = self.metrics_logger.clone();
        let incoming2 = incoming.clone();
        let focus_listener_fut = Box::pin(async move {
            if let Ok(mut focus_listener) = FocusListener::new(
                &incoming2,
                focus_chain_publisher,
                metrics_logger_clone,
            )
            .map_err(|e| {
                log::warn!("could not create focus listener, focus will not be dispatched: {:?}", e)
            }) {
                // This will await indefinitely and process focus messages in a loop, unless there
                // is a problem.
                let _result = focus_listener
                    .dispatch_focus_changes()
                    .await
                    .map(|_| {
                        log::warn!("dispatch focus loop ended, focus will no longer be dispatched")
                    })
                    .map_err(|e| {
                        panic!("could not dispatch focus changes, this is a fatal error: {:?}", e)
                    });
            }
        });
        self.focus_listener_fut = Some(focus_listener_fut);
        self
    }
}

/// An [`InputPipeline`] manages input devices and propagates input events through input handlers.
///
/// On creation, clients declare what types of input devices an [`InputPipeline`] manages. The
/// [`InputPipeline`] will continuously detect new input devices of supported type(s).
///
/// # Example
/// ```
/// let ime_handler =
///     ImeHandler::new(scene_manager.session.clone(), scene_manager.compositor_id).await?;
/// let touch_handler = TouchHandler::new(
///     scene_manager.session.clone(),
///     scene_manager.compositor_id,
///     scene_manager.display_size
/// ).await?;
///
/// let assembly = InputPipelineAssembly::new()
///     .add_handler(Box::new(ime_handler)),
///     .add_handler(Box::new(touch_handler)),
/// let input_pipeline = InputPipeline::new(
///     vec![
///         input_device::InputDeviceType::Touch,
///         input_device::InputDeviceType::Keyboard,
///     ],
///     assembly,
/// );
/// input_pipeline.handle_input_events().await;
/// ```
pub struct InputPipeline {
    /// The entry point into the input handler pipeline. Incoming input events should
    /// be inserted into this async queue, and the input pipeline will ensure that they
    /// are propagated through all the input handlers in the appropriate sequence.
    pipeline_sender: UnboundedSender<Vec<input_device::InputEvent>>,

    /// A clone of this sender is given to every InputDeviceBinding that this pipeline owns.
    /// Each InputDeviceBinding will send InputEvents to the pipeline through this channel.
    device_event_sender: UnboundedSender<Vec<input_device::InputEvent>>,

    /// Receives InputEvents from all InputDeviceBindings that this pipeline owns.
    device_event_receiver: UnboundedReceiver<Vec<input_device::InputEvent>>,

    /// The types of devices this pipeline supports.
    input_device_types: Vec<input_device::InputDeviceType>,

    /// The InputDeviceBindings bound to this pipeline.
    input_device_bindings: InputDeviceBindingMap,

    /// This node is bound to the lifetime of this InputPipeline.
    /// Inspect data will be dumped for this pipeline as long as it exists.
    inspect_node: fuchsia_inspect::Node,

    /// The metrics logger.
    metrics_logger: metrics::MetricsLogger,

    /// The feature flags for the input pipeline.
    pub feature_flags: input_device::InputPipelineFeatureFlags,
    /// The display ownership future, if configured.
    display_ownership_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The focus listener future, if configured.
    focus_listener_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The watcher future.
    watcher_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The runner future.
    runner_fut: Option<LocalBoxFuture<'static, ()>>,

    /// The registry for device listeners.
    pub device_listener_registry: DeviceListenerRegistry,

    /// The display ownership handler, if configured.
    pub display_ownership: Option<Rc<DisplayOwnership>>,
}

impl InputPipeline {
    /// Gets the display ownership handler, if configured.
    pub fn display_ownership(&self) -> Option<&Rc<DisplayOwnership>> {
        self.display_ownership.as_ref()
    }

    fn new_common(
        input_device_types: Vec<input_device::InputDeviceType>,
        assembly: InputPipelineAssembly,
        inspect_node: fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
    ) -> Self {
        let (
            pipeline_sender,
            receiver,
            handlers,
            metrics_logger,
            display_ownership_fut,
            focus_listener_fut,
            display_ownership,
        ) = assembly.into_components();

        let mut handlers_count = handlers.len();
        let display_ownership_fut = if let Some(fut) = display_ownership_fut {
            handlers_count += 1;
            Some(
                async move {
                    fut.await;
                    panic!("display_ownership_fut exited unexpectedly, which compromises device state tracking. Terminating to avoid inconsistent state.");
                }
                .boxed_local(),
            )
        } else {
            None
        };

        let focus_listener_fut = if let Some(fut) = focus_listener_fut {
            handlers_count += 1;
            Some(
                async move {
                    fut.await;
                    panic!("focus_listener_fut exited unexpectedly, which breaks input routing. Terminating to avoid inconsistent state.");
                }
                .boxed_local(),
            )
        } else {
            None
        };

        // Add properties to inspect node
        inspect_node.record_string("supported_input_devices", input_device_types.iter().join(", "));
        inspect_node.record_uint("handlers_registered", handlers_count as u64);
        inspect_node.record_uint("handlers_healthy", handlers_count as u64);

        // Initializes all handlers and starts the input pipeline loop.
        let runner_fut = Some(InputPipeline::run(receiver, handlers, metrics_logger.clone()));

        let (device_event_sender, device_event_receiver) = futures::channel::mpsc::unbounded();
        let input_device_bindings: InputDeviceBindingMap =
            Arc::new(Mutex::new(SortedVecMap::new()));
        let device_listener_registry = DeviceListenerRegistry::new();
        InputPipeline {
            pipeline_sender,
            device_event_sender,
            device_event_receiver,
            input_device_types,
            input_device_bindings,
            inspect_node,
            metrics_logger,
            feature_flags,
            display_ownership_fut,
            focus_listener_fut,
            watcher_fut: None,
            runner_fut,
            device_listener_registry,
            display_ownership,
        }
    }

    /// Creates a new [`InputPipeline`] for integration testing.
    /// Unlike a production input pipeline, this pipeline will not monitor
    /// `/svc/fuchsia.input.report.Service` for devices.
    ///
    /// # Parameters
    /// - `input_device_types`: The types of devices the new [`InputPipeline`] will support.
    /// - `assembly`: The input handlers that the [`InputPipeline`] sends InputEvents to.
    pub fn new_for_test(
        input_device_types: Vec<input_device::InputDeviceType>,
        assembly: InputPipelineAssembly,
    ) -> Self {
        let inspector = fuchsia_inspect::Inspector::default();
        let root = inspector.root();
        let test_node = root.create_child("input_pipeline");
        Self::new_common(
            input_device_types,
            assembly,
            test_node,
            input_device::InputPipelineFeatureFlags { enable_merge_touch_events: false },
        )
    }
}

#[derive(Clone)]
enum DeviceServiceSource {
    Incoming(Incoming),
    Service(fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>),
}

impl DeviceServiceSource {
    fn open_service(
        &self,
    ) -> Result<fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>, Error>
    {
        match self {
            Self::Incoming(incoming) => incoming
                .open_service(fidl_fuchsia_input_report::ServiceMarker)
                .context("failed to open service"),
            Self::Service(service) => Ok(service.clone()),
        }
    }
}

impl InputPipeline {
    /// Creates a new [`InputPipeline`] for production use.
    ///
    /// # Parameters
    /// - `input_device_types`: The types of devices the new [`InputPipeline`] will support.
    /// - `assembly`: The input handlers that the [`InputPipeline`] sends InputEvents to.
    /// - `inspect_node`: The root node for InputPipeline's Inspect tree
    pub fn new(
        incoming: &Incoming,
        input_device_types: Vec<input_device::InputDeviceType>,
        assembly: InputPipelineAssembly,
        inspect_node: fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Self, Error> {
        Self::new_internal(
            DeviceServiceSource::Incoming(incoming.clone()),
            input_device_types,
            assembly,
            inspect_node,
            feature_flags,
            metrics_logger,
        )
    }

    /// Creates a new [`InputPipeline`] with an explicitly provided [`Service`].
    /// Useful for testing with mock service directories.
    pub fn new_with_service(
        service: fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>,
        input_device_types: Vec<input_device::InputDeviceType>,
        assembly: InputPipelineAssembly,
        inspect_node: fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Self, Error> {
        Self::new_internal(
            DeviceServiceSource::Service(service),
            input_device_types,
            assembly,
            inspect_node,
            feature_flags,
            metrics_logger,
        )
    }

    fn new_internal(
        service_source: DeviceServiceSource,
        input_device_types: Vec<input_device::InputDeviceType>,
        assembly: InputPipelineAssembly,
        inspect_node: fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
    ) -> Result<Self, Error> {
        let mut input_pipeline =
            Self::new_common(input_device_types, assembly, inspect_node, feature_flags);
        let input_device_types = input_pipeline.input_device_types.clone();
        let input_event_sender = input_pipeline.device_event_sender.clone();
        let input_device_bindings = input_pipeline.input_device_bindings.clone();
        let bound_instances: BoundInstances = Arc::new(Mutex::new(SortedVecMap::new()));
        let inflight_devices: InflightDeviceMap = Arc::new(Mutex::new(SortedVecMap::new()));
        let device_listener_registry = input_pipeline.device_listener_registry.clone();
        let devices_node = input_pipeline.inspect_node.create_child("input_devices");
        let devices_node_weak = devices_node.clone_weak();
        input_pipeline.inspect_node.record(devices_node);
        let feature_flags = input_pipeline.feature_flags.clone();

        let devices_inspect: Arc<
            Mutex<Option<(Arc<fuchsia_inspect::UintProperty>, Arc<fuchsia_inspect::UintProperty>)>>,
        > = Arc::new(Mutex::new(None));

        let display_ownership = input_pipeline.display_ownership.clone();

        if let Some(display_ownership) = &display_ownership {
            let bindings_clone = input_device_bindings.clone();
            let bound_instances_clone = bound_instances.clone();
            let inflight_devices_clone = inflight_devices.clone();
            let service_source_for_cb = service_source.clone();
            let input_device_types_clone = input_device_types.clone();
            let input_event_sender_clone = input_event_sender.clone();
            let display_ownership_weak = Rc::downgrade(display_ownership);
            let devices_node_weak_clone = devices_node_weak.clone_weak();
            let devices_inspect_clone = devices_inspect.clone();
            let feature_flags_clone = feature_flags.clone();
            let metrics_logger_clone = metrics_logger.clone();
            let device_listener_registry_clone = device_listener_registry.clone();

            display_ownership.set_ownership_change_callback(
                move |old_state: OwnershipState, new_state: OwnershipState| {
                    let bindings = bindings_clone.clone();
                    let bound_instances = bound_instances_clone.clone();
                    let inflight_devices = inflight_devices_clone.clone();
                    let service_source = service_source_for_cb.clone();
                    let input_device_types = input_device_types_clone.clone();
                    let input_event_sender = input_event_sender_clone.clone();
                    let display_ownership_weak = display_ownership_weak.clone();
                    let devices_node_weak = devices_node_weak_clone.clone_weak();
                    let devices_inspect = devices_inspect_clone.clone();
                    let feature_flags = feature_flags_clone.clone();
                    let metrics_logger = metrics_logger_clone.clone();
                    let device_listener_registry = device_listener_registry_clone.clone();

                    async move {
                        match new_state {
                            OwnershipState::DisplayUnowned => {
                                log::info!(
                                    "Display ownership lost: pausing device reader tasks and clearing bindings"
                                );
                                let dropped_tasks: Vec<_> = {
                                    let mut bindings_guard = bindings.lock();
                                    let old_bindings = std::mem::take(&mut *bindings_guard);
                                    let mut new_bindings = SortedVecMap::new();
                                    let mut retained_instances: SortedVecMap<
                                        String,
                                        SortedVecSet<input_device::InputDeviceType>,
                                    > = SortedVecMap::new();
                                    let mut dropped_tasks = Vec::new();

                                    for (device_id, binding_and_tasks) in old_bindings {
                                        let (kept, dropped): (Vec<_>, Vec<_>) =
                                            binding_and_tasks.into_iter().partition(|bt| {
                                                bt.is_injected
                                            });
                                        dropped_tasks.extend(dropped);
                                        if !kept.is_empty() {
                                            for bt in &kept {
                                                if let Some(set) =
                                                    retained_instances.get_mut(&bt.instance_name)
                                                {
                                                    set.insert(bt.device_type);
                                                } else {
                                                    let mut set = SortedVecSet::new();
                                                    set.insert(bt.device_type);
                                                    retained_instances
                                                        .insert(bt.instance_name.clone(), set);
                                                }
                                            }
                                            new_bindings.insert(device_id, kept);
                                        }
                                    }
                                    *bindings_guard = new_bindings;
                                    *bound_instances.lock() = retained_instances;
                                    *inflight_devices.lock() = SortedVecMap::new();
                                    dropped_tasks
                                };
                                std::mem::drop(dropped_tasks);
                            }
                            OwnershipState::ClientInputOwned => {
                                log::info!(
                                    "Client input ownership gained: retaining button reader tasks"
                                );
                                let mut dropped_tasks = Vec::new();
                                {
                                    let mut bindings_guard = bindings.lock();
                                    let old_bindings = std::mem::take(&mut *bindings_guard);
                                    let mut new_bindings = SortedVecMap::new();
                                    let mut retained_instances: SortedVecMap<
                                        String,
                                        SortedVecSet<input_device::InputDeviceType>,
                                    > = SortedVecMap::new();

                                    for (device_id, binding_and_tasks) in old_bindings {
                                        let (kept, dropped): (Vec<_>, Vec<_>) =
                                            binding_and_tasks.into_iter().partition(|bt| {
                                                bt.is_injected
                                                    || matches!(
                                                        bt.binding.get_device_descriptor(),
                                                        input_device::InputDeviceDescriptor::ConsumerControls(_)
                                                    )
                                            });
                                        dropped_tasks.extend(dropped);
                                        if !kept.is_empty() {
                                            for bt in &kept {
                                                if let Some(set) =
                                                    retained_instances.get_mut(&bt.instance_name)
                                                {
                                                    set.insert(bt.device_type);
                                                } else {
                                                    let mut set = SortedVecSet::new();
                                                    set.insert(bt.device_type);
                                                    retained_instances
                                                        .insert(bt.instance_name.clone(), set);
                                                }
                                            }
                                            new_bindings.insert(device_id, kept);
                                        }
                                    }
                                    *bindings_guard = new_bindings;

                                    // Preserve in-flight ConsumerControls reservations for devices currently in inflight_devices.
                                    // Retaining in-flight ConsumerControls prevents re_enumerate_devices from binding duplicate
                                    // readers while still allowing non-ConsumerControls capabilities to be bound later if ownership
                                    // transitions back to PlatformOwned.
                                    let old_bound_instances = bound_instances.lock();
                                    let inflight_guard = inflight_devices.lock();
                                    for (filename, _) in inflight_guard.iter() {
                                        if let Some(types) = old_bound_instances.get(filename) {
                                            if types.contains(&input_device::InputDeviceType::ConsumerControls) {
                                                if let Some(set) = retained_instances.get_mut(filename) {
                                                    set.insert(input_device::InputDeviceType::ConsumerControls);
                                                } else {
                                                    let mut set = SortedVecSet::new();
                                                    set.insert(input_device::InputDeviceType::ConsumerControls);
                                                    retained_instances.insert(filename.clone(), set);
                                                }
                                            }
                                        }
                                    }
                                    std::mem::drop(inflight_guard);
                                    std::mem::drop(old_bound_instances);

                                    *bound_instances.lock() = retained_instances;
                                }
                                std::mem::drop(dropped_tasks);

                                // Re-enumerate to connect and bind ConsumerControls devices ONLY if transitioning
                                // from DisplayUnowned where all bindings were previously dropped.
                                if old_state == OwnershipState::DisplayUnowned {
                                    match service_source.open_service() {
                                        Ok(service) => {
                                            let (discovered_prop, connected_prop) = {
                                                let guard = devices_inspect.lock();
                                                (
                                                    guard.as_ref().map(|(d, _)| d.clone()),
                                                    guard.as_ref().map(|(_, c)| c.clone()),
                                                )
                                            };
                                            let display_ownership =
                                                display_ownership_weak.upgrade();
                                            Self::re_enumerate_devices(
                                                service,
                                                &input_device_types,
                                                &input_event_sender,
                                                &bindings,
                                                &bound_instances,
                                                &inflight_devices,
                                                display_ownership.as_ref(),
                                                &devices_node_weak,
                                                discovered_prop.as_deref(),
                                                connected_prop.as_deref(),
                                                feature_flags,
                                                metrics_logger,
                                                device_listener_registry,
                                            )
                                            .await;
                                        }
                                        Err(e) => {
                                            metrics_logger.log_warn(
                                                InputPipelineErrorMetricDimensionEvent::InputPipelineUnableToWatchForNewInputDevices,
                                                std::format!("Failed to open service on client input ownership: {:?}", e),
                                            );
                                        }
                                    }
                                }
                            }
                            OwnershipState::PlatformOwned => {
                                log::info!(
                                    "Input ownership restored: resuming device discovery and reading"
                                );
                            match service_source.open_service() {
                                Ok(service) => {
                                    let (discovered_prop, connected_prop) = {
                                        let guard = devices_inspect.lock();
                                        (
                                            guard.as_ref().map(|(d, _)| d.clone()),
                                            guard.as_ref().map(|(_, c)| c.clone()),
                                        )
                                    };
                                    let display_ownership = display_ownership_weak.upgrade();
                                    Self::re_enumerate_devices(
                                        service,
                                        &input_device_types,
                                        &input_event_sender,
                                        &bindings,
                                        &bound_instances,
                                        &inflight_devices,
                                        display_ownership.as_ref(),
                                        &devices_node_weak,
                                        discovered_prop.as_deref(),
                                        connected_prop.as_deref(),
                                        feature_flags,
                                        metrics_logger,
                                        device_listener_registry,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    metrics_logger.log_warn(
                                        InputPipelineErrorMetricDimensionEvent::InputPipelineUnableToWatchForNewInputDevices,
                                        std::format!("Failed to open service on ownership restore: {:?}", e),
                                    );
                                }
                            }
                        }
                    }
                }
                .boxed_local()
            });
        }

        let watcher_fut = async move {
            let devices_discovered =
                Arc::new(devices_node_weak.create_uint("devices_discovered", 0));
            let devices_connected =
                Arc::new(devices_node_weak.create_uint("devices_connected", 0));
            *devices_inspect.lock() =
                Some((devices_discovered.clone(), devices_connected.clone()));

            match service_source.open_service() {
                Ok(service) => {
                    match Self::watch_for_devices_internal(
                        service,
                        input_device_types,
                        input_event_sender,
                        input_device_bindings,
                        bound_instances,
                        inflight_devices,
                        display_ownership,
                        &devices_node_weak,
                        &devices_discovered,
                        &devices_connected,
                        false, /* break_on_idle */
                        feature_flags,
                        metrics_logger.clone(),
                        device_listener_registry,
                    )
                    .await
                    .context("failed to watch for devices")
                    {
                        Ok(()) => {}
                        Err(err) => {
                            metrics_logger.log_warn(
                                InputPipelineErrorMetricDimensionEvent::InputPipelineUnableToWatchForNewInputDevices,
                                std::format!(
                                    "Input pipeline is unable to watch for new input devices: {:?}",
                                    err
                                ));
                        }
                    }
                }
                Err(err) => {
                    metrics_logger.log_warn(
                        InputPipelineErrorMetricDimensionEvent::InputPipelineUnableToWatchForNewInputDevices,
                        std::format!(
                            "Input pipeline is unable to open service for watching new input devices: {:?}",
                            err
                        ));
                }
            }
        }
        .boxed_local();

        input_pipeline.watcher_fut = Some(watcher_fut);

        Ok(input_pipeline)
    }

    /// Gets the input device bindings.
    pub fn input_device_bindings(&self) -> &InputDeviceBindingMap {
        &self.input_device_bindings
    }

    /// Gets the input device sender: this is the channel that should be cloned
    /// and used for injecting events from the drivers into the input pipeline.
    pub fn input_event_sender(&self) -> &UnboundedSender<Vec<input_device::InputEvent>> {
        &self.device_event_sender
    }

    /// Gets a list of input device types supported by this input pipeline.
    pub fn input_device_types(&self) -> &[input_device::InputDeviceType] {
        &self.input_device_types
    }

    /// Gets the device listener registry.
    pub fn device_listener_registry(&self) -> &DeviceListenerRegistry {
        &self.device_listener_registry
    }

    /// Forwards all input events into the input pipeline.
    pub async fn handle_input_events(self) {
        let metrics_logger_clone = self.metrics_logger.clone();
        let mut device_event_receiver = self.device_event_receiver;
        let pipeline_sender = self.pipeline_sender.clone();

        let forwarder = async move {
            while let Some(input_event) = device_event_receiver.next().await {
                if let Err(e) = pipeline_sender.unbounded_send(input_event) {
                    metrics_logger_clone.log_error(
                        InputPipelineErrorMetricDimensionEvent::InputPipelineCouldNotForwardEventFromDriver,
                        std::format!("could not forward event from driver: {:?}", e));
                }
            }

            metrics_logger_clone.log_error(
                InputPipelineErrorMetricDimensionEvent::InputPipelineStopHandlingEvents,
                "Input pipeline stopped handling input events.".to_string(),
            );
        }.boxed_local();

        let tasks = crate::task::InputPipelineTasks {
            watcher: self.watcher_fut.unwrap_or_else(|| Box::pin(futures::future::pending())),
            runner: self.runner_fut.unwrap_or_else(|| Box::pin(futures::future::pending())),
            display_ownership: self.display_ownership_fut,
            focus_listener: self.focus_listener_fut,
            forwarder,
        };

        tasks.run().await;
    }

    async fn re_enumerate_devices(
        service: fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>,
        device_types: &[input_device::InputDeviceType],
        input_event_sender: &UnboundedSender<Vec<input_device::InputEvent>>,
        bindings: &InputDeviceBindingMap,
        bound_instances: &BoundInstances,
        inflight_devices: &InflightDeviceMap,
        display_ownership: Option<&Rc<DisplayOwnership>>,
        input_devices_node: &fuchsia_inspect::Node,
        devices_discovered: Option<&fuchsia_inspect::UintProperty>,
        devices_connected: Option<&fuchsia_inspect::UintProperty>,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
        device_listener_registry: DeviceListenerRegistry,
    ) {
        match service.enumerate().await {
            Ok(instances) => {
                for instance in instances {
                    Self::connect_and_bind_device(
                        &instance,
                        device_types,
                        input_event_sender,
                        bindings,
                        bound_instances,
                        inflight_devices,
                        display_ownership,
                        input_devices_node,
                        devices_discovered,
                        devices_connected,
                        feature_flags.clone(),
                        metrics_logger.clone(),
                        device_listener_registry.clone(),
                    )
                    .await;
                }
            }
            Err(e) => {
                metrics_logger.log_warn(
                    InputPipelineErrorMetricDimensionEvent::InputPipelineUnableToWatchForNewInputDevices,
                    std::format!("Failed to enumerate devices on ownership restore: {:?}", e),
                );
            }
        }
    }

    async fn connect_and_bind_device(
        instance: &fidl_fuchsia_input_report::ServiceProxy,
        device_types: &[input_device::InputDeviceType],
        input_event_sender: &UnboundedSender<Vec<input_device::InputEvent>>,
        bindings: &InputDeviceBindingMap,
        bound_instances: &BoundInstances,
        inflight_devices: &InflightDeviceMap,
        display_ownership: Option<&Rc<DisplayOwnership>>,
        input_devices_node: &fuchsia_inspect::Node,
        devices_discovered: Option<&fuchsia_inspect::UintProperty>,
        devices_connected: Option<&fuchsia_inspect::UintProperty>,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
        device_listener_registry: DeviceListenerRegistry,
    ) {
        let filename = instance.instance_name().to_string();

        let ownership_state = display_ownership.map(|o| o.ownership_state());
        if ownership_state == Some(OwnershipState::DisplayUnowned) {
            log::info!("skipping device {} because display is unowned", filename);
            return;
        }

        let is_client_input_owned = ownership_state == Some(OwnershipState::ClientInputOwned);

        // If client input is owned, we only care about ConsumerControls.
        let candidate_types: Vec<input_device::InputDeviceType> = if is_client_input_owned {
            if device_types.contains(&input_device::InputDeviceType::ConsumerControls) {
                vec![input_device::InputDeviceType::ConsumerControls]
            } else {
                return;
            }
        } else {
            device_types.to_vec()
        };

        // Determine what types are needed and not already bound or in-flight.
        let (needed_types, is_first_discovery) = {
            let mut bound = bound_instances.lock();
            let already_bound = bound.get(&filename);
            let is_first = already_bound.is_none();
            let mut needed = Vec::new();
            for t in &candidate_types {
                if already_bound.map_or(true, |set| !set.contains(t)) {
                    needed.push(*t);
                }
            }
            if needed.is_empty() {
                log::info!(
                    "device {} has all candidate types {:?} already bound or in-flight, skipping",
                    filename,
                    candidate_types
                );
                return;
            }
            if let Some(entry) = bound.get_mut(&filename) {
                for t in &needed {
                    entry.insert(*t);
                }
            } else {
                let mut set = SortedVecSet::new();
                for t in &needed {
                    set.insert(*t);
                }
                bound.insert(filename.clone(), set);
            }
            (needed, is_first)
        };

        // In PlatformOwned (or no display ownership), if this is the first discovery of this device instance,
        // increment devices_discovered.
        // In ClientInputOwned, we do NOT increment devices_discovered upfront; we only increment it if
        // a ConsumerControls device is actually connected/bound.
        if !is_client_input_owned && is_first_discovery {
            log::info!("found input device {}", filename);
            if let Some(devices_discovered) = devices_discovered {
                devices_discovered.add(1);
            }
        }

        let bound_instances_clone = bound_instances.clone();
        let filename_clone = filename.clone();
        let needed_types_clone = needed_types.clone();

        let inflight_devices_clone = inflight_devices.clone();
        let filename_for_inflight = filename.clone();

        let res = async {
            let (device_client, device_id, join_handle_shared) = {
                let existing_binding = {
                    let bindings_guard = bindings.lock();
                    bindings_guard.iter().find_map(|(&id, tasks)| {
                        tasks
                            .iter()
                            .find(|t| !t.is_injected && t.instance_name == filename_for_inflight)
                            .map(|t| (id, t.device_proxy.clone()))
                    })
                };
                if let Some((existing_id, existing_proxy)) = existing_binding {
                    (existing_proxy, existing_id, None)
                } else {
                    let mut inflight_guard = inflight_devices_clone.lock();
                    if let Some(entry) = inflight_guard.get_mut(&filename_for_inflight) {
                        entry.active_count += 1;
                        (
                            entry.device_client.clone(),
                            entry.device_id,
                            Some(entry.join_handle.clone()),
                        )
                    } else {
                        let device_proxy =
                            instance.connect_to_input_device().context("connect to input device")?;
                        let channel = device_proxy
                            .into_client_end()
                            .map_err(|_| format_err!("failed to get client end"))?
                            .into_channel();
                        let device_client = fidl_next::ClientEnd::<
                            fidl_next_fuchsia_input_report::InputDevice,
                            zx::Channel,
                        >::from_untyped(channel);
                        let device_client = Dispatcher::client_from_zx_channel(device_client);
                        let (device_client, join_handle) = device_client.spawn_full();
                        let device_id = get_next_device_id();
                        let join_handle_shared = Arc::new(Mutex::new(Some(join_handle)));
                        inflight_guard.insert(
                            filename_for_inflight.clone(),
                            InflightDevice {
                                device_id,
                                device_client: device_client.clone(),
                                join_handle: join_handle_shared.clone(),
                                active_count: 1,
                            },
                        );
                        (device_client, device_id, Some(join_handle_shared))
                    }
                }
            };

            let _inflight_guard = if join_handle_shared.is_some() {
                Some(InflightGuard {
                    inflight_devices: inflight_devices_clone.clone(),
                    filename: filename_for_inflight.clone(),
                    active: true,
                })
            } else {
                None
            };

            let add_result = add_device_bindings(
                device_types,
                &needed_types,
                is_client_input_owned,
                &filename,
                device_client,
                input_event_sender,
                bindings,
                device_id,
                input_devices_node,
                devices_connected,
                feature_flags,
                metrics_logger,
                display_ownership.cloned(),
                false,
            )
            .await;

            if let Some((descriptor, bound_types)) = add_result {
                // If in ClientInputOwned and this is the first discovery, increment devices_discovered now.
                if is_client_input_owned && is_first_discovery {
                    log::info!("found input device {} (client input owned)", filename);
                    if let Some(devices_discovered) = devices_discovered {
                        devices_discovered.add(1);
                    }
                }

                // Clean up any types in `needed_types` that were NOT actually bound.
                {
                    let mut bound = bound_instances_clone.lock();
                    if let Some(entry) = bound.get_mut(&filename_clone) {
                        for t in &needed_types_clone {
                            if !bound_types.contains(t) {
                                entry.remove(t);
                            }
                        }
                    }
                }

                // Check post-await ownership race guard.
                let mut dropped_tasks = Vec::new();
                let keep_device = if let Some(ownership) = display_ownership {
                    match ownership.ownership_state() {
                        OwnershipState::DisplayUnowned => {
                            log::info!(
                                "display unowned while binding device {}, dropping binding",
                                filename_clone
                            );
                            let mut bindings_guard = bindings.lock();
                            if let Some(tasks) = bindings_guard.remove(&device_id) {
                                dropped_tasks.extend(tasks);
                            }
                            let mut bound = bound_instances_clone.lock();
                            bound.remove(&filename_clone);
                            inflight_devices_clone.lock().remove(&filename_clone);
                            false
                        }
                        OwnershipState::ClientInputOwned => {
                            let mut bindings_guard = bindings.lock();
                            if let Some(binding_and_tasks) = bindings_guard.remove(&device_id) {
                                let (kept, dropped): (Vec<_>, Vec<_>) =
                                    binding_and_tasks.into_iter().partition(|bt| {
                                        matches!(
                                            bt.binding.get_device_descriptor(),
                                            input_device::InputDeviceDescriptor::ConsumerControls(
                                                _
                                            )
                                        )
                                    });
                                dropped_tasks.extend(dropped);
                                if kept.is_empty() {
                                    let mut bound = bound_instances_clone.lock();
                                    if let Some(entry) = bound.get_mut(&filename_clone) {
                                        for t in &bound_types {
                                            entry.remove(t);
                                        }
                                        if entry.is_empty() {
                                            bound.remove(&filename_clone);
                                        }
                                    }
                                    false
                                } else {
                                    // Deduplicate within the same device_id.
                                    let mut unique_kept = Vec::new();
                                    for task in kept {
                                        let is_dup = unique_kept.iter().any(|existing: &InputDeviceBindingAndTask| {
                                            !existing.is_injected
                                                && existing.instance_name == filename_clone
                                                && existing.device_type == task.device_type
                                        });
                                        if is_dup {
                                            dropped_tasks.push(task);
                                        } else {
                                            unique_kept.push(task);
                                        }
                                    }
                                    let kept = unique_kept;

                                    let has_duplicate = bindings_guard.iter().any(|(&id, tasks)| {
                                        id != device_id
                                            && tasks.iter().any(|t| {
                                                !t.is_injected
                                                    && t.instance_name == filename_clone
                                                    && matches!(
                                                        t.binding.get_device_descriptor(),
                                                        input_device::InputDeviceDescriptor::ConsumerControls(_)
                                                    )
                                            })
                                    });
                                    if has_duplicate || kept.is_empty() {
                                        dropped_tasks.extend(kept);
                                        let mut bound = bound_instances_clone.lock();
                                        if let Some(entry) = bound.get_mut(&filename_clone) {
                                            for t in &bound_types {
                                                if *t
                                                    != input_device::InputDeviceType::ConsumerControls
                                                {
                                                    entry.remove(t);
                                                }
                                            }
                                            if entry.is_empty() {
                                                bound.remove(&filename_clone);
                                            }
                                        }
                                        false
                                    } else {
                                        bindings_guard.insert(device_id, kept);
                                        let mut bound = bound_instances_clone.lock();
                                        if let Some(entry) = bound.get_mut(&filename_clone) {
                                            entry.insert(
                                                input_device::InputDeviceType::ConsumerControls,
                                            );
                                            for t in &bound_types {
                                                if *t
                                                    != input_device::InputDeviceType::ConsumerControls
                                                {
                                                    entry.remove(t);
                                                }
                                            }
                                        } else {
                                            let mut set = SortedVecSet::new();
                                            set.insert(
                                                input_device::InputDeviceType::ConsumerControls,
                                            );
                                            bound.insert(filename_clone.clone(), set);
                                        }
                                        true
                                    }
                                }
                            } else {
                                false
                            }
                        }
                        OwnershipState::PlatformOwned => {
                            let mut bindings_guard = bindings.lock();
                            // Deduplicate within the same device_id first.
                            if let Some(tasks) = bindings_guard.get_mut(&device_id) {
                                let old_tasks = std::mem::take(tasks);
                                let mut unique_tasks = Vec::new();
                                for task in old_tasks {
                                    let is_dup = unique_tasks.iter().any(|existing: &InputDeviceBindingAndTask| {
                                        !existing.is_injected
                                            && existing.instance_name == filename_clone
                                            && existing.device_type == task.device_type
                                    });
                                    if is_dup {
                                        dropped_tasks.push(task);
                                    } else {
                                        unique_tasks.push(task);
                                    }
                                }
                                *tasks = unique_tasks;
                            }

                            let other_device_id =
                                bindings_guard.iter().find_map(|(&id, tasks)| {
                                    if id != device_id
                                        && tasks.iter().any(|t| {
                                            !t.is_injected && t.instance_name == filename_clone
                                        })
                                    {
                                        Some(id)
                                    } else {
                                        None
                                    }
                                });
                            if let Some(existing_id) = other_device_id {
                                let current_tasks =
                                    bindings_guard.remove(&device_id).unwrap_or_default();
                                for task in current_tasks {
                                    let is_duplicate =
                                        bindings_guard.iter().any(|(&id, tasks)| {
                                            id != device_id
                                                && tasks.iter().any(|t| {
                                                    !t.is_injected
                                                        && t.instance_name == filename_clone
                                                        && t.device_type == task.device_type
                                                })
                                        });
                                    if is_duplicate {
                                        dropped_tasks.push(task);
                                    } else if let Some(existing_tasks) =
                                        bindings_guard.get_mut(&existing_id)
                                    {
                                        existing_tasks.push(task);
                                    } else {
                                        dropped_tasks.push(task);
                                    }
                                }

                                let mut bound = bound_instances_clone.lock();
                                if let Some(entry) = bound.get_mut(&filename_clone) {
                                    for t in &bound_types {
                                        entry.insert(*t);
                                    }
                                } else {
                                    let mut set = SortedVecSet::new();
                                    for t in &bound_types {
                                        set.insert(*t);
                                    }
                                    bound.insert(filename_clone.clone(), set);
                                }
                                false
                            } else {
                                let mut bound = bound_instances_clone.lock();
                                if let Some(entry) = bound.get_mut(&filename_clone) {
                                    for t in &bound_types {
                                        entry.insert(*t);
                                    }
                                } else {
                                    let mut set = SortedVecSet::new();
                                    for t in &bound_types {
                                        set.insert(*t);
                                    }
                                    bound.insert(filename_clone.clone(), set);
                                }
                                true
                            }
                        }
                    }
                } else {
                    let mut bindings_guard = bindings.lock();
                    if let Some(tasks) = bindings_guard.get_mut(&device_id) {
                        let old_tasks = std::mem::take(tasks);
                        let mut unique_tasks = Vec::new();
                        for task in old_tasks {
                            let is_dup = unique_tasks.iter().any(|existing: &InputDeviceBindingAndTask| {
                                !existing.is_injected
                                    && existing.instance_name == filename_clone
                                    && existing.device_type == task.device_type
                            });
                            if is_dup {
                                dropped_tasks.push(task);
                            } else {
                                unique_tasks.push(task);
                            }
                        }
                        *tasks = unique_tasks;
                    }
                    true
                };

                // Drop any discarded tasks outside bindings.lock().
                std::mem::drop(dropped_tasks);

                if !keep_device {
                    return Ok::<(), Error>(());
                }

                if let Some(join_handle_mutex) = &join_handle_shared {
                    if let Some(join_handle) = join_handle_mutex.lock().take() {
                        device_listener_registry.notify_device_changed(
                            fidl_next_fuchsia_ui_input::Action::Added,
                            device_id,
                            descriptor.clone(),
                        );
                        let device_listener_registry_clone = device_listener_registry.clone();
                        let bindings_clone = bindings.clone();
                        let bound_instances_disconnect = bound_instances_clone.clone();
                        let inflight_devices_disconnect = inflight_devices_clone.clone();
                        let filename_disconnect = filename_clone.clone();
                        let watch_task = Dispatcher::spawn_local(async move {
                            let _ = join_handle.await;
                            log::info!("Device {} disconnected", device_id);
                            device_listener_registry_clone.notify_device_changed(
                                fidl_next_fuchsia_ui_input::Action::Removed,
                                device_id,
                                descriptor,
                            );
                            if bindings_clone.lock().remove(&device_id).is_some() {
                                let mut bound = bound_instances_disconnect.lock();
                                bound.remove(&filename_disconnect);
                                inflight_devices_disconnect.lock().remove(&filename_disconnect);
                            }
                        });
                        watch_task.detach();
                    }
                }
            } else {
                let mut bound = bound_instances_clone.lock();
                if let Some(entry) = bound.get_mut(&filename_clone) {
                    for t in &needed_types_clone {
                        entry.remove(t);
                    }
                    if entry.is_empty() {
                        bound.remove(&filename_clone);
                    }
                }
            }
            Ok::<(), Error>(())
        }
        .await;

        if let Err(e) = res {
            let mut bound = bound_instances.lock();
            if let Some(entry) = bound.get_mut(&filename) {
                for t in &needed_types {
                    entry.remove(t);
                }
                if entry.is_empty() {
                    bound.remove(&filename);
                }
            }
            log::error!("Failed to connect and bind input device {}: {:?}", filename, e);
        }
    }

    /// Watches the input report service for new input devices. Creates InputDeviceBindings
    /// if new devices match a type in `device_types`.
    ///
    /// # Parameters
    /// - `service`: The service to watch for new devices.
    /// - `device_types`: The types of devices to watch for.
    /// - `input_event_sender`: The channel new InputDeviceBindings will send InputEvents to.
    /// - `bindings`: Holds all the InputDeviceBindings
    /// - `input_devices_node`: The parent node for all device bindings' inspect nodes.
    /// - `devices_discovered`: Inspect property to track discovered devices.
    /// - `devices_connected`: Inspect property to track connected devices.
    /// - `break_on_idle`: If true, stops watching for devices once all existing devices are handled.
    /// - `feature_flags`: The feature flags.
    /// - `metrics_logger`: The metrics logger.
    ///
    /// # Errors
    /// If the input report service or a connection within it fails.
    pub async fn watch_for_devices(
        service: fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>,
        device_types: Vec<input_device::InputDeviceType>,
        input_event_sender: UnboundedSender<Vec<input_device::InputEvent>>,
        bindings: InputDeviceBindingMap,
        input_devices_node: &fuchsia_inspect::Node,
        devices_discovered: &fuchsia_inspect::UintProperty,
        devices_connected: &fuchsia_inspect::UintProperty,
        break_on_idle: bool,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
        device_listener_registry: DeviceListenerRegistry,
    ) -> Result<(), Error> {
        let bound_instances: BoundInstances = Arc::new(Mutex::new(SortedVecMap::new()));
        let inflight_devices: InflightDeviceMap = Arc::new(Mutex::new(SortedVecMap::new()));
        Self::watch_for_devices_internal(
            service,
            device_types,
            input_event_sender,
            bindings,
            bound_instances,
            inflight_devices,
            None,
            input_devices_node,
            devices_discovered,
            devices_connected,
            break_on_idle,
            feature_flags,
            metrics_logger,
            device_listener_registry,
        )
        .await
    }

    async fn watch_for_devices_internal(
        service: fuchsia_component::client::Service<fidl_fuchsia_input_report::ServiceMarker>,
        device_types: Vec<input_device::InputDeviceType>,
        input_event_sender: UnboundedSender<Vec<input_device::InputEvent>>,
        bindings: InputDeviceBindingMap,
        bound_instances: BoundInstances,
        inflight_devices: InflightDeviceMap,
        display_ownership: Option<Rc<DisplayOwnership>>,
        input_devices_node: &fuchsia_inspect::Node,
        devices_discovered: &fuchsia_inspect::UintProperty,
        devices_connected: &fuchsia_inspect::UintProperty,
        break_on_idle: bool,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
        device_listener_registry: DeviceListenerRegistry,
    ) -> Result<(), Error> {
        if break_on_idle {
            let instances = service.enumerate().await?;
            for instance in instances {
                Self::connect_and_bind_device(
                    &instance,
                    &device_types,
                    &input_event_sender,
                    &bindings,
                    &bound_instances,
                    &inflight_devices,
                    display_ownership.as_ref(),
                    input_devices_node,
                    Some(devices_discovered),
                    Some(devices_connected),
                    feature_flags.clone(),
                    metrics_logger.clone(),
                    device_listener_registry.clone(),
                )
                .await;
            }
            Ok(())
        } else {
            let mut instances = service
                .watch()
                .await
                .context("failed to watch input-report service")?
                .err_into::<Error>()
                .boxed();

            while let Some(instance) = instances.try_next().await? {
                Self::connect_and_bind_device(
                    &instance,
                    &device_types,
                    &input_event_sender,
                    &bindings,
                    &bound_instances,
                    &inflight_devices,
                    display_ownership.as_ref(),
                    input_devices_node,
                    Some(devices_discovered),
                    Some(devices_connected),
                    feature_flags.clone(),
                    metrics_logger.clone(),
                    device_listener_registry.clone(),
                )
                .await;
            }

            Err(format_err!("Input pipeline stopped watching for new input devices."))
        }
    }

    /// Handles the incoming InputDeviceRegistryRequestStream.
    ///
    /// This method will end when the request stream is closed. If the stream closes with an
    /// error the error will be returned in the Result.
    ///
    /// **NOTE**: Only one stream is handled at a time. https://fxbug.dev/42061078
    ///
    /// # Parameters
    /// - `stream`: The stream of InputDeviceRegistryRequests.
    /// - `device_types`: The types of devices to watch for.
    /// - `input_event_sender`: The channel new InputDeviceBindings will send InputEvents to.
    /// - `bindings`: Holds all the InputDeviceBindings associated with the InputPipeline.
    /// - `input_devices_node`: The parent node for all injected devices' inspect nodes.
    /// - `metrics_logger`: The metrics logger.
    pub async fn handle_input_device_registry_request_stream(
        mut stream: fidl_fuchsia_input_injection::InputDeviceRegistryRequestStream,
        device_types: &[input_device::InputDeviceType],
        input_event_sender: &UnboundedSender<Vec<input_device::InputEvent>>,
        bindings: &InputDeviceBindingMap,
        input_devices_node: &fuchsia_inspect::Node,
        feature_flags: input_device::InputPipelineFeatureFlags,
        metrics_logger: metrics::MetricsLogger,
        device_listener_registry: DeviceListenerRegistry,
        task_sender: UnboundedSender<crate::dispatcher::TaskHandle<()>>,
        display_ownership: Option<Rc<DisplayOwnership>>,
    ) -> Result<(), Error> {
        while let Some(request) = stream
            .try_next()
            .await
            .context("Error handling input device registry request stream")?
        {
            match request {
                fidl_fuchsia_input_injection::InputDeviceRegistryRequest::Register {
                    device,
                    ..
                } => {
                    // Add a binding if the device is a type being tracked
                    let device = fidl_next::ClientEnd::<
                        fidl_next_fuchsia_input_report::InputDevice,
                        zx::Channel,
                    >::from_untyped(device.into_channel());
                    let device = Dispatcher::client_from_zx_channel(device);
                    let (device, join_handle) = device.spawn_full();
                    let device_id = get_next_device_id();

                    if let Some((descriptor, _)) = add_device_bindings(
                        device_types,
                        device_types,
                        false,
                        &format!("input-device-registry-{}", device_id),
                        device,
                        input_event_sender,
                        bindings,
                        device_id,
                        input_devices_node,
                        None,
                        feature_flags.clone(),
                        metrics_logger.clone(),
                        display_ownership.clone(),
                        true,
                    )
                    .await
                    {
                        device_listener_registry.notify_device_changed(
                            fidl_next_fuchsia_ui_input::Action::Added,
                            device_id,
                            descriptor.clone(),
                        );

                        let device_listener_registry_clone = device_listener_registry.clone();
                        let bindings_clone = bindings.clone();
                        let watch_task = Dispatcher::spawn_local(async move {
                            let _ = join_handle.await;
                            log::info!("Injected device {} disconnected", device_id);
                            device_listener_registry_clone.notify_device_changed(
                                fidl_next_fuchsia_ui_input::Action::Removed,
                                device_id,
                                descriptor,
                            );
                            bindings_clone.lock().remove(&device_id);
                        });
                        let _ = task_sender.unbounded_send(watch_task);
                    }
                }
                fidl_fuchsia_input_injection::InputDeviceRegistryRequest::RegisterAndGetDeviceInfo {
                    device,
                    responder,
                    .. } => {
                    // Add a binding if the device is a type being tracked
                    let device = fidl_next::ClientEnd::<
                        fidl_next_fuchsia_input_report::InputDevice,
                        zx::Channel,
                    >::from_untyped(device.into_channel());
                    let device = Dispatcher::client_from_zx_channel(device);
                    let (device, join_handle) = device.spawn_full();
                    let device_id = get_next_device_id();

                    let descriptor_opt = add_device_bindings(
                        device_types,
                        device_types,
                        false,
                        &format!("input-device-registry-{}", device_id),
                        device,
                        input_event_sender,
                        bindings,
                        device_id,
                        input_devices_node,
                        None,
                        feature_flags.clone(),
                        metrics_logger.clone(),
                        display_ownership.clone(),
                        true,
                    )
                    .await;

                    if let Some((descriptor, _)) = descriptor_opt {
                        device_listener_registry.notify_device_changed(
                            fidl_next_fuchsia_ui_input::Action::Added,
                            device_id,
                            descriptor.clone(),
                        );

                        let device_listener_registry_clone = device_listener_registry.clone();
                        let bindings_clone = bindings.clone();
                        let watch_task = Dispatcher::spawn_local(async move {
                            let _ = join_handle.await;
                            log::info!("Injected device {} disconnected", device_id);
                            device_listener_registry_clone.notify_device_changed(
                                fidl_next_fuchsia_ui_input::Action::Removed,
                                device_id,
                                descriptor.clone(),
                            );
                            bindings_clone.lock().remove(&device_id);
                        });
                        let _ = task_sender.unbounded_send(watch_task);
                    }

                    responder.send(fidl_fuchsia_input_injection::InputDeviceRegistryRegisterAndGetDeviceInfoResponse{
                        device_id: Some(device_id),
                        ..Default::default()
                    }).expect("Failed to respond to RegisterAndGetDeviceInfo request");
                }
            }
        }

        Ok(())
    }

    /// Initializes all handlers and starts the input pipeline loop in an asynchronous executor.
    fn run(
        mut receiver: UnboundedReceiver<Vec<input_device::InputEvent>>,
        handlers: Vec<Rc<dyn input_handler::BatchInputHandler>>,
        metrics_logger: metrics::MetricsLogger,
    ) -> LocalBoxFuture<'static, ()> {
        async move {
            for handler in &handlers {
                handler.clone().set_handler_healthy();
            }

            let mut handlers_by_type: [Vec<Rc<dyn input_handler::BatchInputHandler>>;
                InputEventType::COUNT] = Default::default();

            // TODO: b/478262850 - We can use supported_input_devices to populate this list.
            let event_types = vec![
                InputEventType::Keyboard,
                InputEventType::LightSensor,
                InputEventType::ConsumerControls,
                InputEventType::Mouse,
                InputEventType::TouchScreen,
                InputEventType::Touchpad,
                #[cfg(test)]
                InputEventType::Fake,
            ];

            for event_type in event_types {
                let handlers_for_type: Vec<Rc<dyn input_handler::BatchInputHandler>> = handlers
                    .iter()
                    .filter(|h| h.interest().contains(&event_type))
                    .cloned()
                    .collect();
                handlers_by_type[event_type as usize] = handlers_for_type;
            }

            while let Some(events) = receiver.next().await {
                if events.is_empty() {
                    continue;
                }

                let mut groups_seen = 0;
                let events = events.into_iter().chunk_by(|e| InputEventType::from(&e.device_event));
                let events = events.into_iter().map(|(k, v)| (k, v.collect::<Vec<_>>()));
                for (event_type, event_group) in events {
                    groups_seen += 1;
                    if groups_seen == 2 {
                        metrics_logger.log_error(
                                InputPipelineErrorMetricDimensionEvent::InputFrameContainsMultipleTypesOfEvents,
                                "it is not recommended to contain multiple types of events in 1 send".to_string(),
                            );
                    }
                    let mut events_in_group = event_group;

                    // Get pre-computed handlers for this event type.
                    let handlers = &handlers_by_type[event_type as usize];

                    for handler in handlers {
                        events_in_group =
                            handler.clone().handle_input_events(events_in_group).await;
                    }

                    for event in events_in_group {
                        if event.handled == input_device::Handled::No {
                            log::warn!("unhandled input event: {:?}", event);
                        }
                        if let Some(trace_id) = event.trace_id {
                            fuchsia_trace::flow_end!(
                                "input",
                                "event_in_input_pipeline",
                                trace_id.into()
                            );
                        }
                    }
                }
            }
            for handler in &handlers {
                handler.clone().set_handler_unhealthy("Pipeline loop terminated");
            }
            panic!("Runner task is not supposed to terminate.")
        }
        .boxed_local()
    }
}

/// Adds `InputDeviceBinding`s to `bindings` for all `device_types` exposed by `device_proxy`.
///
/// # Parameters
/// - `device_types`: The types of devices to watch for.
/// - `device_proxy`: A proxy to the input device.
/// - `input_event_sender`: The channel new InputDeviceBindings will send InputEvents to.
/// - `bindings`: Holds all the InputDeviceBindings associated with the InputPipeline.
/// - `device_id`: The device id of the associated bindings.
/// - `input_devices_node`: The parent node for all device bindings' inspect nodes.
///
/// # Note
/// This will create multiple bindings, in the case where
/// * `device_proxy().get_descriptor()` returns a `fidl_fuchsia_input_report::DeviceDescriptor`
///   with multiple table fields populated, and
/// * multiple populated table fields correspond to device types present in `device_types`
///
/// This is used, for example, to support the Atlas touchpad. In that case, a single
/// instance of `fuchsia.input.report.Service` provides both a `fuchsia.input.report.MouseDescriptor` and
/// a `fuchsia.input.report.TouchDescriptor`.
async fn add_device_bindings(
    pipeline_device_types: &[input_device::InputDeviceType],
    target_device_types: &[input_device::InputDeviceType],
    is_client_input_owned: bool,
    instance_name: &str,
    device_proxy: fidl_next::Client<fidl_next_fuchsia_input_report::InputDevice, Transport>,
    input_event_sender: &UnboundedSender<Vec<input_device::InputEvent>>,
    bindings: &InputDeviceBindingMap,
    device_id: u32,
    input_devices_node: &fuchsia_inspect::Node,
    devices_connected: Option<&fuchsia_inspect::UintProperty>,
    feature_flags: InputPipelineFeatureFlags,
    metrics_logger: metrics::MetricsLogger,
    display_ownership: Option<Rc<DisplayOwnership>>,
    is_injected: bool,
) -> Option<(fidl_next_fuchsia_input_report::DeviceDescriptor, Vec<input_device::InputDeviceType>)>
{
    let mut matched_device_types = vec![];
    let descriptor = match device_proxy.get_descriptor().await {
        Ok(res) => {
            for device_type in target_device_types {
                if input_device::is_device_type(&res.descriptor, *device_type).await {
                    matched_device_types.push(*device_type);
                    match devices_connected {
                        Some(dev_connected) => {
                            let _ = dev_connected.add(1);
                        }
                        None => (),
                    };
                }
            }
            if matched_device_types.is_empty() {
                let mut is_supported_by_pipeline = false;
                for device_type in pipeline_device_types {
                    if input_device::is_device_type(&res.descriptor, *device_type).await {
                        is_supported_by_pipeline = true;
                        break;
                    }
                }
                if !is_client_input_owned && !is_supported_by_pipeline {
                    log::info!(
                        "device {} did not match any supported device types: {:?}",
                        instance_name,
                        pipeline_device_types
                    );
                    let device_node =
                        input_devices_node.create_child(format!("{}_Unsupported", instance_name));
                    let mut health = fuchsia_inspect::health::Node::new(&device_node);
                    health.set_unhealthy("Unsupported device type.");
                    device_node.record(health);
                    input_devices_node.record(device_node);
                }
                return None;
            }
            res.descriptor
        }
        Err(_) => {
            metrics_logger.clone().log_error(
                InputPipelineErrorMetricDimensionEvent::InputPipelineNoDeviceDescriptor,
                std::format!("cannot bind device {} without a device descriptor", instance_name),
            );
            return None;
        }
    };

    log::info!(
        "binding {} to device types: {}",
        instance_name,
        matched_device_types
            .iter()
            .fold(String::new(), |device_types_string, device_type| device_types_string
                + &format!("{:?}, ", device_type))
    );

    let mut new_bindings: Vec<InputDeviceBindingAndTask> = vec![];
    let mut bound_types: Vec<input_device::InputDeviceType> = vec![];
    for device_type in matched_device_types {
        // Clone `device_proxy`, so that multiple bindings (e.g. a `MouseBinding` and a
        // `TouchBinding`) can read data from the same `fuchsia.input.report.Service` instance.
        //
        // There's no conflict in having multiple bindings read from the same node,
        // since:
        // * each binding will create its own `fuchsia.input.report.InputReportsReader`, and
        // * the device driver will copy each incoming report to each connected reader.
        //
        // This does mean that reports from the Atlas touchpad device get read twice
        // (by a `MouseBinding` and a `TouchBinding`), regardless of whether the device
        // is operating in mouse mode or touchpad mode.
        //
        // This hasn't been an issue because:
        // * Semantically: things are fine, because each binding discards irrelevant reports.
        //   (E.g. `MouseBinding` discards anything that isn't a `MouseInputReport`), and
        // * Performance wise: things are fine, because the data rate of the touchpad is low
        //   (125 HZ).
        //
        // If we add additional cases where bindings share an underlying service instance,
        // we might consider adding a multiplexing binding, to avoid reading duplicate reports.
        let proxy = device_proxy.clone();
        let device_node =
            input_devices_node.create_child(format!("{}_{}", instance_name, device_type));
        match input_device::get_device_binding(
            device_type,
            proxy,
            device_id,
            input_event_sender.clone(),
            device_node,
            feature_flags.clone(),
            metrics_logger.clone(),
            display_ownership.clone(),
            is_injected,
        )
        .await
        {
            Ok((binding, task)) => {
                new_bindings.push(InputDeviceBindingAndTask {
                    binding,
                    task,
                    instance_name: instance_name.to_string(),
                    device_type,
                    device_proxy: device_proxy.clone(),
                    is_injected,
                });
                bound_types.push(device_type);
            }
            Err(e) => {
                metrics_logger.log_error(
                    InputPipelineErrorMetricDimensionEvent::InputPipelineFailedToBind,
                    std::format!("failed to bind {} as {:?}: {}", instance_name, device_type, e),
                );
            }
        }
    }

    if !new_bindings.is_empty() {
        let mut bindings = bindings.lock();
        if let Some(v) = bindings.get_mut(&device_id) {
            v.extend(new_bindings);
        } else {
            bindings.insert(device_id, new_bindings);
        }
        Some((descriptor, bound_types))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatcher::Transport;
    use fidl_next::Request;

    struct MockDeviceListener {
        event_sender:
            futures::channel::mpsc::UnboundedSender<fidl_next_fuchsia_ui_input::DeviceEvent>,
    }

    impl fidl_next_fuchsia_ui_input::DeviceListenerServerHandler<Transport> for MockDeviceListener {
        async fn on_device_changed(
            &mut self,
            request: Request<
                fidl_next_fuchsia_ui_input::device_listener::OnDeviceChanged,
                Transport,
            >,
        ) {
            let event = request.payload().event.clone();
            let _ = self.event_sender.unbounded_send(event);
        }
    }
    use crate::input_device::{InputDeviceBinding, InputEventType};
    use crate::utils::Position;
    use crate::{fake_input_device_binding, mouse_binding, observe_fake_events_input_handler};
    use async_trait::async_trait;
    use diagnostics_assertions::AnyProperty;
    use fidl::endpoints::{create_proxy_and_stream, create_request_stream};
    use fidl_fuchsia_io as fio;
    use fuchsia_async as fasync;
    use futures::{FutureExt, StreamExt};
    use pretty_assertions::assert_eq;
    use rand::RngExt as _;
    use sorted_vec_map::SortedVecSet;
    use vfs::{pseudo_directory, service as pseudo_fs_service};

    /// Returns the InputEvent sent over `sender`.
    ///
    /// # Parameters
    /// - `sender`: The channel to send the InputEvent over.
    fn send_input_event(
        sender: UnboundedSender<Vec<input_device::InputEvent>>,
    ) -> Vec<input_device::InputEvent> {
        let mut rng = rand::rng();
        let offset =
            Position { x: rng.random_range(0..10) as f32, y: rng.random_range(0..10) as f32 };
        let input_event = input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(mouse_binding::MouseEvent::new(
                mouse_binding::MouseLocation::Relative(mouse_binding::RelativeLocation {
                    counts: Position { x: offset.x, y: offset.y },
                }),
                None, /* wheel_delta_v */
                None, /* wheel_delta_h */
                mouse_binding::MousePhase::Move,
                SortedVecSet::new(),
                SortedVecSet::new(),
                None, /* is_precision_scroll */
                None, /* wake_lease */
            )),
            device_descriptor: input_device::InputDeviceDescriptor::Mouse(
                mouse_binding::MouseDeviceDescriptor {
                    device_id: 1,
                    absolute_x_range: None,
                    absolute_y_range: None,
                    wheel_v_range: None,
                    wheel_h_range: None,
                    buttons: None,
                },
            ),
            event_time: zx::MonotonicInstant::get(),
            handled: input_device::Handled::No,
            trace_id: None,
        };
        match sender.unbounded_send(vec![input_event.clone()]) {
            Err(_) => assert!(false),
            _ => {}
        }

        vec![input_event]
    }

    /// Returns a MouseDescriptor on an InputDeviceRequest.
    ///
    /// # Parameters
    /// - `input_device_request`: The request to handle.
    fn handle_input_device_request(
        input_device_request: fidl_fuchsia_input_report::InputDeviceRequest,
    ) {
        match input_device_request {
            fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                let _ = responder.send(&fidl_fuchsia_input_report::DeviceDescriptor {
                    device_information: None,
                    mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                        input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                            movement_x: None,
                            movement_y: None,
                            scroll_v: None,
                            scroll_h: None,
                            buttons: Some(vec![0]),
                            position_x: None,
                            position_y: None,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    sensor: None,
                    touch: None,
                    keyboard: None,
                    consumer_control: None,
                    ..Default::default()
                });
            }
            fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                reader,
                responder,
                ..
            } => {
                let _ = responder.send(2);
                std::mem::forget(reader);
            }
            _ => {}
        }
    }

    /// Returns a ConsumerControlDescriptor on an InputDeviceRequest.
    fn handle_consumer_controls_device_request(
        input_device_request: fidl_fuchsia_input_report::InputDeviceRequest,
    ) {
        match input_device_request {
            fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                let _ = responder.send(&fidl_fuchsia_input_report::DeviceDescriptor {
                    device_information: None,
                    mouse: None,
                    sensor: None,
                    touch: None,
                    keyboard: None,
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlDescriptor {
                        input: Some(fidl_fuchsia_input_report::ConsumerControlInputDescriptor {
                            buttons: Some(vec![
                                fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                                fidl_fuchsia_input_report::ConsumerControlButton::VolumeDown,
                            ]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                reader,
                responder,
                ..
            } => {
                let _ = responder.send(2);
                std::mem::forget(reader);
            }
            _ => {}
        }
    }

    /// Returns a DeviceDescriptor supporting both Mouse and ConsumerControls on an InputDeviceRequest.
    fn handle_composite_mouse_and_consumer_controls_request(
        input_device_request: fidl_fuchsia_input_report::InputDeviceRequest,
    ) {
        match input_device_request {
            fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                let _ = responder.send(&fidl_fuchsia_input_report::DeviceDescriptor {
                    device_information: None,
                    mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                        input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                            movement_x: None,
                            movement_y: None,
                            scroll_v: None,
                            scroll_h: None,
                            buttons: Some(vec![0]),
                            position_x: None,
                            position_y: None,
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    sensor: None,
                    touch: None,
                    keyboard: None,
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlDescriptor {
                        input: Some(fidl_fuchsia_input_report::ConsumerControlInputDescriptor {
                            buttons: Some(vec![
                                fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                                fidl_fuchsia_input_report::ConsumerControlButton::VolumeDown,
                            ]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                reader,
                responder,
                ..
            } => {
                let _ = responder.send(2);
                std::mem::forget(reader);
            }
            _ => {}
        }
    }

    /// Tests that an input pipeline handles events from multiple devices.
    #[fuchsia::test]
    async fn multiple_devices_single_handler() {
        // Create two fake device bindings.
        let (device_event_sender, device_event_receiver) = futures::channel::mpsc::unbounded();
        let first_device_binding =
            fake_input_device_binding::FakeInputDeviceBinding::new(device_event_sender.clone());
        let second_device_binding =
            fake_input_device_binding::FakeInputDeviceBinding::new(device_event_sender.clone());

        // Create a fake input handler.
        let (handler_event_sender, mut handler_event_receiver) =
            futures::channel::mpsc::channel(100);
        let input_handler = observe_fake_events_input_handler::ObserveFakeEventsInputHandler::new(
            handler_event_sender,
        );

        // Build the input pipeline.
        let (sender, receiver, handlers, _, _, _, _) =
            InputPipelineAssembly::new(metrics::MetricsLogger::default())
                .add_handler(input_handler)
                .into_components();
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");
        let runner_fut =
            Some(InputPipeline::run(receiver, handlers, metrics::MetricsLogger::default()));
        let input_pipeline = InputPipeline {
            pipeline_sender: sender,
            device_event_sender,
            device_event_receiver,
            input_device_types: vec![],
            input_device_bindings: Arc::new(Mutex::new(SortedVecMap::new())),
            inspect_node: test_node,
            metrics_logger: metrics::MetricsLogger::default(),
            feature_flags: input_device::InputPipelineFeatureFlags::default(),
            display_ownership_fut: None,
            focus_listener_fut: None,
            watcher_fut: None,
            runner_fut,
            device_listener_registry: DeviceListenerRegistry::new(),
            display_ownership: None,
        };

        // Send an input event from each device.
        let first_device_events = send_input_event(first_device_binding.input_event_sender());
        let second_device_events = send_input_event(second_device_binding.input_event_sender());

        // Run the pipeline.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Assert the handler receives the events.
        let first_handled_event = handler_event_receiver.next().await;
        assert_eq!(first_handled_event, first_device_events.into_iter().next());

        let second_handled_event = handler_event_receiver.next().await;
        assert_eq!(second_handled_event, second_device_events.into_iter().next());
    }

    /// Tests that an input pipeline handles events through multiple input handlers.
    #[fuchsia::test]
    async fn single_device_multiple_handlers() {
        // Create two fake device bindings.
        let (device_event_sender, device_event_receiver) = futures::channel::mpsc::unbounded();
        let input_device_binding =
            fake_input_device_binding::FakeInputDeviceBinding::new(device_event_sender.clone());

        // Create two fake input handlers.
        let (first_handler_event_sender, mut first_handler_event_receiver) =
            futures::channel::mpsc::channel(100);
        let first_input_handler =
            observe_fake_events_input_handler::ObserveFakeEventsInputHandler::new(
                first_handler_event_sender,
            );
        let (second_handler_event_sender, mut second_handler_event_receiver) =
            futures::channel::mpsc::channel(100);
        let second_input_handler =
            observe_fake_events_input_handler::ObserveFakeEventsInputHandler::new(
                second_handler_event_sender,
            );

        // Build the input pipeline.
        let (sender, receiver, handlers, _, _, _, _) =
            InputPipelineAssembly::new(metrics::MetricsLogger::default())
                .add_handler(first_input_handler)
                .add_handler(second_input_handler)
                .into_components();
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");
        let runner_fut =
            Some(InputPipeline::run(receiver, handlers, metrics::MetricsLogger::default()));
        let input_pipeline = InputPipeline {
            pipeline_sender: sender,
            device_event_sender,
            device_event_receiver,
            input_device_types: vec![],
            input_device_bindings: Arc::new(Mutex::new(SortedVecMap::new())),
            inspect_node: test_node,
            metrics_logger: metrics::MetricsLogger::default(),
            feature_flags: input_device::InputPipelineFeatureFlags::default(),
            display_ownership_fut: None,
            focus_listener_fut: None,
            watcher_fut: None,
            runner_fut,
            device_listener_registry: DeviceListenerRegistry::new(),
            display_ownership: None,
        };

        // Send an input event.
        let input_events = send_input_event(input_device_binding.input_event_sender());

        // Run the pipeline.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Assert both handlers receive the event.
        let expected_event = input_events.into_iter().next();
        let first_handler_event = first_handler_event_receiver.next().await;
        assert_eq!(first_handler_event, expected_event);
        let second_handler_event = second_handler_event_receiver.next().await;
        assert_eq!(second_handler_event, expected_event);
    }

    /// Tests that a single mouse device binding is created for the one input device in the
    /// input report service directory.
    #[fuchsia::test]
    async fn watch_devices_one_match_exists() {
        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_0" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            async move {
                                while let Some(input_device_request) =
                                    request_stream.try_next().await.unwrap()
                                {
                                    handle_input_device_request(input_device_request);
                                }

                            }.boxed()
                        },
                    )
                }
            }
        };

        // Get a proxy to the pseudo directory for the input pipeline. The input pipeline uses this
        // proxy to get connections to input devices.
        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());

        let (input_event_sender, _input_event_receiver) = futures::channel::mpsc::unbounded();
        let bindings: InputDeviceBindingMap = Arc::new(Mutex::new(SortedVecMap::new()));
        let supported_device_types = vec![input_device::InputDeviceType::Mouse];

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");
        test_node.record_string(
            "supported_input_devices",
            supported_device_types.clone().iter().join(", "),
        );
        let input_devices = test_node.create_child("input_devices");
        // Assert that inspect tree is initialized with no devices.
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_pipeline: {
                supported_input_devices: "Mouse",
                input_devices: {}
            }
        });

        let devices_discovered = input_devices.create_uint("devices_discovered", 0);
        let devices_connected = input_devices.create_uint("devices_connected", 0);

        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let _ = InputPipeline::watch_for_devices(
            service,
            supported_device_types,
            input_event_sender,
            bindings.clone(),
            &input_devices,
            &devices_discovered,
            &devices_connected,
            true, /* break_on_idle */
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
            DeviceListenerRegistry::new(),
        )
        .await;

        // Assert that one mouse device with accurate device id was found.
        {
            let bindings_map = bindings.lock();
            assert_eq!(bindings_map.len(), 1);
            let bindings_vector = bindings_map.get(&10);
            assert!(bindings_vector.is_some());
            assert_eq!(bindings_vector.unwrap().len(), 1);
            let boxed_mouse_binding = bindings_vector.unwrap().get(0);
            assert!(boxed_mouse_binding.is_some());
            assert_eq!(
                boxed_mouse_binding.unwrap().binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::Mouse(mouse_binding::MouseDeviceDescriptor {
                    device_id: 10,
                    absolute_x_range: None,
                    absolute_y_range: None,
                    wheel_v_range: None,
                    wheel_h_range: None,
                    buttons: Some(vec![0]),
                })
            );
        }

        // Assert that inspect tree reflects new device discovered and connected.
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_pipeline: {
                supported_input_devices: "Mouse",
                input_devices: {
                    devices_discovered: 1u64,
                    devices_connected: 1u64,
                    "instance_0_Mouse": contains {
                        reports_received_count: 0u64,
                        reports_filtered_count: 0u64,
                        events_generated: 0u64,
                        last_received_timestamp_ns: 0u64,
                        last_generated_timestamp_ns: 0u64,
                        "fuchsia.inspect.Health": {
                            status: "OK",
                            // Timestamp value is unpredictable and not relevant in this context,
                            // so we only assert that the property is present.
                            start_timestamp_nanos: AnyProperty
                        },
                    }
                }
            }
        });
    }

    /// Tests that no device bindings are created because the input pipeline looks for keyboard devices
    /// but only a mouse exists.
    #[fuchsia::test]
    async fn watch_devices_no_matches_exist() {
        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_0" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            async move {
                                while let Some(input_device_request) =
                                    request_stream.try_next().await.unwrap()
                                {
                                    handle_input_device_request(input_device_request);
                                }

                            }.boxed()
                        },
                    )
                }
            }
        };

        // Get a proxy to the pseudo directory for the input pipeline. The input pipeline uses this
        // proxy to get connections to input devices.
        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());

        let (input_event_sender, _input_event_receiver) = futures::channel::mpsc::unbounded();
        let bindings: InputDeviceBindingMap = Arc::new(Mutex::new(SortedVecMap::new()));
        let supported_device_types = vec![input_device::InputDeviceType::Keyboard];

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");
        test_node.record_string(
            "supported_input_devices",
            supported_device_types.clone().iter().join(", "),
        );
        let input_devices = test_node.create_child("input_devices");
        // Assert that inspect tree is initialized with no devices.
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_pipeline: {
                supported_input_devices: "Keyboard",
                input_devices: {}
            }
        });

        let devices_discovered = input_devices.create_uint("devices_discovered", 0);
        let devices_connected = input_devices.create_uint("devices_connected", 0);

        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let _ = InputPipeline::watch_for_devices(
            service,
            supported_device_types,
            input_event_sender,
            bindings.clone(),
            &input_devices,
            &devices_discovered,
            &devices_connected,
            true, /* break_on_idle */
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
            DeviceListenerRegistry::new(),
        )
        .await;

        // Assert that no devices were found.
        {
            let bindings = bindings.lock();
            assert_eq!(bindings.len(), 0);
        }

        // Assert that inspect tree reflects new device discovered, but not connected.
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_pipeline: {
                supported_input_devices: "Keyboard",
                input_devices: {
                    devices_discovered: 1u64,
                    devices_connected: 0u64,
                    "instance_0_Unsupported": {
                        "fuchsia.inspect.Health": {
                            status: "UNHEALTHY",
                            message: "Unsupported device type.",
                            // Timestamp value is unpredictable and not relevant in this context,
                            // so we only assert that the property is present.
                            start_timestamp_nanos: AnyProperty
                        },
                    }
                }
            }
        });
    }

    /// Tests that a single keyboard device binding is created for the input device registered
    /// through InputDeviceRegistry.
    #[fuchsia::test]
    async fn handle_input_device_registry_request_stream() {
        let (input_device_registry_proxy, input_device_registry_request_stream) =
            create_proxy_and_stream::<fidl_fuchsia_input_injection::InputDeviceRegistryMarker>();
        let (input_device_client_end, mut input_device_request_stream) =
            create_request_stream::<fidl_fuchsia_input_report::InputDeviceMarker>();

        let device_types = vec![input_device::InputDeviceType::Mouse];
        let (input_event_sender, _input_event_receiver) = futures::channel::mpsc::unbounded();
        let bindings: InputDeviceBindingMap = Arc::new(Mutex::new(SortedVecMap::new()));

        let device_listener_registry = DeviceListenerRegistry::new();
        let (listener_client, listener_server) =
            fidl_next::fuchsia::create_channel::<fidl_next_fuchsia_ui_input::DeviceListener>();
        let listener_client = listener_client.spawn();
        let _existing_devices = device_listener_registry.add_listener(listener_client);

        let (event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();
        let _server_task = listener_server.spawn(MockDeviceListener { event_sender });

        // Handle input device requests.
        let _task = fasync::Task::local(async move {
            // Register a device.
            let _ = input_device_registry_proxy.register(input_device_client_end);

            let mut count = 0;
            while count < 2 {
                if let Some(input_device_request) =
                    input_device_request_stream.try_next().await.unwrap()
                {
                    handle_input_device_request(input_device_request);
                    count += 1;
                } else {
                    break;
                }
            }

            // End handle_input_device_registry_request_stream() by taking the event stream.
            input_device_registry_proxy.take_event_stream();
        });

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");

        // Start listening for InputDeviceRegistryRequests.
        let bindings_clone = bindings.clone();
        let device_listener_registry_clone = device_listener_registry.clone();
        let (task_sender, mut task_receiver) = futures::channel::mpsc::unbounded();
        let mut tasks = futures::stream::FuturesUnordered::new();
        let registry_fut = InputPipeline::handle_input_device_registry_request_stream(
            input_device_registry_request_stream,
            &device_types,
            &input_event_sender,
            &bindings_clone,
            &test_node,
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
            device_listener_registry_clone,
            task_sender,
            None,
        )
        .fuse();

        let mut registry_fut = std::pin::pin!(registry_fut);
        loop {
            futures::select! {
                res = registry_fut => {
                    res.unwrap();
                    break;
                }
                task = task_receiver.next() => {
                    if let Some(task) = task {
                        tasks.push(task);
                    }
                }
                _ = tasks.select_next_some() => {}
            }
        }

        // Assert that a device was registered.
        let device_id = {
            let bindings = bindings.lock();
            assert_eq!(bindings.len(), 1);
            *bindings.keys().next().unwrap()
        };

        // Assert that device listener was notified.
        if let Some(event) = event_receiver.next().await {
            assert_eq!(event.action, Some(fidl_next_fuchsia_ui_input::Action::Added));
            assert_eq!(event.device_id, Some(device_id));
        } else {
            panic!("Expected a request on listener stream");
        }
    }

    /// Tests that an injected device is removed and listeners are notified with Action::Removed
    /// when the injected device channel is closed.
    #[fasync::run_singlethreaded(test)]
    async fn handle_input_device_registry_disconnection() {
        let (input_device_registry_proxy, input_device_registry_request_stream) =
            create_proxy_and_stream::<fidl_fuchsia_input_injection::InputDeviceRegistryMarker>();
        let (input_device_client_end, mut input_device_request_stream) =
            create_request_stream::<fidl_fuchsia_input_report::InputDeviceMarker>();

        let device_types = vec![input_device::InputDeviceType::Mouse];
        let (input_event_sender, _input_event_receiver) = futures::channel::mpsc::unbounded();
        let bindings: InputDeviceBindingMap = Arc::new(Mutex::new(SortedVecMap::new()));

        let device_listener_registry = DeviceListenerRegistry::new();
        let (listener_client, listener_server) =
            fidl_next::fuchsia::create_channel::<fidl_next_fuchsia_ui_input::DeviceListener>();
        let listener_client = listener_client.spawn();
        let _existing_devices = device_listener_registry.add_listener(listener_client);

        let (event_sender, mut event_receiver) = futures::channel::mpsc::unbounded();
        let _server_task = listener_server.spawn(MockDeviceListener { event_sender });

        // Handle input device requests.
        let _task = fasync::Task::local(async move {
            // Register a device.
            let _ = input_device_registry_proxy.register(input_device_client_end);

            let mut count = 0;
            while count < 2 {
                if let Some(input_device_request) =
                    input_device_request_stream.try_next().await.unwrap()
                {
                    handle_input_device_request(input_device_request);
                    count += 1;
                } else {
                    break;
                }
            }

            // Close the device channel by dropping the request stream.
            std::mem::drop(input_device_request_stream);

            // Wait a bit to let the local task run.
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(100),
            ))
            .await;

            // End handle_input_device_registry_request_stream() by taking the event stream.
            input_device_registry_proxy.take_event_stream();
        });

        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");

        // Start listening for InputDeviceRegistryRequests.
        let bindings_clone = bindings.clone();
        let device_listener_registry_clone = device_listener_registry.clone();
        let (task_sender, mut task_receiver) = futures::channel::mpsc::unbounded();
        let mut tasks = futures::stream::FuturesUnordered::new();
        let registry_fut = InputPipeline::handle_input_device_registry_request_stream(
            input_device_registry_request_stream,
            &device_types,
            &input_event_sender,
            &bindings_clone,
            &test_node,
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
            device_listener_registry_clone,
            task_sender,
            None,
        )
        .fuse();

        let mut registry_fut = std::pin::pin!(registry_fut);
        loop {
            futures::select! {
                res = registry_fut => {
                    res.unwrap();
                    break;
                }
                task = task_receiver.next() => {
                    if let Some(task) = task {
                        tasks.push(task);
                    }
                }
                _ = tasks.select_next_some() => {}
            }
        }
        while let Some(_) = tasks.next().await {}

        // Assert that the device was registered and then removed.
        {
            let bindings = bindings.lock();
            assert_eq!(bindings.len(), 0);
        }

        // Assert that device listener was notified of Addition.
        let event_added = if let Some(event) = event_receiver.next().await {
            event
        } else {
            panic!("Expected Added event");
        };
        assert_eq!(event_added.action, Some(fidl_next_fuchsia_ui_input::Action::Added));

        // Assert that device listener was notified of Removal.
        let event_removed = if let Some(event) = event_receiver.next().await {
            event
        } else {
            panic!("Expected Removed event");
        };
        assert_eq!(event_removed.action, Some(fidl_next_fuchsia_ui_input::Action::Removed));
    }

    // Tests that correct properties are added to inspect node when InputPipeline is created.
    #[fuchsia::test]
    async fn check_inspect_node_has_correct_properties() {
        let device_types = vec![
            input_device::InputDeviceType::Touch,
            input_device::InputDeviceType::ConsumerControls,
        ];
        let inspector = fuchsia_inspect::Inspector::default();
        let test_node = inspector.root().create_child("input_pipeline");
        // Create fake input handler for assembly
        let (fake_handler_event_sender, _fake_handler_event_receiver) =
            futures::channel::mpsc::channel(100);
        let fake_input_handler =
            observe_fake_events_input_handler::ObserveFakeEventsInputHandler::new(
                fake_handler_event_sender,
            );
        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_handler(fake_input_handler);
        let _test_input_pipeline = InputPipeline::new(
            &Incoming::new(),
            device_types,
            assembly,
            test_node,
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        );
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_pipeline: {
                supported_input_devices: "Touch, ConsumerControls",
                handlers_registered: 1u64,
                handlers_healthy: 1u64,
                input_devices: {}
            }
        });
    }

    struct SpecificInterestFakeHandler {
        interest_types: Vec<input_device::InputEventType>,
        event_sender: std::cell::RefCell<futures::channel::mpsc::Sender<input_device::InputEvent>>,
    }

    impl SpecificInterestFakeHandler {
        pub fn new(
            interest_types: Vec<input_device::InputEventType>,
            event_sender: futures::channel::mpsc::Sender<input_device::InputEvent>,
        ) -> Rc<Self> {
            Rc::new(SpecificInterestFakeHandler {
                interest_types,
                event_sender: std::cell::RefCell::new(event_sender),
            })
        }
    }

    impl Handler for SpecificInterestFakeHandler {
        fn set_handler_healthy(self: std::rc::Rc<Self>) {}
        fn set_handler_unhealthy(self: std::rc::Rc<Self>, _msg: &str) {}
        fn get_name(&self) -> &'static str {
            "SpecificInterestFakeHandler"
        }

        fn interest(&self) -> Vec<input_device::InputEventType> {
            self.interest_types.clone()
        }
    }

    #[async_trait(?Send)]
    impl input_handler::InputHandler for SpecificInterestFakeHandler {
        async fn handle_input_event(
            self: Rc<Self>,
            input_event: input_device::InputEvent,
        ) -> Vec<input_device::InputEvent> {
            match self.event_sender.borrow_mut().try_send(input_event.clone()) {
                Err(e) => panic!("SpecificInterestFakeHandler failed to send event: {:?}", e),
                Ok(_) => {}
            }
            vec![input_event]
        }
    }

    #[fuchsia::test]
    async fn run_only_sends_events_to_interested_handlers() {
        // Mouse Handler (Specific Interest: Mouse)
        let (mouse_sender, mut mouse_receiver) = futures::channel::mpsc::channel(1);
        let mouse_handler =
            SpecificInterestFakeHandler::new(vec![InputEventType::Mouse], mouse_sender);

        // Fake Handler (Specific Interest: Fake)
        let (fake_sender, mut fake_receiver) = futures::channel::mpsc::channel(1);
        let fake_handler =
            SpecificInterestFakeHandler::new(vec![InputEventType::Fake], fake_sender);

        let (pipeline_sender, pipeline_receiver, handlers, _, _, _, _) =
            InputPipelineAssembly::new(metrics::MetricsLogger::default())
                .add_handler(mouse_handler)
                .add_handler(fake_handler)
                .into_components();

        // Run the pipeline logic
        let _runner_task = fasync::Task::local(InputPipeline::run(
            pipeline_receiver,
            handlers,
            metrics::MetricsLogger::default(),
        ));

        // Create a Fake event
        let fake_event = input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Fake,
            device_descriptor: input_device::InputDeviceDescriptor::Fake,
            event_time: zx::MonotonicInstant::get(),
            handled: input_device::Handled::No,
            trace_id: None,
        };

        // Send the Fake event
        pipeline_sender.unbounded_send(vec![fake_event.clone()]).expect("failed to send event");

        // Verify Fake Handler received it
        let received_by_fake = fake_receiver.next().await;
        assert_eq!(received_by_fake, Some(fake_event));

        // Verify Mouse Handler did NOT receive it
        assert!(mouse_receiver.try_recv().is_err());
    }

    fn create_mouse_event(x: f32, y: f32) -> input_device::InputEvent {
        input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Mouse(mouse_binding::MouseEvent::new(
                mouse_binding::MouseLocation::Relative(mouse_binding::RelativeLocation {
                    counts: Position { x, y },
                }),
                None,
                None,
                mouse_binding::MousePhase::Move,
                SortedVecSet::new(),
                SortedVecSet::new(),
                None,
                None,
            )),
            device_descriptor: input_device::InputDeviceDescriptor::Mouse(
                mouse_binding::MouseDeviceDescriptor {
                    device_id: 1,
                    absolute_x_range: None,
                    absolute_y_range: None,
                    wheel_v_range: None,
                    wheel_h_range: None,
                    buttons: None,
                },
            ),
            event_time: zx::MonotonicInstant::get(),
            handled: input_device::Handled::No,
            trace_id: None,
        }
    }

    #[fuchsia::test]
    async fn run_mixed_event_types_dispatched_correctly() {
        // Mouse Handler (Specific Interest: Mouse)
        let (mouse_sender, mut mouse_receiver) = futures::channel::mpsc::channel(10);
        let mouse_handler =
            SpecificInterestFakeHandler::new(vec![InputEventType::Mouse], mouse_sender);

        // Fake Handler (Specific Interest: Fake)
        let (fake_sender, mut fake_receiver) = futures::channel::mpsc::channel(10);
        let fake_handler =
            SpecificInterestFakeHandler::new(vec![InputEventType::Fake], fake_sender);

        let (pipeline_sender, pipeline_receiver, handlers, _, _, _, _) =
            InputPipelineAssembly::new(metrics::MetricsLogger::default())
                .add_handler(mouse_handler)
                .add_handler(fake_handler)
                .into_components();

        // Run the pipeline logic
        let _runner_task = fasync::Task::local(InputPipeline::run(
            pipeline_receiver,
            handlers,
            metrics::MetricsLogger::default(),
        ));

        // Create events
        let mouse_event_1 = create_mouse_event(1.0, 1.0);
        let mouse_event_2 = create_mouse_event(2.0, 2.0);
        let mouse_event_3 = create_mouse_event(3.0, 3.0);

        let fake_event_1 = input_device::InputEvent {
            device_event: input_device::InputDeviceEvent::Fake,
            device_descriptor: input_device::InputDeviceDescriptor::Fake,
            event_time: zx::MonotonicInstant::get(),
            handled: input_device::Handled::No,
            trace_id: None,
        };

        // Send mixed batch: [Mouse, Mouse, Fake, Mouse]
        // This should result in 3 chunks: [Mouse, Mouse], [Fake], [Mouse]
        let mixed_batch = vec![
            mouse_event_1.clone(),
            mouse_event_2.clone(),
            fake_event_1.clone(),
            mouse_event_3.clone(),
        ];
        pipeline_sender.unbounded_send(mixed_batch).expect("failed to send events");

        // Verify Mouse Handler received M1, M2, and then M3
        assert_eq!(mouse_receiver.next().await, Some(mouse_event_1));
        assert_eq!(mouse_receiver.next().await, Some(mouse_event_2));
        assert_eq!(mouse_receiver.next().await, Some(mouse_event_3));

        // Verify Fake Handler received F1
        assert_eq!(fake_receiver.next().await, Some(fake_event_1));
    }

    #[fuchsia::test]
    async fn ownership_transition_pauses_and_resumes_device_readers() {
        use crate::display_ownership;
        use zx::Peered as _;

        let readers = Arc::new(Mutex::new(Vec::new()));
        let readers_clone = readers.clone();
        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_0" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let readers = readers_clone.clone();
                            async move {
                                while let Ok(Some(input_device_request)) =
                                    request_stream.try_next().await
                                {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            readers.lock().push(reader);
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: platform owns both display and input.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![input_device::InputDeviceType::Mouse],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();

        // Run the pipeline tasks in the background.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until initial device is discovered and bound.
        for _ in 0..50 {
            if bindings.lock().len() == 1 {
                break;
            }
            fasync::Timer::new(fasync::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(10),
            ))
            .await;
        }
        assert_eq!(bindings.lock().len(), 1);

        // 1. Lose display ownership: DISPLAY_PLATFORM_OWNED -> DISPLAY_UNOWNED.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");
        assert!(bindings.lock().is_empty());

        // 2. Regain display ownership: DISPLAY_UNOWNED -> DISPLAY_PLATFORM_OWNED.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");
        assert_eq!(bindings.lock().len(), 1);

        // 3. Lose input ownership: INPUT_PLATFORM_OWNED -> INPUT_CLIENT_OWNED.
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");
        assert!(bindings.lock().is_empty());

        // 4. Regain input ownership: INPUT_CLIENT_OWNED -> INPUT_PLATFORM_OWNED.
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");
        assert_eq!(bindings.lock().len(), 1);

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn device_discovered_while_unowned_is_not_bound_until_ownership_granted() {
        use crate::display_ownership;
        use zx::Peered as _;

        let readers = Arc::new(Mutex::new(Vec::new()));
        let readers_clone = readers.clone();
        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_0" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let readers = readers_clone.clone();
                            async move {
                                while let Ok(Some(input_device_request)) =
                                    request_stream.try_next().await
                                {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            readers.lock().push(reader);
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: platform does NOT own display.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_UNOWNED | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![input_device::InputDeviceType::Mouse],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();

        // Run the pipeline tasks in the background.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait a bit to ensure watcher has run and attempted to bind.
        fasync::Timer::new(fasync::MonotonicInstant::after(zx::MonotonicDuration::from_millis(50)))
            .await;

        // Device should NOT be bound because ownership was lost.
        assert!(bindings.lock().is_empty());

        // Regain display ownership: DISPLAY_UNOWNED -> DISPLAY_PLATFORM_OWNED.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Device should now be re-enumerated and bound.
        assert_eq!(bindings.lock().len(), 1);

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn client_input_ownership_preserves_consumer_controls_and_drops_mouse() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (mouse_reader_sender, mut mouse_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (button_reader_sender, mut button_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let mouse_reader_sender = mouse_reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            mouse_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                },
                "instance_buttons" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let button_reader_sender = button_reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            button_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_consumer_controls_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: platform owns both display and input.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();

        // Run the pipeline tasks in the background.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Deterministically wait for both readers to connect.
        let mouse_reader = mouse_reader_receiver.next().await.expect("mouse reader created");
        let mouse_channel = mouse_reader.into_channel();
        let button_reader = button_reader_receiver.next().await.expect("button reader created");
        let button_channel = button_reader.into_channel();

        let button_device_id = {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 2);
            guard
                .iter()
                .find(|(_, bts)| {
                    matches!(
                        bts[0].binding.get_device_descriptor(),
                        input_device::InputDeviceDescriptor::ConsumerControls(_)
                    )
                })
                .map(|(id, _)| *id)
                .unwrap()
        };

        // 1. Client takes input ownership (INPUT_PLATFORM_OWNED -> INPUT_CLIENT_OWNED).
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Mouse reader channel observes CHANNEL_PEER_CLOSED.
        fasync::OnSignals::new(&mouse_channel, zx::Signals::CHANNEL_PEER_CLOSED)
            .await
            .expect("mouse reader peer closed");

        // Zero spurious GetInputReportsReaderV2 calls on Mouse during ClientInputOwned.
        assert!(mouse_reader_receiver.try_recv().is_err());

        // ConsumerControls reader channel remains open and uninterrupted.
        let sigs = button_channel
            .wait_one(zx::Signals::CHANNEL_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST);
        assert!(matches!(sigs, zx::WaitResult::TimedOut(_)));

        // ConsumerControls binding retained under the same device_id; Mouse binding dropped.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            assert!(guard.contains_key(&button_device_id));
            let remaining = guard.get(&button_device_id).unwrap();
            assert!(matches!(
                remaining[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        // 2. Restore platform input ownership (INPUT_CLIENT_OWNED -> INPUT_PLATFORM_OWNED).
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Mouse receives a new reader.
        let _new_mouse_reader =
            mouse_reader_receiver.next().await.expect("new mouse reader on restore");

        // Zero duplicate GetInputReportsReaderV2 requests on ConsumerControls.
        assert!(button_reader_receiver.try_recv().is_err());

        // Same button_device_id retained.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 2);
            assert!(guard.contains_key(&button_device_id));
        }

        // 3. Lose display ownership to Virtcon (DISPLAY_PLATFORM_OWNED -> DISPLAY_UNOWNED).
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // All bindings dropped.
        assert!(bindings.lock().is_empty());

        // ConsumerControls reader now closes.
        fasync::OnSignals::new(&button_channel, zx::Signals::CHANNEL_PEER_CLOSED)
            .await
            .expect("button reader peer closed");

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn device_discovered_while_client_input_owned_binds_consumer_controls_only() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (mouse_reader_sender, mut mouse_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (mouse_probed_sender, mut mouse_probed_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (button_reader_sender, mut button_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let mouse_reader_sender = mouse_reader_sender.clone();
                            let mouse_probed_sender = mouse_probed_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            mouse_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                                let _ = mouse_probed_sender.unbounded_send(());
                            }.boxed()
                        },
                    )
                },
                "instance_buttons" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let button_reader_sender = button_reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            button_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_consumer_controls_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: Display is platform owned, but input is client owned.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();

        // Run the pipeline tasks in the background.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // ConsumerControls is bound immediately.
        let _button_reader = button_reader_receiver.next().await.expect("button reader connected");

        // Mouse should NOT be bound while ClientInputOwned.
        assert!(mouse_reader_receiver.try_recv().is_err());
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let remaining = guard.values().next().unwrap();
            assert!(matches!(
                remaining[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        mouse_probed_receiver.next().await.expect("mouse probe finished");

        // Verify Inspect: devices_discovered is 1 (for buttons), devices_connected is 1 (for buttons).
        // No unsupported node for instance_mouse.
        diagnostics_assertions::assert_data_tree!(inspector, root: contains {
            input_pipeline: contains {
                supported_input_devices: "Mouse, ConsumerControls",
                handlers_registered: 2u64,
                handlers_healthy: 2u64,
                input_devices: {
                    devices_discovered: 1u64,
                    devices_connected: 1u64,
                    "instance_buttons_ConsumerControls": contains {
                        "fuchsia.inspect.Health": contains {
                            status: "OK",
                        }
                    }
                }
            }
        });

        // Restore platform input ownership (INPUT_CLIENT_OWNED -> INPUT_PLATFORM_OWNED).
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Now Mouse is discovered and bound.
        let _mouse_reader = mouse_reader_receiver.next().await.expect("mouse reader connected");
        assert!(button_reader_receiver.try_recv().is_err());
        assert_eq!(bindings.lock().len(), 2);

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn transition_from_display_unowned_to_client_input_owned_rebinds_consumer_controls() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (mouse_reader_sender, mut mouse_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (button_reader_sender, mut button_reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let mouse_reader_sender = mouse_reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            mouse_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                },
                "instance_buttons" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let button_reader_sender = button_reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            button_reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_consumer_controls_device_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: Virtcon display unowned with input client owned.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_UNOWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();

        // Run the pipeline tasks in the background.
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // In DisplayUnowned, all bindings must be empty.
        assert!(bindings.lock().is_empty());

        // Regain display ownership while input is still client owned:
        // DISPLAY_UNOWNED -> DISPLAY_PLATFORM_OWNED.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // ConsumerControls should be re-enumerated and bound; Mouse remains unbound.
        let _button_reader = button_reader_receiver.next().await.expect("button reader connected");
        assert!(mouse_reader_receiver.try_recv().is_err());
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let remaining = guard.values().next().unwrap();
            assert!(matches!(
                remaining[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn composite_device_preserves_consumer_controls_and_rebinds_mouse() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: PlatformOwned.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // In PlatformOwned, composite device creates Mouse reader and ConsumerControls reader.
        let mouse_reader = reader_receiver.next().await.expect("mouse reader connected");
        let mouse_channel = mouse_reader.into_channel();
        let button_reader = reader_receiver.next().await.expect("button reader connected");
        let button_channel = button_reader.into_channel();

        // Both bindings exist under a single device id initially.
        let initial_device_id = {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (&id, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 2);
            id
        };

        // 1. Client takes input ownership (PlatformOwned -> ClientInputOwned).
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Non-button reader closes (CHANNEL_PEER_CLOSED).
        fasync::OnSignals::new(&mouse_channel, zx::Signals::CHANNEL_PEER_CLOSED)
            .await
            .expect("mouse reader closed on client input ownership");

        // ConsumerControls reader remains open.
        let sigs = button_channel
            .wait_one(zx::Signals::CHANNEL_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST);
        assert!(matches!(sigs, zx::WaitResult::TimedOut(_)));

        // ConsumerControls binding retained under initial_device_id.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let bts = guard.get(&initial_device_id).unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        // 2. Restore platform input ownership (ClientInputOwned -> PlatformOwned).
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Re-enumeration binds the missing Mouse capability.
        let _new_mouse_reader =
            reader_receiver.next().await.expect("new mouse reader connected on restore");

        // ConsumerControls reader was never duplicated.
        assert!(reader_receiver.try_recv().is_err());

        // ConsumerControls remains active on initial_device_id; Mouse is bound under the same initial_device_id.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let bts = guard.get(&initial_device_id).unwrap();
            assert_eq!(bts.len(), 2);
        }

        // 3. Switch back to ClientInputOwned again.
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // New mouse reader closes; ConsumerControls still retained.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let bts = guard.get(&initial_device_id).unwrap();
            assert_eq!(bts.len(), 1);
        }

        // 4. Lose display ownership to Virtcon (DISPLAY_PLATFORM_OWNED -> DISPLAY_UNOWNED).
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        assert!(bindings.lock().is_empty());
        fasync::OnSignals::new(&button_channel, zx::Signals::CHANNEL_PEER_CLOSED)
            .await
            .expect("button reader closed on DisplayUnowned");

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn composite_device_discovered_while_client_input_owned_binds_missing_on_platform_owned()
    {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        // Initial state: ClientInputOwned.
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // In ClientInputOwned, discovering composite device binds ConsumerControls only.
        let _button_reader =
            reader_receiver.next().await.expect("button reader connected in ClientInputOwned");
        assert!(reader_receiver.try_recv().is_err());
        let initial_device_id = {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (&id, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
            id
        };

        // Restore PlatformOwned: missing Mouse is discovered and bound.
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _mouse_reader =
            reader_receiver.next().await.expect("mouse reader connected on restore");
        assert!(reader_receiver.try_recv().is_err());
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let bts = guard.get(&initial_device_id).unwrap();
            assert_eq!(bts.len(), 2);
        }

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_descriptor_transition_to_display_unowned_drops_bindings() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (stream_closed_sender, mut stream_closed_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            let stream_closed_sender = stream_closed_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            desc_started_sender.unbounded_send(()).unwrap();
                                            if let Some(rx) = resume_desc_receiver.lock().await.take() {
                                                let _ = rx.await;
                                            }
                                            let _ = responder.send(&fidl_fuchsia_input_report::DeviceDescriptor {
                                                device_information: None,
                                                mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                                                    input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                                                        movement_x: None,
                                                        movement_y: None,
                                                        scroll_v: None,
                                                        scroll_h: None,
                                                        buttons: Some(vec![0]),
                                                        position_x: None,
                                                        position_y: None,
                                                        ..Default::default()
                                                    }),
                                                    ..Default::default()
                                                }),
                                                sensor: None,
                                                touch: None,
                                                keyboard: None,
                                                consumer_control: None,
                                                ..Default::default()
                                            });
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                                let _ = stream_closed_sender.unbounded_send(());
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![input_device::InputDeviceType::Mouse],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until connect_and_bind_device is suspended inside get_descriptor().await.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to DisplayUnowned while get_descriptor is in-flight.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Resume get_descriptor responder.
        resume_desc_sender.send(()).unwrap();

        // Await server request stream termination to confirm post-await race guard dropped the client connection.
        stream_closed_receiver.next().await.expect("device client disconnected");
        assert!(reader_receiver.try_recv().is_err());
        assert!(bindings.lock().is_empty());

        // Transition back to PlatformOwned: device should bind cleanly.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _new_reader = reader_receiver.next().await.expect("mouse reader connected on restore");
        assert_eq!(bindings.lock().len(), 1);

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_descriptor_transition_to_client_input_owned_retains_consumer_controls() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            desc_started_sender.unbounded_send(()).unwrap();
                                            if let Some(rx) = resume_desc_receiver.lock().await.take() {
                                                let _ = rx.await;
                                            }
                                            handle_composite_mouse_and_consumer_controls_request(
                                                fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder },
                                            );
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until connect_and_bind_device is suspended inside get_descriptor().await.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to ClientInputOwned while get_descriptor is in-flight.
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Resume get_descriptor responder.
        resume_desc_sender.send(()).unwrap();

        // In the composite device, add_device_bindings binds Mouse (spawning its reader task while
        // awaiting ConsumerControls' descriptor), then ConsumerControls.
        let mouse_reader = reader_receiver.next().await.expect("mouse reader connected");
        let mouse_channel = mouse_reader.into_channel();
        let button_reader = reader_receiver.next().await.expect("button reader connected");
        let button_channel = button_reader.into_channel();

        // Mouse reader closes when dropped by post-await race guard; button reader remains open.
        fasync::OnSignals::new(&mouse_channel, zx::Signals::CHANNEL_PEER_CLOSED)
            .await
            .expect("mouse reader closed on ClientInputOwned");
        let sigs = button_channel
            .wait_one(zx::Signals::CHANNEL_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST);
        assert!(matches!(sigs, zx::WaitResult::TimedOut(_)));

        let initial_device_id = {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (&id, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
            id
        };

        // Transition back to PlatformOwned: Mouse should be bound without duplicating ConsumerControls.
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _new_mouse_reader =
            reader_receiver.next().await.expect("new mouse reader connected on restore");
        assert!(reader_receiver.try_recv().is_err());

        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let bts = guard.get(&initial_device_id).unwrap();
            assert_eq!(bts.len(), 2);
        }

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_descriptor_transition_to_client_input_owned_drops_standalone_mouse() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (stream_closed_sender, mut stream_closed_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            let stream_closed_sender = stream_closed_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            desc_started_sender.unbounded_send(()).unwrap();
                                            if let Some(rx) = resume_desc_receiver.lock().await.take() {
                                                let _ = rx.await;
                                            }
                                            let _ = responder.send(&fidl_fuchsia_input_report::DeviceDescriptor {
                                                device_information: None,
                                                mouse: Some(fidl_fuchsia_input_report::MouseDescriptor {
                                                    input: Some(fidl_fuchsia_input_report::MouseInputDescriptor {
                                                        movement_x: None,
                                                        movement_y: None,
                                                        scroll_v: None,
                                                        scroll_h: None,
                                                        buttons: Some(vec![0]),
                                                        position_x: None,
                                                        position_y: None,
                                                        ..Default::default()
                                                    }),
                                                    ..Default::default()
                                                }),
                                                sensor: None,
                                                touch: None,
                                                keyboard: None,
                                                consumer_control: None,
                                                ..Default::default()
                                            });
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_input_device_request(req),
                                    }
                                }
                                let _ = stream_closed_sender.unbounded_send(());
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![input_device::InputDeviceType::Mouse],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until connect_and_bind_device is suspended inside get_descriptor().await.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to ClientInputOwned while get_descriptor is in-flight.
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Resume get_descriptor responder.
        resume_desc_sender.send(()).unwrap();

        // Await server request stream termination to confirm post-await race guard (kept.is_empty()) dropped the client connection.
        stream_closed_receiver.next().await.expect("device client disconnected");
        assert!(reader_receiver.try_recv().is_err());
        assert!(bindings.lock().is_empty());

        // Transition back to PlatformOwned: standalone mouse should rebind cleanly.
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _new_reader = reader_receiver.next().await.expect("mouse reader connected on restore");
        assert_eq!(bindings.lock().len(), 1);

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_descriptor_transition_to_client_input_owned_drops_duplicate_consumer_controls()
     {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            let rx_opt = resume_desc_receiver.lock().await.take();
                                            if let Some(rx) = rx_opt {
                                                desc_started_sender.unbounded_send(()).unwrap();
                                                let _ = rx.await;
                                            }
                                            handle_composite_mouse_and_consumer_controls_request(
                                                fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder },
                                            );
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until connect_and_bind_device is suspended inside get_descriptor().await.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to DisplayUnowned while get_descriptor is in-flight.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Transition from DisplayUnowned to ClientInputOwned: triggers re_enumerate_devices
        // to bind ConsumerControls.
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _first_button_reader =
            reader_receiver.next().await.expect("button reader connected from re-enumeration");

        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (_, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        // Resume the initial get_descriptor responder.
        resume_desc_sender.send(()).unwrap();

        // The completing in-flight bind should detect the duplicate ConsumerControls binding in
        // ClientInputOwned and drop its redundant tasks without adding a duplicate reader.
        fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_millis(100))).await;

        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (_, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::ConsumerControls(_)
            ));
        }

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_descriptor_transition_to_platform_owned_merges_complementary_bindings() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            let rx_opt = resume_desc_receiver.lock().await.take();
                                            if let Some(rx) = rx_opt {
                                                desc_started_sender.unbounded_send(()).unwrap();
                                                let _ = rx.await;
                                            }
                                            handle_composite_mouse_and_consumer_controls_request(
                                                fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder },
                                            );
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // In ClientInputOwned, the initial discovery only binds ConsumerControls.
        // Wait until connect_and_bind_device is suspended inside get_descriptor().await.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to PlatformOwned while get_descriptor is in-flight.
        // This triggers re_enumerate_devices to bind the remaining supported types (Mouse).
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        // Resume get_descriptor for the ConsumerControls binding so both in-flight tasks can complete.
        resume_desc_sender.send(()).unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _reader1 = reader_receiver.next().await.expect("first reader connected");
        let _reader2 = reader_receiver.next().await.expect("second reader connected");

        // Both Mouse and ConsumerControls should be preserved in bindings.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (_, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 2);
            let has_mouse = bts.iter().any(|bt| {
                matches!(
                    bt.binding.get_device_descriptor(),
                    input_device::InputDeviceDescriptor::Mouse(_)
                )
            });
            let has_consumer_controls = bts.iter().any(|bt| {
                matches!(
                    bt.binding.get_device_descriptor(),
                    input_device::InputDeviceDescriptor::ConsumerControls(_)
                )
            });
            assert!(has_mouse);
            assert!(has_consumer_controls);
        }

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn in_flight_reservation_preserved_across_client_input_owned_transition_and_re_enumeration()
     {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (desc_started_sender, mut desc_started_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (resume_desc_sender, resume_desc_receiver) = futures::channel::oneshot::channel::<()>();
        let resume_desc_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(resume_desc_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_composite" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_sender = desc_started_sender.clone();
                            let resume_desc_receiver = resume_desc_receiver.clone();
                            let reader_sender = reader_sender.clone();
                            async move {
                                while let Ok(Some(input_device_request)) = request_stream.try_next().await {
                                    match input_device_request {
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                            let rx_opt = resume_desc_receiver.lock().await.take();
                                            if let Some(rx) = rx_opt {
                                                desc_started_sender.unbounded_send(()).unwrap();
                                                let _ = rx.await;
                                            }
                                            handle_composite_mouse_and_consumer_controls_request(
                                                fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder },
                                            );
                                        }
                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                            reader,
                                            responder,
                                            ..
                                        } => {
                                            let _ = responder.send(2);
                                            reader_sender.unbounded_send(reader).unwrap();
                                        }
                                        req => handle_composite_mouse_and_consumer_controls_request(req),
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // Wait until connect_and_bind_device is suspended inside get_descriptor().await while in PlatformOwned.
        desc_started_receiver.next().await.expect("GetDescriptor called");

        // Transition to ClientInputOwned while get_descriptor is in-flight.
        // This must preserve the in-flight ConsumerControls reservation in bound_instances.
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Transition to PlatformOwned while the original get_descriptor is still in-flight.
        // This triggers re_enumerate_devices. Because the ConsumerControls reservation was preserved
        // across ClientInputOwned, re_enumerate_devices does not duplicate the ConsumerControls reader.
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        // Resume get_descriptor so the in-flight task can complete.
        resume_desc_sender.send(()).unwrap();
        loop_done.next().await.expect("loop_done notification");

        let _reader1 = reader_receiver.next().await.expect("first reader connected");
        let _reader2 = reader_receiver.next().await.expect("second reader connected");

        // Verify exactly one ConsumerControls and one Mouse binding exist (no duplicates).
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (_, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 2);
            let mouse_count = bts
                .iter()
                .filter(|bt| {
                    matches!(
                        bt.binding.get_device_descriptor(),
                        input_device::InputDeviceDescriptor::Mouse(_)
                    )
                })
                .count();
            let consumer_controls_count = bts
                .iter()
                .filter(|bt| {
                    matches!(
                        bt.binding.get_device_descriptor(),
                        input_device::InputDeviceDescriptor::ConsumerControls(_)
                    )
                })
                .count();
            assert_eq!(mouse_count, 1);
            assert_eq!(consumer_controls_count, 1);
        }

        // Verify no additional readers were requested.
        assert!(reader_receiver.try_recv().is_err());

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn client_input_owned_non_button_probe_keeps_client_alive_for_platform_owned() {
        use crate::display_ownership;
        use zx::Peered as _;

        let (reader_sender, mut reader_receiver) = futures::channel::mpsc::unbounded::<
            fidl::endpoints::ServerEnd<fidl_fuchsia_input_report::InputReportsReaderV2Marker>,
        >();
        let (desc_started_a_sender, mut desc_started_a_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let (desc_started_b_sender, mut desc_started_b_receiver) =
            futures::channel::mpsc::unbounded::<()>();
        let first_responder = Arc::new(Mutex::new(None));
        let first_responder_clone = first_responder.clone();
        let first_intercepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first_intercepted_clone = first_intercepted.clone();
        let desc_b_intercepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let desc_b_intercepted_clone = desc_b_intercepted.clone();
        let (disconnect_sender, disconnect_receiver) = futures::channel::oneshot::channel::<()>();
        let disconnect_receiver =
            std::sync::Arc::new(futures::lock::Mutex::new(Some(disconnect_receiver)));

        let dir = pseudo_directory! {
            "fuchsia.input.report.Service" => pseudo_directory! {
                "instance_mouse" => pseudo_directory! {
                    "input_device" => pseudo_fs_service::host(
                        move |mut request_stream: fidl_fuchsia_input_report::InputDeviceRequestStream| {
                            let desc_started_a_sender = desc_started_a_sender.clone();
                            let desc_started_b_sender = desc_started_b_sender.clone();
                            let first_responder = first_responder_clone.clone();
                            let first_intercepted = first_intercepted_clone.clone();
                            let desc_b_intercepted = desc_b_intercepted_clone.clone();
                            let reader_sender = reader_sender.clone();
                            let disconnect_receiver = disconnect_receiver.clone();
                            async move {
                                let mut disconnect_rx = disconnect_receiver.lock().await.take();
                                let mut disconnect_fut = std::pin::pin!(async {
                                    if let Some(rx) = disconnect_rx.take() {
                                        let _ = rx.await;
                                    } else {
                                        futures::future::pending::<()>().await;
                                    }
                                }.fuse());
                                loop {
                                    futures::select! {
                                        _ = disconnect_fut => {
                                            break;
                                        }
                                        request = request_stream.try_next() => {
                                            match request {
                                                Ok(Some(input_device_request)) => {
                                                    match input_device_request {
                                                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                                                            if !first_intercepted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                                                *first_responder.lock() = Some(responder);
                                                                desc_started_a_sender.unbounded_send(()).unwrap();
                                                            } else {
                                                                if !desc_b_intercepted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                                                                    desc_started_b_sender.unbounded_send(()).unwrap();
                                                                }
                                                                handle_input_device_request(
                                                                    fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder },
                                                                );
                                                            }
                                                        }
                                                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                                                            reader,
                                                            responder,
                                                            ..
                                                        } => {
                                                            let _ = responder.send(2);
                                                            reader_sender.unbounded_send(reader).unwrap();
                                                        }
                                                        req => handle_input_device_request(req),
                                                    }
                                                }
                                                _ => break,
                                            }
                                        }
                                    }
                                }
                            }.boxed()
                        },
                    )
                }
            }
        };

        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(
            &svc_proxy,
            "fuchsia.input.report.Service",
            fio::PERM_READABLE,
        )
        .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED | *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        // 1. In ClientInputOwned, the initial discovery probes for ConsumerControls (Task A).
        // Wait until connect_and_bind_device is suspended inside GetDescriptor().
        desc_started_a_receiver.next().await.expect("GetDescriptor A called");

        // 2. Transition to PlatformOwned while GetDescriptor A is still in-flight.
        // This triggers re_enumerate_devices, which reuses the in-flight mouse device entry (Task B).
        test_event
            .signal_peer(
                *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        // 3. Wait until Task B enters connect_and_bind_device, reuses inflight_devices, and calls GetDescriptor.
        desc_started_b_receiver.next().await.expect("GetDescriptor B called");

        // 4. Now answer GetDescriptor A:
        // - Task A completes and returns None (pure mouse has no buttons/ConsumerControls).
        // - Dropping Task A's guard decrements active_count from 2 to 1.
        // - Crucially: inflight_devices entry is NOT removed and join_handle is NOT dropped/aborted!
        let responder_a = first_responder.lock().take().expect("first responder stored");
        handle_input_device_request(fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor {
            responder: responder_a,
        });

        // 5. Task B in PlatformOwned completes re-enumeration, binds Mouse, claims join_handle, and spawns watch_task.
        loop_done.next().await.expect("loop_done notification");

        let _mouse_reader = reader_receiver.next().await.expect("mouse reader connected");

        // Verify the Mouse device is successfully bound.
        {
            let guard = bindings.lock();
            assert_eq!(guard.len(), 1);
            let (_, bts) = guard.iter().next().unwrap();
            assert_eq!(bts.len(), 1);
            assert!(matches!(
                bts[0].binding.get_device_descriptor(),
                input_device::InputDeviceDescriptor::Mouse(_)
            ));
        }

        // Trigger device disconnection by dropping the mock server end.
        disconnect_sender.send(()).unwrap();

        // Verify watch_task cleans up the device bindings upon disconnect.
        for _ in 0..50 {
            if bindings.lock().is_empty() {
                break;
            }
            fasync::Timer::new(zx::MonotonicInstant::after(zx::Duration::from_millis(10))).await;
        }
        assert!(bindings.lock().is_empty(), "device binding was not removed on disconnect");

        std::mem::drop(_pipeline_task);
    }

    #[fuchsia::test]
    async fn injected_device_reports_discarded_and_readers_persisted_across_ownership_transitions()
    {
        use crate::display_ownership;
        use zx::Peered as _;

        let dir = pseudo_directory! {};
        let svc_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let dir = fuchsia_fs::directory::open_directory_async(&svc_proxy, ".", fio::PERM_READABLE)
            .expect("open service directory");
        let service = fuchsia_component::client::Service::from_service_dir_proxy(
            dir,
            fidl_fuchsia_input_report::ServiceMarker,
        );

        let (test_event, handler_event) = zx::EventPair::create();
        test_event
            .signal_peer(
                zx::Signals::empty(),
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();

        let (loop_done_sender, mut loop_done) = futures::channel::mpsc::unbounded::<()>();
        let display_ownership = display_ownership::DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        struct ConsumingFakeHandler {
            event_sender:
                std::cell::RefCell<futures::channel::mpsc::Sender<input_device::InputEvent>>,
        }

        impl Handler for ConsumingFakeHandler {
            fn set_handler_healthy(self: std::rc::Rc<Self>) {}
            fn set_handler_unhealthy(self: std::rc::Rc<Self>, _msg: &str) {}
            fn get_name(&self) -> &'static str {
                "ConsumingFakeHandler"
            }
            fn interest(&self) -> Vec<input_device::InputEventType> {
                vec![InputEventType::Mouse, InputEventType::ConsumerControls]
            }
        }

        #[async_trait(?Send)]
        impl input_handler::InputHandler for ConsumingFakeHandler {
            async fn handle_input_event(
                self: Rc<Self>,
                input_event: input_device::InputEvent,
            ) -> Vec<input_device::InputEvent> {
                let _ = self.event_sender.borrow_mut().try_send(input_event);
                vec![]
            }
        }

        let (received_events_sender, mut received_events_receiver) =
            futures::channel::mpsc::channel(100);
        let fake_handler = Rc::new(ConsumingFakeHandler {
            event_sender: std::cell::RefCell::new(received_events_sender),
        });

        let assembly = InputPipelineAssembly::new(metrics::MetricsLogger::default())
            .add_handler(fake_handler)
            .add_display_ownership_handler(display_ownership.clone());

        let inspector = fuchsia_inspect::Inspector::default();
        let input_pipeline = InputPipeline::new_with_service(
            service,
            vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ],
            assembly,
            inspector.root().create_child("input_pipeline"),
            InputPipelineFeatureFlags { enable_merge_touch_events: false },
            metrics::MetricsLogger::default(),
        )
        .expect("failed to create input pipeline");

        let bindings = input_pipeline.input_device_bindings().clone();
        let bindings_clone = bindings.clone();
        let event_sender = input_pipeline.input_event_sender().clone();
        let feature_flags = input_pipeline.feature_flags.clone();
        let device_listener_registry = input_pipeline.device_listener_registry().clone();
        let _pipeline_task = fasync::Task::local(input_pipeline.handle_input_events());

        let (input_device_registry_proxy, input_device_registry_request_stream) =
            create_proxy_and_stream::<fidl_fuchsia_input_injection::InputDeviceRegistryMarker>();
        let (task_sender, mut task_receiver) = futures::channel::mpsc::unbounded();
        let _drain_tasks = fasync::Task::local(async move {
            let mut tasks = futures::stream::FuturesUnordered::new();
            loop {
                futures::select! {
                    t = task_receiver.next() => {
                        if let Some(t) = t {
                            tasks.push(t);
                        } else {
                            break;
                        }
                    }
                    _ = tasks.select_next_some() => {}
                }
            }
            while let Some(_) = tasks.next().await {}
        });

        let input_devices_node = inspector.root().create_child("injected_input_devices");
        let display_ownership_clone = display_ownership.clone();
        let _registry_task = fasync::Task::local(async move {
            let device_types = vec![
                input_device::InputDeviceType::Mouse,
                input_device::InputDeviceType::ConsumerControls,
            ];
            let _ = InputPipeline::handle_input_device_registry_request_stream(
                input_device_registry_request_stream,
                &device_types,
                &event_sender,
                &bindings_clone,
                &input_devices_node,
                feature_flags,
                metrics::MetricsLogger::default(),
                device_listener_registry,
                task_sender,
                Some(display_ownership_clone),
            )
            .await;
        });

        async fn register_device(
            registry_proxy: &fidl_fuchsia_input_injection::InputDeviceRegistryProxy,
            is_mouse: bool,
        ) -> (
            fidl_fuchsia_input_report::InputReportsReaderV2ControlHandle,
            futures::channel::mpsc::UnboundedReceiver<u64>,
            fasync::Task<()>,
        ) {
            let (input_device_client_end, mut input_device_request_stream) =
                create_request_stream::<fidl_fuchsia_input_report::InputDeviceMarker>();
            let (control_sender, mut control_receiver) = futures::channel::mpsc::unbounded::<
                fidl_fuchsia_input_report::InputReportsReaderV2ControlHandle,
            >();
            let (ack_sender, ack_receiver) = futures::channel::mpsc::unbounded::<u64>();

            let server_task = fasync::Task::local(async move {
                let mut reader_tasks = Vec::new();
                while let Ok(Some(req)) = input_device_request_stream.try_next().await {
                    match req {
                        fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                            if is_mouse {
                                handle_input_device_request(fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder });
                            } else {
                                handle_consumer_controls_device_request(fidl_fuchsia_input_report::InputDeviceRequest::GetDescriptor { responder });
                            }
                        }
                        fidl_fuchsia_input_report::InputDeviceRequest::GetInputReportsReaderV2 {
                            reader,
                            responder,
                            ..
                        } => {
                            let _ = responder.send(2);
                            let (mut request_stream, control_handle) =
                                reader.into_stream_and_control_handle();
                            let _ = control_sender.unbounded_send(control_handle);
                            let ack_sender = ack_sender.clone();
                            reader_tasks.push(fasync::Task::local(async move {
                                while let Some(Ok(r)) = request_stream.next().await {
                                    if let fidl_fuchsia_input_report::InputReportsReaderV2Request::AcknowledgeReports {
                                        last_acknowledged_report_stamp,
                                        ..
                                    } = r {
                                        let _ = ack_sender.unbounded_send(last_acknowledged_report_stamp);
                                    }
                                }
                            }));
                        }
                        _ => {}
                    }
                }
                while let Some(t) = reader_tasks.pop() {
                    t.await;
                }
            });

            registry_proxy.register(input_device_client_end).unwrap();
            let control_handle = control_receiver.next().await.expect("got reader control handle");
            (control_handle, ack_receiver, server_task)
        }

        // 1. Register mouse 1 and consumer controls 1 before transition (PlatformOwned).
        let (mouse1_ctrl, mut mouse1_acks, _mouse1_server) =
            register_device(&input_device_registry_proxy, true).await;
        let (button1_ctrl, mut button1_acks, _button1_server) =
            register_device(&input_device_registry_proxy, false).await;

        assert_eq!(bindings.lock().len(), 2);

        // Send mouse report: received
        mouse1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(5),
                        movement_y: Some(5),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                1,
            )
            .unwrap();
        assert_eq!(mouse1_acks.next().await, Some(1));
        let ev = received_events_receiver.next().await.expect("mouse event delivered");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::Mouse(_)));

        // Send button report: received
        button1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![
                            fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                1,
            )
            .unwrap();
        assert_eq!(button1_acks.next().await, Some(1));
        let ev = received_events_receiver.next().await.expect("button event delivered");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::ConsumerControls(_)));

        // 2. Transition to ClientInputOwned
        test_event
            .signal_peer(
                *display_ownership::INPUT_PLATFORM_OWNED,
                *display_ownership::INPUT_CLIENT_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // Both injected devices are retained!
        assert_eq!(bindings.lock().len(), 2);

        // Send mouse report on mouse 1: acknowledged, but discarded (no event emitted)!
        mouse1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(10),
                        movement_y: Some(10),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                2,
            )
            .unwrap();
        assert_eq!(mouse1_acks.next().await, Some(2));
        assert!(received_events_receiver.try_recv().is_err());

        // Send button report on button 1: delivered!
        button1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![
                            fidl_fuchsia_input_report::ConsumerControlButton::VolumeDown,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                2,
            )
            .unwrap();
        assert_eq!(button1_acks.next().await, Some(2));
        let ev =
            received_events_receiver.next().await.expect("button event delivered in client owned");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::ConsumerControls(_)));

        // 3. Register mouse 2 during ClientInputOwned
        let (mouse2_ctrl, mut mouse2_acks, _mouse2_server) =
            register_device(&input_device_registry_proxy, true).await;
        assert_eq!(bindings.lock().len(), 3);

        // Send mouse report on mouse 2: acknowledged, but discarded!
        mouse2_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(20),
                        movement_y: Some(20),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                1,
            )
            .unwrap();
        assert_eq!(mouse2_acks.next().await, Some(1));
        assert!(received_events_receiver.try_recv().is_err());

        // 4. Transition to DisplayUnowned
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_PLATFORM_OWNED,
                *display_ownership::DISPLAY_UNOWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // All 3 injected devices are retained!
        assert_eq!(bindings.lock().len(), 3);

        // Send button report on button 1: discarded in DisplayUnowned!
        button1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![
                            fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                3,
            )
            .unwrap();
        assert_eq!(button1_acks.next().await, Some(3));
        let cleanup_ev = received_events_receiver
            .next()
            .await
            .expect("held consumer controls button released on discard");
        if let input_device::InputDeviceEvent::ConsumerControls(ref cc) = cleanup_ev.device_event {
            assert!(cc.pressed_buttons.is_empty());
        } else {
            panic!("expected consumer controls release event");
        }
        assert!(received_events_receiver.try_recv().is_err());

        // Send mouse report on mouse 1: discarded!
        mouse1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(30),
                        movement_y: Some(30),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                3,
            )
            .unwrap();
        assert_eq!(mouse1_acks.next().await, Some(3));
        assert!(received_events_receiver.try_recv().is_err());

        // 5. Transition back to PlatformOwned
        test_event
            .signal_peer(
                *display_ownership::DISPLAY_UNOWNED | *display_ownership::INPUT_CLIENT_OWNED,
                *display_ownership::DISPLAY_PLATFORM_OWNED
                    | *display_ownership::INPUT_PLATFORM_OWNED,
            )
            .unwrap();
        loop_done.next().await.expect("loop_done notification");

        // All 3 injected devices are retained!
        assert_eq!(bindings.lock().len(), 3);

        // Send mouse report on mouse 1: delivered!
        mouse1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(40),
                        movement_y: Some(40),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                4,
            )
            .unwrap();
        assert_eq!(mouse1_acks.next().await, Some(4));
        let ev =
            received_events_receiver.next().await.expect("mouse 1 event delivered after restore");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::Mouse(_)));

        // Send mouse report on mouse 2: delivered!
        mouse2_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    mouse: Some(fidl_fuchsia_input_report::MouseInputReport {
                        movement_x: Some(50),
                        movement_y: Some(50),
                        scroll_v: None,
                        scroll_h: None,
                        pressed_buttons: Some(vec![]),
                        position_x: None,
                        position_y: None,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                2,
            )
            .unwrap();
        assert_eq!(mouse2_acks.next().await, Some(2));
        let ev =
            received_events_receiver.next().await.expect("mouse 2 event delivered after restore");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::Mouse(_)));

        // Send button report on button 1: delivered!
        button1_ctrl
            .send_on_input_reports(
                vec![fidl_fuchsia_input_report::InputReport {
                    event_time: Some(zx::MonotonicInstant::get().into_nanos()),
                    consumer_control: Some(fidl_fuchsia_input_report::ConsumerControlInputReport {
                        pressed_buttons: Some(vec![
                            fidl_fuchsia_input_report::ConsumerControlButton::VolumeUp,
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                4,
            )
            .unwrap();
        assert_eq!(button1_acks.next().await, Some(4));
        let ev =
            received_events_receiver.next().await.expect("button event delivered after restore");
        assert!(matches!(ev.device_event, input_device::InputDeviceEvent::ConsumerControls(_)));

        std::mem::drop(_pipeline_task);
    }
}
