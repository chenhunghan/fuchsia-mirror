// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::input_device::{self, InputEvent, UnhandledInputEvent};
use crate::input_handler::{Handler, InputHandlerStatus, UnhandledInputHandler};
use crate::keyboard_binding::{KeyboardDeviceDescriptor, KeyboardEvent};
use crate::metrics;
use anyhow::{Context, Result};
use async_trait::async_trait;
use fidl_fuchsia_input::Key;
use fidl_fuchsia_ui_input_internal as finput;
use fidl_fuchsia_ui_input3::KeyEventType;
use fuchsia_async::{OnSignals, Task};
use fuchsia_inspect::health::Reporter;
use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedReceiver, UnboundedSender};
use futures::future::LocalBoxFuture;
use keymaps::KeyState;
use metrics_registry::InputPipelineErrorMetricDimensionEvent;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;
use zx::{
    AsHandleRef, MonotonicDuration, MonotonicInstant, NullableHandle, Rights, Signals, Status,
    WaitResult,
};

// Signal value corresponding to `DISPLAY_UNOWNED` (ZX_USER_SIGNAL_0).
pub static DISPLAY_UNOWNED: LazyLock<Signals> = LazyLock::new(|| {
    Signals::from_bits(finput::InputOwnershipSignal::DISPLAY_UNOWNED.bits())
        .expect("static init should not fail")
});

// Signal value corresponding to `DISPLAY_PLATFORM_OWNED` (ZX_USER_SIGNAL_1).
pub static DISPLAY_PLATFORM_OWNED: LazyLock<Signals> = LazyLock::new(|| {
    Signals::from_bits(finput::InputOwnershipSignal::DISPLAY_PLATFORM_OWNED.bits())
        .expect("static init should not fail")
});

// Signal value corresponding to `INPUT_CLIENT_OWNED` (ZX_USER_SIGNAL_3).
pub static INPUT_CLIENT_OWNED: LazyLock<Signals> = LazyLock::new(|| {
    Signals::from_bits(finput::InputOwnershipSignal::INPUT_CLIENT_OWNED.bits())
        .expect("static init should not fail")
});

// Signal value corresponding to `INPUT_PLATFORM_OWNED` (ZX_USER_SIGNAL_4).
pub static INPUT_PLATFORM_OWNED: LazyLock<Signals> = LazyLock::new(|| {
    Signals::from_bits(finput::InputOwnershipSignal::INPUT_PLATFORM_OWNED.bits())
        .expect("static init should not fail")
});

// Any ownership-related signal.
pub static ANY_OWNERSHIP_SIGNAL: LazyLock<Signals> = LazyLock::new(|| {
    *DISPLAY_UNOWNED | *DISPLAY_PLATFORM_OWNED | *INPUT_CLIENT_OWNED | *INPUT_PLATFORM_OWNED
});

// Granular ownership state representing which entity owns display and input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnershipState {
    /// Both display and input are owned by the platform.
    PlatformOwned,
    /// Display is owned by the platform, but input is owned by a direct client (e.g. Starnix).
    ClientInputOwned,
    /// Display is unowned (e.g. switched away to virtual console / Virtcon).
    DisplayUnowned,
}

// Stores the last received ownership signals.
#[derive(Debug, Clone, PartialEq)]
pub struct Ownership {
    pub signals: Signals,
}

impl std::convert::From<Signals> for Ownership {
    fn from(signals: Signals) -> Self {
        Ownership { signals }
    }
}

impl Ownership {
    /// Evaluates the 4-bit orthogonal signals on `signals` to determine the granular ownership state.
    ///
    /// - If `Signals::OBJECT_PEER_CLOSED` is asserted, or `DISPLAY_UNOWNED` is asserted, or
    ///   `DISPLAY_PLATFORM_OWNED` is missing, ownership is `DisplayUnowned`.
    /// - If display is platform-owned, but `INPUT_CLIENT_OWNED` is asserted or `INPUT_PLATFORM_OWNED`
    ///   is missing, ownership is `ClientInputOwned`.
    /// - Otherwise, ownership is `PlatformOwned`.
    pub fn signals_to_ownership_state(signals: Signals) -> OwnershipState {
        if signals.contains(Signals::OBJECT_PEER_CLOSED) {
            return OwnershipState::DisplayUnowned;
        }
        let is_display_unowned =
            signals.contains(*DISPLAY_UNOWNED) || !signals.contains(*DISPLAY_PLATFORM_OWNED);
        if is_display_unowned {
            return OwnershipState::DisplayUnowned;
        }

        let has_input_signals =
            signals.contains(*INPUT_CLIENT_OWNED) || signals.contains(*INPUT_PLATFORM_OWNED);
        if has_input_signals {
            if signals.contains(*INPUT_CLIENT_OWNED) || !signals.contains(*INPUT_PLATFORM_OWNED) {
                OwnershipState::ClientInputOwned
            } else {
                OwnershipState::PlatformOwned
            }
        } else {
            // Fallback for legacy display-only signals if ViewTree bits are absent.
            OwnershipState::PlatformOwned
        }
    }

    /// Returns the granular ownership state.
    pub fn ownership_state(&self) -> OwnershipState {
        Self::signals_to_ownership_state(self.signals)
    }

    /// Returns true if display is unowned.
    pub fn is_display_unowned(&self) -> bool {
        self.ownership_state() == OwnershipState::DisplayUnowned
    }

    /// Returns true if input is client-owned while display is platform-owned.
    pub fn is_input_client_owned(&self) -> bool {
        self.ownership_state() == OwnershipState::ClientInputOwned
    }

    /// Returns true if the platform does not currently own both display and input.
    pub fn is_display_ownership_lost(&self) -> bool {
        Self::is_signals_ownership_lost(self.signals)
    }

    /// Evaluates the 4-bit orthogonal signals on `signals` to determine whether
    /// platform input ownership is lost.
    pub fn is_signals_ownership_lost(signals: Signals) -> bool {
        Self::signals_to_ownership_state(signals) != OwnershipState::PlatformOwned
    }

    fn read_current_signals<T: AsHandleRef>(event: &T) -> Result<Signals, Status> {
        match event.as_handle_ref().wait_one(Signals::empty(), MonotonicInstant::INFINITE_PAST) {
            WaitResult::Ok(signals) | WaitResult::TimedOut(signals) => Ok(signals),
            WaitResult::Canceled(signals) => Ok(signals),
            WaitResult::Err(Status::PEER_CLOSED) => Ok(Signals::OBJECT_PEER_CLOSED),
            WaitResult::Err(status) => Err(status),
        }
    }

    fn has_contradictory_signals(signals: Signals) -> bool {
        (signals.contains(*DISPLAY_PLATFORM_OWNED) && signals.contains(*DISPLAY_UNOWNED))
            || (signals.contains(*INPUT_PLATFORM_OWNED) && signals.contains(*INPUT_CLIENT_OWNED))
    }

    /// Waits for the next ownership state transition.
    pub async fn wait_ownership_change<T: AsHandleRef>(
        &self,
        event: &T,
    ) -> Result<Signals, Status> {
        let current_state = self.ownership_state();
        loop {
            let cur = Self::read_current_signals(event)?;
            if cur.contains(Signals::OBJECT_PEER_CLOSED) {
                if current_state == OwnershipState::DisplayUnowned {
                    return Err(Status::PEER_CLOSED);
                } else {
                    return Ok(cur);
                }
            }
            if Self::has_contradictory_signals(cur) {
                log::error!(
                    "display_ownership: peer asserted invalid contradictory signals: {:?}",
                    cur
                );
                return Err(Status::BAD_STATE);
            }
            let new_state = Self::signals_to_ownership_state(cur);
            if new_state != current_state {
                return Ok(cur);
            }

            let wait_signals = match current_state {
                OwnershipState::PlatformOwned => {
                    *DISPLAY_UNOWNED | *INPUT_CLIENT_OWNED | Signals::OBJECT_PEER_CLOSED
                }
                OwnershipState::ClientInputOwned => {
                    *DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED | Signals::OBJECT_PEER_CLOSED
                }
                OwnershipState::DisplayUnowned => {
                    *DISPLAY_PLATFORM_OWNED | Signals::OBJECT_PEER_CLOSED
                }
            };

            OnSignals::new(event, wait_signals).await?;
        }
    }
}

type OwnershipChangeCallback =
    Rc<dyn Fn(OwnershipState, OwnershipState) -> LocalBoxFuture<'static, ()>>;

/// A handler that turns the input pipeline off or on based on whether
/// Scenic and ViewTree grant input ownership to the platform.
///
/// This allows us to pause input processing when the user switches away
/// from the product (e.g. terminal) into virtual console or when a client view
/// (such as Starnix) has direct hardware input ownership.
///
/// See the `README.md` file in this crate for details.
///
/// # Safety and Concurrency
///
/// This struct uses `RefCell` to manage internal state. While `DisplayOwnership`
/// logic is split between multiple tasks (`handle_ownership_change` and
/// `handle_unhandled_input_event`), safety is maintained because:
/// 1. The pipeline runs on a single-threaded `LocalExecutor`.
/// 2. Borrows of `RefCell`s (like `ownership` and `key_state`) are never held
///    across `await` points.
///
/// If asynchronous calls are added to critical sections in the future,
/// ensure that all borrows are dropped before the `await`.
pub struct DisplayOwnership {
    /// The current view of the display ownership. It is mutated by the
    /// display ownership task when appropriate signals arrive.
    ownership: Rc<RefCell<Ownership>>,

    /// The registry of currently pressed keys.
    key_state: RefCell<KeyState>,

    /// Keys released or cancelled before handle_ownership_change processed the ownership loss.
    pending_released_keys: RefCell<Vec<Key>>,

    /// The source of ownership change events for the main loop.
    display_ownership_change_receiver: RefCell<Option<UnboundedReceiver<Ownership>>>,

    /// An optional callback invoked whenever ownership transitions.
    /// The callback receives `(old_state, new_state)`.
    ownership_change_callback: RefCell<Option<OwnershipChangeCallback>>,

    /// A background task that watches for display ownership changes. We keep
    /// it alive to ensure that it keeps running.
    _display_ownership_task: Task<()>,

    /// The metrics logger.
    metrics_logger: metrics::MetricsLogger,

    /// The inventory of this handler's Inspect status.
    inspect_status: InputHandlerStatus,

    /// The event processing loop will do an `unbounded_send(())` on this
    /// channel once at the end of each loop pass, in test configurations only.
    /// The test fixture uses this channel to execute test fixture in
    /// lock-step with the event processing loop for test cases where the
    /// precise event sequencing is relevant.
    #[cfg(test)]
    loop_done: RefCell<Option<UnboundedSender<()>>>,
    display_ownership_event: NullableHandle,
}

impl DisplayOwnership {
    /// Creates a new handler that watches `display_ownership_event` for events.
    ///
    /// The `display_ownership_event` is assumed to be an [Event] obtained from
    /// `fuchsia.ui.input.internal.InputOwnership/GetEvent`.
    pub fn new(
        display_ownership_event: impl AsHandleRef + 'static,
        input_handlers_node: &fuchsia_inspect::Node,
        metrics_logger: metrics::MetricsLogger,
    ) -> Rc<Self> {
        DisplayOwnership::new_internal(
            display_ownership_event,
            None,
            input_handlers_node,
            metrics_logger,
        )
    }

    #[cfg(test)]
    pub fn new_for_test(
        display_ownership_event: impl AsHandleRef + 'static,
        loop_done: UnboundedSender<()>,
        metrics_logger: metrics::MetricsLogger,
    ) -> Rc<Self> {
        let inspector = fuchsia_inspect::Inspector::default();
        let fake_handlers_node = inspector.root().create_child("input_handlers_node");
        DisplayOwnership::new_internal(
            display_ownership_event,
            Some(loop_done),
            &fake_handlers_node,
            metrics_logger,
        )
    }

    fn new_internal(
        display_ownership_event: impl AsHandleRef + 'static,
        _loop_done: Option<UnboundedSender<()>>,
        input_handlers_node: &fuchsia_inspect::Node,
        metrics_logger: metrics::MetricsLogger,
    ) -> Rc<Self> {
        let event_handle = display_ownership_event
            .as_handle_ref()
            .duplicate_handle(Rights::SAME_RIGHTS)
            .expect("unable to duplicate display ownership event");
        let initial_state = match display_ownership_event
            .as_handle_ref()
            .wait_one(*ANY_OWNERSHIP_SIGNAL, MonotonicInstant::INFINITE_PAST)
        {
            WaitResult::Ok(signals) | WaitResult::TimedOut(signals) => signals,
            WaitResult::Canceled(signals) => signals,
            WaitResult::Err(e) => panic!("unable to set the initial display state: {:?}", e),
        };
        log::debug!("setting initial display ownership to: {:?}", initial_state);
        let initial_ownership: Ownership = initial_state.into();
        let ownership = Rc::new(RefCell::new(initial_ownership.clone()));

        let mut ownership_clone = initial_ownership;
        let (ownership_sender, ownership_receiver) = mpsc::unbounded();
        let display_ownership_task = Task::local(async move {
            loop {
                let signals = ownership_clone.wait_ownership_change(&display_ownership_event).await;
                match signals {
                    Err(e) => {
                        log::warn!("could not read display state: {:?}", e);
                        break;
                    }
                    Ok(signals) => {
                        log::debug!("setting display ownership to: {:?}", signals);
                        ownership_sender.unbounded_send(signals.into()).unwrap();
                        ownership_clone = signals.into();
                    }
                }
            }
            log::warn!(
                "display loop exiting and will no longer monitor display changes - this is not expected"
            );
        });
        log::info!("Display ownership handler installed");
        let inspect_status = InputHandlerStatus::new(
            input_handlers_node,
            "display_ownership",
            /* generates_events */ false,
        );
        Rc::new(Self {
            ownership,
            key_state: RefCell::new(KeyState::new()),
            pending_released_keys: RefCell::new(Vec::new()),
            display_ownership_change_receiver: RefCell::new(Some(ownership_receiver)),
            ownership_change_callback: RefCell::new(None),
            _display_ownership_task: display_ownership_task,
            metrics_logger,
            inspect_status,
            #[cfg(test)]
            loop_done: RefCell::new(_loop_done),
            display_ownership_event: event_handle,
        })
    }

    /// Registers a callback to be invoked whenever ownership transitions.
    /// The callback receives `(old_state, new_state)`.
    pub fn set_ownership_change_callback(
        &self,
        callback: impl Fn(OwnershipState, OwnershipState) -> LocalBoxFuture<'static, ()> + 'static,
    ) {
        *self.ownership_change_callback.borrow_mut() = Some(Rc::new(callback));
    }

    /// Returns the granular ownership state.
    pub fn ownership_state(&self) -> OwnershipState {
        // Query the signal state synchronously with INFINITE_PAST (non-blocking)
        // to get the real-time state and avoid TOCTOU race conditions during
        // display transitions. While this introduces a syscall overhead, it is
        // negligible for low-frequency keyboard events.
        match self
            .display_ownership_event
            .wait_one(Signals::empty(), MonotonicInstant::INFINITE_PAST)
        {
            WaitResult::Ok(signals) | WaitResult::TimedOut(signals) => {
                Ownership::signals_to_ownership_state(signals)
            }
            WaitResult::Err(Status::PEER_CLOSED) | WaitResult::Canceled(_) => {
                OwnershipState::DisplayUnowned
            }
            WaitResult::Err(e) => {
                log::error!("Unexpected error on display ownership event: {:?}", e);
                self.ownership.borrow().ownership_state()
            }
        }
    }

    /// Returns true if display is unowned.
    pub fn is_display_unowned(&self) -> bool {
        self.ownership_state() == OwnershipState::DisplayUnowned
    }

    /// Returns true if input is client-owned while display is platform-owned.
    pub fn is_input_client_owned(&self) -> bool {
        self.ownership_state() == OwnershipState::ClientInputOwned
    }

    /// Returns true if the platform does not currently own both display and input.
    pub fn is_display_ownership_lost(&self) -> bool {
        self.ownership_state() != OwnershipState::PlatformOwned
    }

    /// Watches for display ownership changes and sends cancel/sync events.
    ///
    /// NOTE: RefCell safety relies on the single-threaded nature of the executor.
    /// No borrows of `ownership` or `key_state` must be held across the `await`
    /// below to avoid panics if `handle_unhandled_input_event` runs while this
    /// task is suspended.
    pub async fn handle_ownership_change(
        self: &Rc<Self>,
        output: UnboundedSender<Vec<InputEvent>>,
    ) -> Result<()> {
        let mut ownership_source = self
            .display_ownership_change_receiver
            .borrow_mut()
            .take()
            .context("display_ownership_change_receiver already taken")?;
        while let Some(new_ownership) = ownership_source.next().await {
            let was_ownership_state = self.ownership.borrow().ownership_state();
            let new_ownership_state = new_ownership.ownership_state();
            let was_ownership_lost = was_ownership_state != OwnershipState::PlatformOwned;
            let is_display_ownership_lost = new_ownership_state != OwnershipState::PlatformOwned;
            // When the ownership is modified, float a set of cancel or sync
            // events to scoop up stale keyboard state, treating it the same
            // as loss of focus.
            if was_ownership_lost != is_display_ownership_lost {
                let event_type = match is_display_ownership_lost {
                    true => KeyEventType::Cancel,
                    false => KeyEventType::Sync,
                };
                let keys = self.key_state.borrow().get_set();
                let mut event_time = MonotonicInstant::get();
                for key in keys.into_iter() {
                    let key_event = KeyboardEvent::new(key, event_type);
                    output
                        .unbounded_send(vec![into_input_event(key_event, event_time)])
                        .context("unable to send display updates")?;
                    event_time = event_time + MonotonicDuration::from_nanos(1);
                }
                if is_display_ownership_lost {
                    for key in self.pending_released_keys.borrow_mut().drain(..) {
                        self.key_state.borrow_mut().update(KeyEventType::Released, key);
                    }
                }
            }
            *(self.ownership.borrow_mut()) = new_ownership;
            if was_ownership_state != new_ownership_state {
                let callback = self.ownership_change_callback.borrow().clone();
                if let Some(cb) = callback {
                    cb(was_ownership_state, new_ownership_state).await;
                }
            }
            #[cfg(test)]
            {
                if let Some(loop_done) = self.loop_done.borrow().as_ref() {
                    loop_done.unbounded_send(()).unwrap();
                }
            }
        }
        Ok(())
    }
}

impl Handler for DisplayOwnership {
    fn set_handler_healthy(self: std::rc::Rc<Self>) {
        self.inspect_status.health_node.borrow_mut().set_ok();
    }

    fn set_handler_unhealthy(self: std::rc::Rc<Self>, msg: &str) {
        self.inspect_status.health_node.borrow_mut().set_unhealthy(msg);
    }

    fn get_name(&self) -> &'static str {
        "DisplayOwnership"
    }

    fn interest(&self) -> Vec<input_device::InputEventType> {
        vec![input_device::InputEventType::Keyboard]
    }
}

#[async_trait(?Send)]
impl UnhandledInputHandler for DisplayOwnership {
    async fn handle_unhandled_input_event(
        self: Rc<Self>,
        unhandled_input_event: UnhandledInputEvent,
    ) -> Vec<input_device::InputEvent> {
        fuchsia_trace::duration!("input", "display_ownership");
        self.inspect_status.count_received_event(&unhandled_input_event.event_time);
        let is_display_ownership_lost = self.is_display_ownership_lost();
        match unhandled_input_event.device_event {
            input_device::InputDeviceEvent::Keyboard(ref e) => {
                if matches!(e.get_event_type(), KeyEventType::Pressed | KeyEventType::Sync) {
                    self.pending_released_keys.borrow_mut().retain(|k| *k != e.get_key());
                    self.key_state.borrow_mut().update(e.get_event_type(), e.get_key());
                } else if matches!(
                    e.get_event_type(),
                    KeyEventType::Released | KeyEventType::Cancel
                ) {
                    if is_display_ownership_lost
                        && self.ownership.borrow().ownership_state()
                            == OwnershipState::PlatformOwned
                    {
                        let mut pending = self.pending_released_keys.borrow_mut();
                        if !pending.contains(&e.get_key()) {
                            pending.push(e.get_key());
                        }
                    } else {
                        self.key_state.borrow_mut().update(e.get_event_type(), e.get_key());
                    }
                }
            }
            _ => {
                self.metrics_logger.log_error(
                    InputPipelineErrorMetricDimensionEvent::HandlerReceivedUninterestedEvent,
                    std::format!(
                        "{} uninterested input event: {:?}",
                        self.get_name(),
                        unhandled_input_event.get_event_type()
                    ),
                );
            }
        }
        if is_display_ownership_lost {
            self.inspect_status.count_handled_event();
        }

        #[cfg(test)]
        {
            if let Some(loop_done) = self.loop_done.borrow().as_ref() {
                loop_done.unbounded_send(()).unwrap();
            }
        }

        vec![
            input_device::InputEvent::from(unhandled_input_event)
                .into_handled_if(is_display_ownership_lost),
        ]
    }
}

fn empty_keyboard_device_descriptor() -> input_device::InputDeviceDescriptor {
    input_device::InputDeviceDescriptor::Keyboard(
        // Should descriptor be something sensible?
        KeyboardDeviceDescriptor {
            keys: vec![],
            device_information: fidl_fuchsia_input_report::DeviceInformation {
                vendor_id: Some(0),
                product_id: Some(0),
                version: Some(0),
                polling_rate: Some(0),
                ..Default::default()
            },
            device_id: 0,
        },
    )
}

fn into_input_event(
    keyboard_event: KeyboardEvent,
    event_time: MonotonicInstant,
) -> input_device::InputEvent {
    input_device::InputEvent {
        device_event: input_device::InputDeviceEvent::Keyboard(keyboard_event),
        device_descriptor: empty_keyboard_device_descriptor(),
        event_time,
        handled: input_device::Handled::No,
        trace_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing_utilities::{create_fake_input_event, create_input_event};
    use fidl_fuchsia_input::Key;
    use fuchsia_async as fasync;
    use pretty_assertions::assert_eq;
    use std::convert::TryFrom as _;
    use zx::{EventPair, Peered};

    // Manages losing and regaining display and input ownership, since manual management
    // is error-prone: if signal_peer does not change the signal state, the waiting
    // process will block forever, which makes tests run longer than needed.
    struct DisplayWrangler {
        event: EventPair,
        last_display: Signals,
        last_input: Signals,
    }

    impl DisplayWrangler {
        fn new(event: EventPair) -> Self {
            let mut instance = DisplayWrangler {
                event,
                last_display: *DISPLAY_PLATFORM_OWNED,
                last_input: *INPUT_PLATFORM_OWNED,
            };
            // Initialize the signal state to unowned before the handler reads it,
            // establishing a predictable baseline state for the test cases.
            instance
                .event
                .signal_peer(Signals::empty(), *DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED)
                .unwrap();
            instance.last_display = *DISPLAY_UNOWNED;
            instance.last_input = *INPUT_PLATFORM_OWNED;
            instance
        }

        fn set_unowned(&mut self) {
            assert!(self.last_display != *DISPLAY_UNOWNED, "display is already unowned");
            self.event.signal_peer(*DISPLAY_PLATFORM_OWNED, *DISPLAY_UNOWNED).unwrap();
            self.last_display = *DISPLAY_UNOWNED;
        }

        fn set_owned(&mut self) {
            assert!(self.last_display != *DISPLAY_PLATFORM_OWNED, "display is already owned");
            self.event.signal_peer(*DISPLAY_UNOWNED, *DISPLAY_PLATFORM_OWNED).unwrap();
            self.last_display = *DISPLAY_PLATFORM_OWNED;
        }

        fn set_client_input_owned(&mut self) {
            assert!(self.last_input != *INPUT_CLIENT_OWNED, "input is already client owned");
            self.event.signal_peer(*INPUT_PLATFORM_OWNED, *INPUT_CLIENT_OWNED).unwrap();
            self.last_input = *INPUT_CLIENT_OWNED;
        }

        fn set_platform_input_owned(&mut self) {
            assert!(self.last_input != *INPUT_PLATFORM_OWNED, "input is already platform owned");
            self.event.signal_peer(*INPUT_CLIENT_OWNED, *INPUT_PLATFORM_OWNED).unwrap();
            self.last_input = *INPUT_PLATFORM_OWNED;
        }
    }

    #[fuchsia::test]
    async fn display_ownership_change() {
        // handler_event is the event that the unit under test will examine for
        // display ownership changes. test_event is used to set the appropriate
        // signals.
        let (test_event, handler_event) = EventPair::create();

        // test_sender is used to pipe input events into the handler.
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();

        // test_receiver is used to pipe input events out of the handler.
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();

        // The unit under test adds a () each time it completes one pass through
        // its event loop. Use to ensure synchronization.
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        // We use a wrapper to signal test_event correctly, since doing it wrong
        // by hand causes tests to hang, which isn't the best dev experience.
        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        // Go two full circles of signaling.

        // 1
        wrangler.set_owned();
        loop_done.next().await;
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        // 2
        wrangler.set_unowned();
        loop_done.next().await;
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        // 3
        wrangler.set_owned();
        loop_done.next().await;
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        // 4
        wrangler.set_unowned();
        loop_done.next().await;
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(4)
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                // Event received while we owned the display.
                create_fake_input_event(fake_time),
                // Event received when we lost the display.
                create_fake_input_event(fake_time).into_handled(),
                // Display ownership regained.
                create_fake_input_event(fake_time),
                // Display ownership lost.
                create_fake_input_event(fake_time).into_handled(),
            ]
        );
    }

    fn new_keyboard_input_event(key: Key, event_type: KeyEventType) -> InputEvent {
        let fake_time = MonotonicInstant::from_nanos(42);
        create_input_event(
            KeyboardEvent::new(key, event_type),
            &input_device::InputDeviceDescriptor::Fake,
            fake_time,
            input_device::Handled::No,
        )
    }

    #[fuchsia::test]
    async fn basic_key_state_handling() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();
        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        // Gain the display, and press a key.
        wrangler.set_owned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // Lose display.
        wrangler.set_unowned();
        loop_done.next().await;

        // Regain display
        wrangler.set_owned();
        loop_done.next().await;

        // Key event after regaining.
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(4)
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                new_keyboard_input_event(Key::A, KeyEventType::Pressed),
                new_keyboard_input_event(Key::A, KeyEventType::Cancel)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::A, KeyEventType::Sync)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::A, KeyEventType::Released),
            ]
        );
    }

    #[fuchsia::test]
    async fn more_key_state_handling() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();
        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        wrangler.set_owned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::B, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // Lose display, release a key, press a key.
        wrangler.set_unowned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::B, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::C, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // Regain display
        wrangler.set_owned();
        loop_done.next().await;

        // Key event after regaining.
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::C, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(10) // 2 pressed, 2 cancelled, 1 released (handled), 1 pressed (handled), 2 synced, 2 released
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                new_keyboard_input_event(Key::A, KeyEventType::Pressed),
                new_keyboard_input_event(Key::B, KeyEventType::Pressed),
                new_keyboard_input_event(Key::A, KeyEventType::Cancel)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::B, KeyEventType::Cancel)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::B, KeyEventType::Released).into_handled(),
                new_keyboard_input_event(Key::C, KeyEventType::Pressed).into_handled(),
                // The CANCEL and SYNC events are emitted in the sort ordering of the
                // `Key` enum values. Perhaps they should be emitted instead in the order
                // they have been received for SYNC, and in reverse order for CANCEL.
                new_keyboard_input_event(Key::A, KeyEventType::Sync)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::C, KeyEventType::Sync)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::A, KeyEventType::Released),
                new_keyboard_input_event(Key::C, KeyEventType::Released),
            ]
        );
    }

    #[fuchsia::test]
    async fn display_ownership_initialized_with_inspect_node() {
        let (test_event, handler_event) = EventPair::create();
        let (loop_done_sender, _) = mpsc::unbounded::<()>();
        let inspector = fuchsia_inspect::Inspector::default();
        let fake_handlers_node = inspector.root().create_child("input_handlers_node");
        // Signal needs to be initialized first so DisplayOwnership::new doesn't panic with a TIMEOUT error
        let _ = DisplayWrangler::new(test_event);
        let _handler = DisplayOwnership::new_internal(
            handler_event,
            Some(loop_done_sender),
            &fake_handlers_node,
            metrics::MetricsLogger::default(),
        );
        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_handlers_node: {
                display_ownership: {
                    events_received_count: 0u64,
                    events_handled_count: 0u64,
                    last_received_timestamp_ns: 0u64,
                    "fuchsia.inspect.Health": {
                        status: "STARTING_UP",
                        // Timestamp value is unpredictable and not relevant in this context,
                        // so we only assert that the property is present.
                        start_timestamp_nanos: diagnostics_assertions::AnyProperty
                    },
                }
            }
        });
    }

    #[fuchsia::test]
    async fn display_ownership_inspect_counts_events() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, _test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();
        let mut wrangler = DisplayWrangler::new(test_event);
        let inspector = fuchsia_inspect::Inspector::default();
        let fake_handlers_node = inspector.root().create_child("input_handlers_node");
        let handler = DisplayOwnership::new_internal(
            handler_event,
            Some(loop_done_sender),
            &fake_handlers_node,
            metrics::MetricsLogger::default(),
        );
        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        // Gain the display, and press a key.
        wrangler.set_owned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // Lose display
        // Input event is marked `Handled` if received after display ownership is lost
        wrangler.set_unowned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::B, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // Regain display
        wrangler.set_owned();
        loop_done.next().await;

        // Key event after regaining.
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;

        diagnostics_assertions::assert_data_tree!(inspector, root: {
            input_handlers_node: {
                display_ownership: {
                    events_received_count: 3u64,
                    events_handled_count: 1u64,
                    last_received_timestamp_ns: 42u64,
                    "fuchsia.inspect.Health": {
                        status: "STARTING_UP",
                        // Timestamp value is unpredictable and not relevant in this context,
                        // so we only assert that the property is present.
                        start_timestamp_nanos: diagnostics_assertions::AnyProperty
                    },
                }
            }
        });
    }

    #[fuchsia::test]
    async fn display_ownership_peer_closed() {
        let (test_event, handler_event) = EventPair::create();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let (handler_sender, _test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let handler_clone = handler.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender).await.unwrap();
        });

        // 1. Set to owned.
        wrangler.set_owned();
        loop_done.next().await;

        // Verify it is owned (not lost).
        assert!(!handler.is_display_ownership_lost());

        // 2. Close the peer by dropping wrangler.
        std::mem::drop(wrangler);

        // Verify it is now lost.
        assert!(handler.is_display_ownership_lost());
    }

    #[fuchsia::test]
    async fn orthogonal_input_client_owned_transitions() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        // 1. Gain display: both DISPLAY_PLATFORM_OWNED and INPUT_PLATFORM_OWNED are asserted.
        wrangler.set_owned();
        loop_done.next().await;
        assert!(!handler.is_display_ownership_lost());
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        // 2. Client takes direct input (INPUT_CLIENT_OWNED), display remains owned.
        wrangler.set_client_input_owned();
        loop_done.next().await;
        assert!(handler.is_display_ownership_lost());
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        // 3. Client direct input ends, platform input restored (INPUT_PLATFORM_OWNED).
        wrangler.set_platform_input_owned();
        loop_done.next().await;
        assert!(!handler.is_display_ownership_lost());
        test_sender.unbounded_send(create_fake_input_event(fake_time)).unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(3)
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                create_fake_input_event(fake_time),
                create_fake_input_event(fake_time).into_handled(),
                create_fake_input_event(fake_time),
            ]
        );
    }

    #[fuchsia::test]
    async fn orthogonal_virtcon_while_client_owned() {
        let (test_event, handler_event) = EventPair::create();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let (handler_sender, _test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let handler_clone = handler.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender).await.unwrap();
        });

        // 1. Gain normal UI ownership.
        wrangler.set_owned();
        loop_done.next().await;
        assert!(!handler.is_display_ownership_lost());
        assert_eq!(handler.ownership_state(), OwnershipState::PlatformOwned);

        // 2. Client takes input (Bit 1 | Bit 3).
        wrangler.set_client_input_owned();
        loop_done.next().await;
        assert!(handler.is_display_ownership_lost());
        assert_eq!(handler.ownership_state(), OwnershipState::ClientInputOwned);

        // 3. User switches to Virtcon while client owned (Bit 0 | Bit 3).
        wrangler.set_unowned();
        loop_done.next().await;
        assert!(handler.is_display_ownership_lost());
        assert_eq!(handler.ownership_state(), OwnershipState::DisplayUnowned);

        // 4. User switches back from Virtcon to Scenic (Bit 1 | Bit 3).
        wrangler.set_owned();
        loop_done.next().await;
        assert!(handler.is_display_ownership_lost());
        assert_eq!(handler.ownership_state(), OwnershipState::ClientInputOwned);

        // 5. Client exits direct input (Bit 1 | Bit 4).
        // Now pipeline regains ownership!
        wrangler.set_platform_input_owned();
        loop_done.next().await;
        assert!(!handler.is_display_ownership_lost());
        assert_eq!(handler.ownership_state(), OwnershipState::PlatformOwned);
    }

    #[fuchsia::test]
    async fn ownership_change_callback_invoked() {
        let (test_event, handler_event) = EventPair::create();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let (cb_sender, mut cb_receiver) = mpsc::unbounded::<(OwnershipState, OwnershipState)>();
        handler.set_ownership_change_callback(move |old_state, new_state| {
            let cb_sender = cb_sender.clone();
            Box::pin(async move {
                cb_sender.unbounded_send((old_state, new_state)).unwrap();
            })
        });

        let (handler_sender, _test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let handler_clone = handler.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender).await.unwrap();
        });

        // 1. Set to owned -> callback invoked with PlatformOwned.
        wrangler.set_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::DisplayUnowned, OwnershipState::PlatformOwned))
        );

        // 2. Switch to Virtcon (unowned) -> callback invoked with DisplayUnowned.
        wrangler.set_unowned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::PlatformOwned, OwnershipState::DisplayUnowned))
        );

        // 3. Switch back to Scenic -> callback invoked with PlatformOwned.
        wrangler.set_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::DisplayUnowned, OwnershipState::PlatformOwned))
        );

        // 4. Client takes direct input -> callback invoked with ClientInputOwned.
        wrangler.set_client_input_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::PlatformOwned, OwnershipState::ClientInputOwned))
        );

        // 5. Client releases direct input -> callback invoked with PlatformOwned.
        wrangler.set_platform_input_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::ClientInputOwned, OwnershipState::PlatformOwned))
        );
    }

    #[fuchsia::test]
    async fn ownership_change_callback_between_client_input_owned_and_display_unowned() {
        let (test_event, handler_event) = EventPair::create();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let (cb_sender, mut cb_receiver) = mpsc::unbounded::<(OwnershipState, OwnershipState)>();
        handler.set_ownership_change_callback(move |old_state, new_state| {
            let cb_sender = cb_sender.clone();
            Box::pin(async move {
                cb_sender.unbounded_send((old_state, new_state)).unwrap();
            })
        });

        let (handler_sender, _test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let handler_clone = handler.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender).await.unwrap();
        });

        // 1. Initial owned -> callback invoked with PlatformOwned.
        wrangler.set_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::DisplayUnowned, OwnershipState::PlatformOwned))
        );

        // 2. Client takes input -> callback invoked with ClientInputOwned.
        wrangler.set_client_input_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::PlatformOwned, OwnershipState::ClientInputOwned))
        );

        // 3. User switches to Virtcon while client owned -> callback invoked with DisplayUnowned.
        wrangler.set_unowned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::ClientInputOwned, OwnershipState::DisplayUnowned))
        );

        // 4. User switches back from Virtcon to Scenic -> callback invoked with ClientInputOwned.
        wrangler.set_owned();
        loop_done.next().await;
        assert_eq!(
            cb_receiver.next().await,
            Some((OwnershipState::DisplayUnowned, OwnershipState::ClientInputOwned))
        );
    }

    #[fuchsia::test]
    fn signal_state_evaluations() {
        // PlatformOwned
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_PLATFORM_OWNED | *INPUT_PLATFORM_OWNED),
            OwnershipState::PlatformOwned
        );
        // ClientInputOwned
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_PLATFORM_OWNED | *INPUT_CLIENT_OWNED),
            OwnershipState::ClientInputOwned
        );
        // DisplayUnowned (Virtcon)
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED),
            OwnershipState::DisplayUnowned
        );
        // DisplayUnowned takes precedence even if INPUT_CLIENT_OWNED is asserted
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_UNOWNED | *INPUT_CLIENT_OWNED),
            OwnershipState::DisplayUnowned
        );
        // Peer closed -> DisplayUnowned
        assert_eq!(
            Ownership::signals_to_ownership_state(Signals::OBJECT_PEER_CLOSED),
            OwnershipState::DisplayUnowned
        );
        // Legacy display-only: DISPLAY_PLATFORM_OWNED without input signals -> PlatformOwned
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_PLATFORM_OWNED),
            OwnershipState::PlatformOwned
        );
        // Legacy display-only: DISPLAY_UNOWNED without input signals -> DisplayUnowned
        assert_eq!(
            Ownership::signals_to_ownership_state(*DISPLAY_UNOWNED),
            OwnershipState::DisplayUnowned
        );
    }

    #[fuchsia::test]
    async fn peer_closed_terminates_without_busy_loop() {
        let (test_event, handler_event) = EventPair::create();
        // Initialize as unowned
        test_event.signal_peer(Signals::empty(), *DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED).unwrap();

        let ownership = Ownership::from(*DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED);
        assert!(ownership.is_display_ownership_lost());

        // Drop peer while ownership is unowned.
        drop(test_event);

        // wait_ownership_change should return Err(Status::PEER_CLOSED) immediately without hanging.
        let result = ownership.wait_ownership_change(&handler_event).await;
        assert_eq!(result, Err(Status::PEER_CLOSED));
    }

    #[fuchsia::test]
    async fn peer_closed_transitions_to_unowned_then_terminates() {
        let (test_event, handler_event) = EventPair::create();
        // Initialize as owned
        test_event
            .signal_peer(Signals::empty(), *DISPLAY_PLATFORM_OWNED | *INPUT_PLATFORM_OWNED)
            .unwrap();

        let ownership = Ownership::from(*DISPLAY_PLATFORM_OWNED | *INPUT_PLATFORM_OWNED);
        assert!(!ownership.is_display_ownership_lost());

        // Drop peer.
        drop(test_event);

        // First wait: detects transition to unowned because of peer closed.
        let result = ownership.wait_ownership_change(&handler_event).await;
        assert!(result.is_ok());
        let cur_signals = result.unwrap();
        let unowned_ownership = Ownership::from(cur_signals);
        assert!(unowned_ownership.is_display_ownership_lost());

        // Second wait: since it is already unowned and peer is closed, returns PEER_CLOSED without busy loop.
        let result2 = unowned_ownership.wait_ownership_change(&handler_event).await;
        assert_eq!(result2, Err(Status::PEER_CLOSED));
    }

    #[fuchsia::test]
    async fn contradictory_display_signals_terminates_with_bad_state() {
        let (test_event, handler_event) = EventPair::create();
        // Peer asserts both DISPLAY_PLATFORM_OWNED and DISPLAY_UNOWNED (contradictory)
        // along with normal INPUT_PLATFORM_OWNED.
        let signals = *DISPLAY_PLATFORM_OWNED | *DISPLAY_UNOWNED | *INPUT_PLATFORM_OWNED;
        test_event.signal_peer(Signals::empty(), signals).unwrap();

        let ownership = Ownership::from(signals);
        assert!(ownership.is_display_ownership_lost());

        let result = ownership.wait_ownership_change(&handler_event).await;
        assert_eq!(result, Err(Status::BAD_STATE));
    }

    #[fuchsia::test]
    async fn contradictory_input_signals_terminates_with_bad_state() {
        let (test_event, handler_event) = EventPair::create();
        // Peer asserts both INPUT_PLATFORM_OWNED and INPUT_CLIENT_OWNED (contradictory)
        // along with normal DISPLAY_PLATFORM_OWNED.
        let signals = *DISPLAY_PLATFORM_OWNED | *INPUT_PLATFORM_OWNED | *INPUT_CLIENT_OWNED;
        test_event.signal_peer(Signals::empty(), signals).unwrap();

        let ownership = Ownership::from(signals);
        assert!(ownership.is_display_ownership_lost());

        let result = ownership.wait_ownership_change(&handler_event).await;
        assert_eq!(result, Err(Status::BAD_STATE));
    }

    #[fuchsia::test]
    async fn key_state_across_client_input_owned_and_display_unowned() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();
        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        // 1. Gain the display and input ownership, press key A.
        wrangler.set_owned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // 2. Client takes direct input ownership (PlatformOwned -> ClientInputOwned).
        // Since was_ownership_lost (false) != is_display_ownership_lost (true), Cancel is emitted.
        wrangler.set_client_input_owned();
        loop_done.next().await;

        // 3. Switch to Virtcon (ClientInputOwned -> DisplayUnowned).
        // Since was_ownership_lost (true) == is_display_ownership_lost (true), NO duplicate Cancel is emitted!
        wrangler.set_unowned();
        loop_done.next().await;

        // 4. Switch back to Scenic while client still owns input (DisplayUnowned -> ClientInputOwned).
        // Since was_ownership_lost (true) == is_display_ownership_lost (true), NO spurious Sync is emitted!
        wrangler.set_owned();
        loop_done.next().await;

        // 5. Restore platform input ownership (ClientInputOwned -> PlatformOwned).
        // Since was_ownership_lost (true) != is_display_ownership_lost (false), Sync is emitted!
        wrangler.set_platform_input_owned();
        loop_done.next().await;

        // 6. Release key A.
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Released))
            .unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(4)
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                new_keyboard_input_event(Key::A, KeyEventType::Pressed),
                new_keyboard_input_event(Key::A, KeyEventType::Cancel)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::A, KeyEventType::Sync)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::A, KeyEventType::Released),
            ]
        );
    }

    #[fuchsia::test]
    async fn peer_closed_from_client_input_owned_transitions_to_unowned_then_terminates() {
        let (test_event, handler_event) = EventPair::create();
        // Initialize as ClientInputOwned
        test_event
            .signal_peer(Signals::empty(), *DISPLAY_PLATFORM_OWNED | *INPUT_CLIENT_OWNED)
            .unwrap();

        let ownership = Ownership::from(*DISPLAY_PLATFORM_OWNED | *INPUT_CLIENT_OWNED);
        assert_eq!(ownership.ownership_state(), OwnershipState::ClientInputOwned);

        // Drop peer while in ClientInputOwned.
        drop(test_event);

        // First wait: detects transition to DisplayUnowned because of peer closed.
        let result = ownership.wait_ownership_change(&handler_event).await;
        assert!(result.is_ok());
        let cur_signals = result.unwrap();
        let unowned_ownership = Ownership::from(cur_signals);
        assert_eq!(unowned_ownership.ownership_state(), OwnershipState::DisplayUnowned);

        // Second wait: since it is already DisplayUnowned and peer is closed, returns PEER_CLOSED without busy loop.
        let result2 = unowned_ownership.wait_ownership_change(&handler_event).await;
        assert_eq!(result2, Err(Status::PEER_CLOSED));
    }

    #[fuchsia::test]
    async fn keyboard_cancel_and_in_flight_pressed_handling() {
        let (test_event, handler_event) = EventPair::create();
        let (test_sender, handler_receiver) = mpsc::unbounded::<InputEvent>();
        let (handler_sender, test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();
        let mut wrangler = DisplayWrangler::new(test_event);
        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        let handler_clone = handler.clone();
        let handler_sender_clone = handler_sender.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender_clone).await.unwrap();
        });

        let handler_clone_2 = handler.clone();
        let _input_task = fasync::Task::local(async move {
            let mut receiver = handler_receiver;
            while let Some(event) = receiver.next().await {
                let unhandled_event = UnhandledInputEvent::try_from(event).unwrap();
                let out_events =
                    handler_clone_2.clone().handle_unhandled_input_event(unhandled_event).await;
                handler_sender.unbounded_send(out_events).unwrap();
            }
        });

        let fake_time = MonotonicInstant::from_nanos(42);

        // 1. Gain ownership and press Key::A.
        wrangler.set_owned();
        loop_done.next().await;
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        // 2. Lose ownership.
        wrangler.set_unowned();
        loop_done.next().await;

        // 3. While unowned, an in-flight Pressed event for Key::B arrives,
        // followed by a Cancel event for Key::B (from discarding reports).
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::B, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;
        test_sender.unbounded_send(new_keyboard_input_event(Key::B, KeyEventType::Cancel)).unwrap();
        loop_done.next().await;

        // 4. Also release Key::A using a Cancel event while unowned.
        test_sender.unbounded_send(new_keyboard_input_event(Key::A, KeyEventType::Cancel)).unwrap();
        loop_done.next().await;

        // 5. Regain ownership. Since Key::A was cancelled and Key::B was pressed then cancelled,
        // no Sync events should be emitted!
        wrangler.set_owned();
        loop_done.next().await;

        // Send a normal Key::C press after regaining ownership to verify pipeline is working.
        test_sender
            .unbounded_send(new_keyboard_input_event(Key::C, KeyEventType::Pressed))
            .unwrap();
        loop_done.next().await;

        let actual: Vec<InputEvent> = test_receiver
            .take(6)
            .flat_map(|events| futures::stream::iter(events))
            .map(|e| e.into_with_event_time(fake_time))
            .collect()
            .await;

        assert_eq!(
            actual,
            vec![
                new_keyboard_input_event(Key::A, KeyEventType::Pressed),
                new_keyboard_input_event(Key::A, KeyEventType::Cancel)
                    .into_with_device_descriptor(empty_keyboard_device_descriptor()),
                new_keyboard_input_event(Key::B, KeyEventType::Pressed).into_handled(),
                new_keyboard_input_event(Key::B, KeyEventType::Cancel).into_handled(),
                new_keyboard_input_event(Key::A, KeyEventType::Cancel).into_handled(),
                new_keyboard_input_event(Key::C, KeyEventType::Pressed),
            ]
        );
    }

    #[fuchsia::test]
    async fn keyboard_cancel_before_ownership_task_clears_key_state() {
        let (test_event, handler_event) = EventPair::create();
        let (handler_sender, mut test_receiver) = mpsc::unbounded::<Vec<InputEvent>>();
        let (loop_done_sender, mut loop_done) = mpsc::unbounded::<()>();

        // Start PlatformOwned
        test_event
            .signal_peer(Signals::empty(), *DISPLAY_PLATFORM_OWNED | *INPUT_PLATFORM_OWNED)
            .unwrap();

        let handler = DisplayOwnership::new_for_test(
            handler_event,
            loop_done_sender,
            metrics::MetricsLogger::default(),
        );

        // 1. Press Key::A while PlatformOwned.
        let press_event = new_keyboard_input_event(Key::A, KeyEventType::Pressed);
        let handled = handler
            .clone()
            .handle_unhandled_input_event(UnhandledInputEvent::try_from(press_event).unwrap())
            .await;
        assert_eq!(handled[0].handled, input_device::Handled::No);

        // 2. Signal ownership loss on test_event, BUT do NOT start handle_ownership_change yet!
        test_event.signal_peer(*DISPLAY_PLATFORM_OWNED, *DISPLAY_UNOWNED).unwrap();

        // 3. Send Cancel event for Key::A. Real-time signal is unowned, but handler.ownership is PlatformOwned.
        let cancel_event = new_keyboard_input_event(Key::A, KeyEventType::Cancel);
        let handled = handler
            .clone()
            .handle_unhandled_input_event(UnhandledInputEvent::try_from(cancel_event).unwrap())
            .await;
        assert_eq!(handled[0].handled, input_device::Handled::Yes);

        // 4. Now run handle_ownership_change task. It processes ownership loss, emits Cancel for Key::A,
        // and drains pending_released_keys.
        let handler_clone = handler.clone();
        let _task = fasync::Task::local(async move {
            handler_clone.handle_ownership_change(handler_sender).await.unwrap();
        });
        loop_done.next().await;

        let cancel_events = test_receiver.next().await.expect("cancel events");
        assert_eq!(cancel_events.len(), 1);
        if let input_device::InputDeviceEvent::Keyboard(ref k) = cancel_events[0].device_event {
            assert_eq!(k.get_event_type(), KeyEventType::Cancel);
            assert_eq!(k.get_key(), Key::A);
        } else {
            panic!("expected keyboard cancel event");
        }

        // 5. Restore PlatformOwned. Since Key::A was in pending_released_keys, key_state is empty!
        test_event.signal_peer(*DISPLAY_UNOWNED, *DISPLAY_PLATFORM_OWNED).unwrap();
        loop_done.next().await;

        // No sync event should be emitted!
        assert!(test_receiver.try_recv().is_err());
    }
}
