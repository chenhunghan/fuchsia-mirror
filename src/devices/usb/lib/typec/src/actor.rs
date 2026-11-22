// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Per-port sequential FIFO actor loop and timers.

use crate::port::{PortConfig, TypecPort};
use crate::runner::{PortState, RunnerState};
use crate::state::DebounceTimer;
use crate::typec::Typec;
use anyhow::Result;
use fuchsia_async as fasync;
use fuchsia_sync::MutexGuard;
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::AbortHandle;
use std::ops::Deref;
use std::sync::{Arc, Weak};

/// Whether a `CC_STATUS` sample came from a runtime alert or cold-boot initial sync.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CcSampleMode {
    /// Sample `CC_STATUS` on an interrupt alert.
    Alert,
    /// Synchronize port state with initial hardware `CC_STATUS`, skipping `tCCDebounce` when a
    /// Source partner already supplies VBUS.
    InitialSync,
}

/// Sequential execution context for a [`TypecPort`], constructed for an actor turn by
/// [`execute_command`].
///
/// Dereferences to `T` so [`crate::PortController`], [`Typec`], and `&self`
/// [`crate::TypecExt`] queries run directly on `self`, while actor-enqueueing
/// `self: &Arc<Self>` [`crate::TypecExt`] methods remain uncallable from inside an actor turn.
pub(crate) struct PortActor<'a, T: Typec> {
    driver: &'a Arc<T>,
    pub(crate) wake_lease: Option<&'a zx::EventPair>,
}

impl<T: Typec> Copy for PortActor<'_, T> {}

impl<T: Typec> Clone for PortActor<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Typec> Deref for PortActor<'_, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.driver
    }
}

impl<'a, T: Typec> PortActor<'a, T> {
    fn port(self) -> &'a TypecPort {
        self.driver.typec()
    }

    pub(crate) fn state(self) -> MutexGuard<'a, PortState> {
        self.driver.typec().state.lock()
    }

    /// Returns a sub-context without the turn's wake lease so helper-armed timers only retain
    /// any wake lease they already hold.
    pub(crate) const fn without_wake_lease(self) -> Self {
        Self { driver: self.driver, wake_lease: None }
    }

    /// Arms or clears `kind`'s [`TimerSlot`] on `port.scope`: clears the slot when `kind` has no
    /// active duration, retains an already-running countdown when `!kind.replaces_existing()`, or
    /// spawns a new sleeping timer that enqueues [`Command::TimerExpired`] onto the actor.
    pub(crate) fn arm_timer(self, kind: TimerKind) {
        let port = self.port();
        let (id, abort_reg, duration) = {
            let mut state = self.state();
            let Some(duration) = kind.duration_for(&state, &port.config) else {
                kind.slot_mut(&mut state.runner).clear();
                state.wake_if_idle();
                return;
            };
            if !kind.replaces_existing()
                && kind.slot_mut(&mut state.runner).retain_active(self.wake_lease)
            {
                return;
            }
            let (abort, abort_reg) = AbortHandle::new_pair();
            let id = kind.slot_mut(&mut state.runner).replace(abort, self.wake_lease);
            (id, abort_reg, duration)
        };

        let weak = Arc::downgrade(self.driver);
        let guard_weak = weak.clone();
        let debounce_gate = capture_debounce_gate(port, kind);
        let guard = Defer::new(move || {
            if let Some(driver) = guard_weak.upgrade() {
                let mut state = driver.typec().state.lock();
                if kind.slot_mut(&mut state.runner).take_if_id(id).is_some() {
                    state.wake_if_idle();
                }
            }
        });
        let timer_fut = async move {
            if duration > zx::BootDuration::ZERO {
                fasync::Timer::new(fasync::BootInstant::after(duration)).await;
            }
            await_debounce_gate(debounce_gate).await;
            let Some(driver) = weak.upgrade() else { return };
            if enqueue_command(&driver, Command::TimerExpired { kind, id }) {
                guard.defuse();
            }
        };

        port.scope.spawn(futures::future::Abortable::new(timer_fut, abort_reg).map(|_| ()));
    }
}

/// Action dispatched to the per-port actor loop on `port.scope`.
pub(crate) enum Action {
    /// Sample `CC_STATUS` for an alert or initial hardware sync.
    CcStatus(CcSampleMode),
    /// Sample VBUS to qualify a pending attach, detect Sink VBUS loss, or debounce `vSafe0V`.
    VbusChange,
}

/// Pending work item in [`RunnerState::queue`], executed in FIFO order on `port.scope`.
pub(crate) enum Command {
    Action { action: Action, wake_lease: Option<zx::EventPair>, reply: oneshot::Sender<Result<()>> },
    TimerExpired { kind: TimerKind, id: u64 },
}

/// Timer category spawned on `port.scope` and delivered through [`Command::TimerExpired`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerKind {
    /// State-machine debounce window (`tCCDebounce`, `tPDDebounce`, `tSRCDisconnect`, or
    /// `tRpValueChange`); `allow_recovery` permits one `CC_STATUS` resample recovery if
    /// `tCCDebounce` expires on a bus read error.
    Debounce { allow_recovery: bool },
    /// Post-`vSafe0V` noise filter (`vsafe0v_debounce`), keeping an active countdown across
    /// duplicate alerts.
    Vsafe0v,
    /// Safety bound on an armed VBUS discharge (`discharge_timeout`), keeping an active deadline
    /// across bounces back to `Unattached` and omitting [`PortState::is_busy`]; `allow_retry`
    /// permits re-arming once if disarming fails.
    DischargeTimeout { allow_retry: bool },
}

impl TimerKind {
    fn slot(self, runner: &RunnerState) -> &TimerSlot {
        match self {
            Self::Debounce { .. } => &runner.timer,
            Self::Vsafe0v => &runner.vsafe0v_timer,
            Self::DischargeTimeout { .. } => &runner.discharge_timer,
        }
    }

    fn slot_mut(self, runner: &mut RunnerState) -> &mut TimerSlot {
        match self {
            Self::Debounce { .. } => &mut runner.timer,
            Self::Vsafe0v => &mut runner.vsafe0v_timer,
            Self::DischargeTimeout { .. } => &mut runner.discharge_timer,
        }
    }

    fn can_fire(self, state: &PortState) -> bool {
        match self {
            Self::Debounce { .. } => !state.machine.is_disabled(),
            Self::Vsafe0v => state.can_disarm_vsafe0v(),
            Self::DischargeTimeout { .. } => state.runner.hw.vbus_discharge.is_enabled(),
        }
    }

    fn duration_for(self, state: &PortState, config: &PortConfig) -> Option<zx::BootDuration> {
        if !self.can_fire(state) {
            return None;
        }
        match self {
            Self::Debounce { .. } => state.machine.debounce_timer().map(|kind| match kind {
                DebounceTimer::CcAttach => config.cc_debounce,
                DebounceTimer::SnkExit => config.pd_debounce,
                DebounceTimer::SrcDisconnect => config.src_disconnect,
                DebounceTimer::RpValueChange => config.rp_value_change,
            }),
            Self::Vsafe0v => Some(config.vsafe0v_debounce),
            Self::DischargeTimeout { .. } => (config.discharge_timeout
                != zx::BootDuration::INFINITE)
                .then_some(config.discharge_timeout),
        }
    }

    /// Whether arming `self` replaces an already-running timer (`true`) or preserves its
    /// countdown (`false`).
    const fn replaces_existing(self) -> bool {
        matches!(self, Self::Debounce { .. })
    }
}

/// Sleeping timer handle; aborted and any held wake lease released when dropped.
struct TimerHandle {
    id: u64,
    abort: AbortHandle,
    wake_lease: Option<zx::EventPair>,
}

impl Drop for TimerHandle {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

/// Slot holding at most one active [`TimerHandle`]; replacing or clearing aborts the previous
/// timer and advances the per-slot generation ID.
pub(crate) struct TimerSlot {
    active: Option<TimerHandle>,
    next_id: u64,
}

impl TimerSlot {
    pub(crate) const fn new() -> Self {
        Self { active: None, next_id: 0 }
    }

    pub(crate) const fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(crate) fn clear(&mut self) {
        self.active = None;
    }

    pub(crate) fn take_wake_lease_and_clear(&mut self) -> Option<zx::EventPair> {
        self.active.take().and_then(|mut h| h.wake_lease.take())
    }

    fn has_id(&self, id: u64) -> bool {
        self.active.as_ref().is_some_and(|h| h.id == id)
    }

    fn replace(&mut self, abort: AbortHandle, wake_lease: Option<&zx::EventPair>) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let wake_lease = wake_lease.and_then(|l| {
            l.duplicate_handle(zx::Rights::SAME_RIGHTS)
                .inspect_err(|e| log::warn!("Failed to duplicate wake lease for timer: {e:?}"))
                .ok()
        });
        self.active = Some(TimerHandle { id, abort, wake_lease });
        id
    }

    fn retain_active(&mut self, wake_lease: Option<&zx::EventPair>) -> bool {
        let Some(handle) = &mut self.active else {
            return false;
        };
        if let Some(lease) = wake_lease {
            match lease.duplicate_handle(zx::Rights::SAME_RIGHTS) {
                Ok(dup) => handle.wake_lease = Some(dup),
                Err(e) => log::warn!("Failed to duplicate wake lease for active timer: {e:?}"),
            }
        }
        true
    }

    /// Clears the active timer if its ID matches `id`, returning `Some(wake_lease)` on match
    /// (`Some(None)` when it held no lease) or `None` if already cancelled or superseded.
    fn take_if_id(&mut self, id: u64) -> Option<Option<zx::EventPair>> {
        self.active.take_if(|h| h.id == id).map(|mut h| h.wake_lease.take())
    }
}

/// Enqueues `action` onto the per-port sequential actor and awaits its result.
pub(crate) async fn run_command<T: Typec>(
    driver: Arc<T>,
    wake_lease: Option<zx::EventPair>,
    action: Action,
) -> Result<()> {
    let (reply, rx) = oneshot::channel();
    if !enqueue_command(&driver, Command::Action { action, wake_lease, reply }) {
        return Err(anyhow::anyhow!("port async scope is closed"));
    }
    // Keep `driver` alive across `rx.await` so `run_actor`'s `Weak<T>` upgrades even when the
    // caller dropped its own `Arc<T>` after creating this `+ use<Self>` future.
    let res = rx.await.unwrap_or_else(|_| Err(anyhow::anyhow!("port async scope was closed")));
    drop(driver);
    res
}

/// Executes a single queued [`Command`] on this actor turn under its wake lease, returning the
/// caller's reply channel and result when `cmd` came from [`run_command`].
async fn execute_command<T: Typec>(
    driver: &Arc<T>,
    cmd: Command,
) -> Option<(oneshot::Sender<Result<()>>, Result<()>)> {
    match cmd {
        Command::Action { action, wake_lease, reply } => {
            let actor = PortActor { driver, wake_lease: wake_lease.as_ref() };
            let res = match action {
                Action::CcStatus(mode) => actor.handle_cc_status(mode).await,
                Action::VbusChange => actor.handle_vbus_change().await,
            };
            Some((reply, res))
        }
        Command::TimerExpired { kind, id } => {
            let (wake_lease, debounce_timer) = {
                let mut state = driver.typec().state.lock();
                let Some(wake_lease) = kind.slot_mut(&mut state.runner).take_if_id(id) else {
                    return None;
                };
                if !kind.can_fire(&state) {
                    return None;
                }
                (wake_lease, state.machine.debounce_timer())
            };
            let actor = PortActor { driver, wake_lease: wake_lease.as_ref() };
            actor.handle_timer_expired(kind, debounce_timer).await;
            None
        }
    }
}

/// Sequential actor loop running on `port.scope`.
async fn run_actor<T: Typec>(weak: Weak<T>, scope_guard: Defer<impl FnOnce()>) {
    let mut next_cmd = None;
    loop {
        let Some(driver) = weak.upgrade() else {
            scope_guard.defuse();
            return;
        };
        let Some(cmd) =
            next_cmd.take().or_else(|| driver.typec().state.lock().pop_or_retire_actor())
        else {
            scope_guard.defuse();
            return;
        };
        let reply = execute_command(&driver, cmd).await;
        // Pop the next queued command, or retire `actor_running` and wake idle waiters before
        // replying so `wait_for_debounce()` observes an idle port as soon as a caller resolves.
        next_cmd = driver.typec().state.lock().pop_or_retire_actor();
        drop(driver);
        if let Some((reply, res)) = reply {
            // Receiver dropped if the caller cancelled its `run_command` future.
            let _ = reply.send(res);
        }
        if next_cmd.is_none() {
            scope_guard.defuse();
            return;
        }
    }
}

/// Enqueues `cmd` onto the port actor in FIFO order, lazily spawning the actor on `port.scope`
/// when idle.
fn enqueue_command<T: Typec>(driver: &Arc<T>, cmd: Command) -> bool {
    let port = driver.typec();
    if port.scope.is_cancelled() {
        return false;
    }
    let mut state = port.state.lock();
    if let Command::TimerExpired { kind, id } = &cmd
        && (!kind.slot(&state.runner).has_id(*id) || !kind.can_fire(&state))
    {
        return false;
    }
    if state.runner.actor_running {
        state.runner.queue.push_back(cmd);
        return true;
    }
    state.runner.queue.push_back(cmd);
    state.runner.actor_running = true;
    drop(state);

    let weak = Arc::downgrade(driver);
    let scope_weak = weak.clone();
    let scope_guard = Defer::new(move || {
        if let Some(driver) = scope_weak.upgrade() {
            driver.typec().state.lock().reset_on_scope_drop();
        }
    });
    port.scope.spawn(run_actor(weak, scope_guard));
    true
}

/// Runs a cleanup closure on drop unless [`Self::defuse`] is called.
struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Defer<F> {
    fn new(f: F) -> Self {
        Self(Some(f))
    }

    fn defuse(mut self) {
        self.0 = None;
    }
}

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

#[cfg(not(test))]
fn capture_debounce_gate(_port: &TypecPort, _kind: TimerKind) {}

#[cfg(not(test))]
async fn await_debounce_gate((): ()) {}

#[cfg(test)]
impl TimerSlot {
    pub(crate) fn id(&self) -> Option<u64> {
        self.active.as_ref().map(|h| h.id)
    }
}

#[cfg(test)]
fn capture_debounce_gate(
    port: &TypecPort,
    kind: TimerKind,
) -> Option<Arc<futures::lock::Mutex<()>>> {
    (!matches!(kind, TimerKind::DischargeTimeout { .. }))
        .then(|| port.debounce_gate.get().cloned())
        .flatten()
}

#[cfg(test)]
async fn await_debounce_gate(gate: Option<Arc<futures::lock::Mutex<()>>>) {
    if let Some(gate) = gate {
        drop(gate.lock().await);
    }
}
