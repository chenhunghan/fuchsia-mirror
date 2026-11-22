// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! UFS device, link, and PHY power state machine ([`PowerControl`] transitions) and the background
//! [`PowerWorker`] that serializes system suspend and resume commands.
//!
//! The power state lives in `RequestList`, under the same lock that client IO submission takes.
//! [`PowerControl::lock`] hands out a [`PowerGuard`], the exclusive right to read a stable power
//! state or run a transition. [`PowerWorker`] runs every transition while holding that guard, so
//! transitions never overlap.
//!
//! Each transition carries its numbered step list on its method comment:
//!
//! - `Active -> SystemSleep`: [`PowerGuard::suspend`] -> [`PowerGuard::enter_suspend`] ->
//!   [`PowerGuard::enter_sleep`].
//! - `Active -> PowerDown`: [`PowerGuard::suspend`] -> [`PowerGuard::enter_suspend`] ->
//!   [`PowerGuard::enter_power_down`].
//! - `SystemSleep -> Active`: [`PowerGuard::resume`] -> [`PowerGuard::exit_sleep`] +
//!   [`PowerGuard::finish_resume`].
//! - `PowerDown -> Active`, and reset after a failed transition: [`PowerGuard::resume`] ->
//!   [`PowerGuard::reset_hardware`] + [`PowerGuard::finish_resume`].
//!
//! Failures: transitions do not try to undo individual steps. If draining in-flight transfers
//! times out, nothing has changed yet, so suspend reopens client IO and stays `Active`. Any other
//! suspend failure stops the transition where it is, keeps client IO paused, and marks the hardware
//! as needing a reset on the next resume. If the hardware reset during resume fails, resume panics
//! so the system reboots rather than hanging client IO.

use std::future::Future;
use std::ops::{Deref, DerefMut};
use std::pin::pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;

use fidl_fuchsia_hardware_ufs as fufs;
use fidl_fuchsia_hardware_ufs_phy as fufs_phy;
use fuchsia_async as fasync;
use fuchsia_inspect::{NumericProperty, Property};
use futures::future::{Either, select};
use futures::lock::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};
use log::{debug, error, warn};
use mmio::Mmio;

use crate::UfsError;
use crate::constants::{
    HIBERNATE_TIMEOUT, HIGHEST_ACTIVE_ICC_LEVEL, POWER_STEP_STALL_REPEAT, POWER_STEP_STALL_WARN,
    UFS_DEVICE_WLUN,
};
use crate::controller::UfsController;
use crate::device_manager::{DeviceManager, UfsPowerMode};
use crate::inspect::{LinkState, record_elapsed_us};
use crate::query::{Attribute, Flag};
use crate::scsi::{ScsiOpcode, StartStopPowerCondition};
use crate::transfer_request_processor::TransferRequestProcessor;

/// Mutually exclusive device, link, and PHY power state managed by [`PowerControl`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Device active, link active or in hardware auto-hibernate, client IO open.
    Active,
    /// System suspend in device sleep (`PowerDown` unsupported), client IO paused.
    SystemSleep,
    /// System suspend with controller and rails off, client IO paused.
    PowerDown,
}

impl PowerState {
    /// Returns the device power mode, the `START STOP UNIT` power condition, and the link state
    /// that this state puts the hardware in.
    pub fn device_and_link_states(self) -> (UfsPowerMode, StartStopPowerCondition, LinkState) {
        use StartStopPowerCondition as Condition;
        match self {
            Self::Active => (UfsPowerMode::Active, Condition::Active, LinkState::Active),
            Self::SystemSleep => (UfsPowerMode::Sleep, Condition::Sleep, LinkState::Hibernate),
            Self::PowerDown => (UfsPowerMode::PowerDown, Condition::PowerDown, LinkState::Off),
        }
    }
}

/// The driver's power state, stored in `RequestList`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DevicePowerState {
    pub mode: PowerState,
    /// Blocks client IO submission: set across an active suspend transition (while `mode` is
    /// still `Active` so in-flight transfers drain cleanly) and whenever `mode` is not `Active`.
    pub io_paused: bool,
    /// Set when a transition failed part way and left the hardware in an unknown state.
    ///
    /// The failed steps are not undone: client requests stay parked until the next resume resets
    /// the hardware from scratch.
    pub needs_reset: bool,
}

impl DevicePowerState {
    pub const ACTIVE: Self =
        Self { mode: PowerState::Active, io_paused: false, needs_reset: false };

    /// Returns the settled [`DevicePowerState`] for `mode` and `needs_reset`, pausing client IO
    /// unless `mode` is [`PowerState::Active`] and no hardware reset is pending.
    fn settled(mode: PowerState, needs_reset: bool) -> Self {
        Self { mode, io_paused: mode != PowerState::Active || needs_reset, needs_reset }
    }

    pub fn allows_client_io(self) -> bool {
        !self.io_paused
    }
}

/// Platform-specific hooks invoked around power transitions.
pub trait PowerHooks: Send + Sync {
    /// Initializes the PHY after the host controller is enabled and before link startup.
    fn init_phy(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        async { Ok(()) }
    }

    /// Drops the bus bandwidth vote after the PHY leaves `Active`.
    fn drop_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        async { Ok(()) }
    }

    /// Restores the bus bandwidth vote before the PHY returns to `Active`.
    fn restore_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
        async { Ok(()) }
    }
}

#[cfg(test)]
impl PowerHooks for () {}

fn is_valid_transition(prev: PowerState, next: PowerState) -> bool {
    !matches!(
        (prev, next),
        (PowerState::SystemSleep, PowerState::PowerDown)
            | (PowerState::PowerDown, PowerState::SystemSleep)
    )
}

fn log_transition(
    from: PowerState,
    to: PowerState,
    start: zx::MonotonicInstant,
    res: &Result<(), UfsError>,
) {
    let elapsed_us =
        u64::try_from((zx::MonotonicInstant::get() - start).into_micros()).unwrap_or(0);
    match res {
        Ok(()) => debug!(from:?, to:?, elapsed_us; "Power transition complete"),
        Err(err) => error!(from:?, to:?, elapsed_us, err:%; "Power transition failed"),
    }
}

/// Inspect `power_step` value when no external power transition step is pending.
pub const POWER_STEP_IDLE: &str = "idle";

/// A call to another driver that a power transition or bring-up waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerStep {
    BusRestore,
    BusDrop,
    PhyActive,
    PhyHibernateSleep,
    PhyPowerDown,
    PhyInit,
    PhyPrePowerModeChange,
}

impl PowerStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BusRestore => "bus_restore",
            Self::BusDrop => "bus_drop",
            Self::PhyActive => "phy_active",
            Self::PhyHibernateSleep => "phy_hibernate_sleep",
            Self::PhyPowerDown => "phy_power_down",
            Self::PhyInit => "phy_init",
            Self::PhyPrePowerModeChange => "phy_pre_power_mode_change",
        }
    }
}

impl DeviceManager {
    /// Awaits an external power step without a timeout, warning at [`POWER_STEP_STALL_WARN`] and
    /// every [`POWER_STEP_STALL_REPEAT`] while keeping Inspect `power_step` updated. Bring-up
    /// steps run before Inspect is attached, so only the warning reports them.
    ///
    /// There is no timeout: a timeout would lead to a hardware reset that makes the same calls. The
    /// warning and `power_step` name the stalled call in logs and snapshots.
    pub async fn run_power_step<F: Future>(&self, step: PowerStep, fut: F) -> F::Output {
        struct IdleOnDrop<'a>(&'a fuchsia_inspect::StringProperty);
        impl Drop for IdleOnDrop<'_> {
            fn drop(&mut self) {
                self.0.set(POWER_STEP_IDLE);
            }
        }
        let prop = &self.inspect().power_step;
        prop.set(step.as_str());
        let _reset = IdleOnDrop(prop);
        let start = fasync::MonotonicInstant::now();
        let mut fut = pin!(fut);
        let mut delay = POWER_STEP_STALL_WARN;
        loop {
            match select(fut.as_mut(), pin!(fasync::Timer::new(delay))).await {
                Either::Left((out, _)) => return out,
                Either::Right(((), _)) => {
                    let waited_s = (fasync::MonotonicInstant::now() - start).into_seconds();
                    warn!(step = step.as_str(), waited_s; "External power step stalled");
                    #[cfg(test)]
                    self.power_step_stall_warning_count.fetch_add(1, Ordering::Relaxed);
                    delay = POWER_STEP_STALL_REPEAT;
                }
            }
        }
    }

    /// Requests a power state on the attached PHY, if any, and counts failures in
    /// `link_vote_error_count`.
    ///
    /// Succeeds when no PHY is present or the PHY reports `NOT_SUPPORTED` / `UnsupportedMethod`.
    pub async fn set_phy_power_state(
        &self,
        phy: Option<&fufs_phy::UfsPhyProxy>,
        state: fufs_phy::PowerState,
        step: PowerStep,
    ) -> Result<(), UfsError> {
        let Some(phy) = phy else {
            return Ok(());
        };
        match self
            .run_power_step(step, phy.set_power_state(state))
            .await
            .map(|res| res.map_err(zx::Status::err_from_raw))
        {
            Ok(Ok(()) | Err(zx::Status::NOT_SUPPORTED))
            | Err(fidl::Error::UnsupportedMethod { .. }) => return Ok(()),
            Ok(Err(status)) => {
                warn!(target_state:? = state, status:%; "PHY power state change failed");
            }
            Err(e) => {
                warn!(
                    target_state:? = state,
                    err:% = e;
                    "Failed to reach PHY for power state change"
                );
            }
        }
        self.inspect().link_vote_error_count.add(1);
        Err(UfsError::IoError)
    }
}

struct PowerInner<H> {
    manager: DeviceManager,
    hooks: H,
}

/// Guarantees serialized power transitions and power-state access via [`Self::lock`] across the
/// device manager, processor, PHY, and hooks (only bring-up, the power worker, and `stop()` take
/// the lock). Created once at driver bring-up. Dropping it releases the shared driver references.
pub struct PowerControl<M: Mmio, H: PowerHooks> {
    inner: AsyncMutex<PowerInner<H>>,
    pub processor: Arc<TransferRequestProcessor<M>>,
    phy: Option<fufs_phy::UfsPhyProxy>,
}

impl<M: Mmio, H: PowerHooks> PowerControl<M, H> {
    pub fn new(
        manager: DeviceManager,
        processor: Arc<TransferRequestProcessor<M>>,
        phy: Option<fufs_phy::UfsPhyProxy>,
        hooks: H,
    ) -> Self {
        Self { inner: AsyncMutex::new(PowerInner { manager, hooks }), processor, phy }
    }

    pub async fn lock(&self) -> PowerGuard<'_, M, H> {
        PowerGuard {
            inner: self.inner.lock().await,
            processor: &self.processor,
            phy: self.phy.as_ref(),
        }
    }
}

/// Guarantees exclusive ownership to run a power transition, rely on a power-state snapshot, or
/// access [`DeviceManager`] through `DerefMut` without overlapping writes. Created by
/// [`PowerControl::lock`]. Dropping it releases the async power mutex so another transition or
/// teardown check can run.
pub struct PowerGuard<'a, M: Mmio, H: PowerHooks> {
    inner: AsyncMutexGuard<'a, PowerInner<H>>,
    pub processor: &'a TransferRequestProcessor<M>,
    phy: Option<&'a fufs_phy::UfsPhyProxy>,
}

impl<M: Mmio, H: PowerHooks> Deref for PowerGuard<'_, M, H> {
    type Target = DeviceManager;

    fn deref(&self) -> &Self::Target {
        &self.inner.manager
    }
}

impl<M: Mmio, H: PowerHooks> DerefMut for PowerGuard<'_, M, H> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.manager
    }
}

/// Guarantees that client IO admission stays paused while held (`PowerGuard::resume` must not
/// be called through [`DerefMut`]). Created by [`PowerGuard::pause_client_io`]. Dropping it takes
/// the synchronous `request_list` lock to restore settled admission for the recorded power state,
/// which is safe because `IoGuard` lives only on the serialized power-transition path.
struct IoGuard<'g, 'a, M: Mmio, H: PowerHooks> {
    guard: &'g mut PowerGuard<'a, M, H>,
}

impl<'a, M: Mmio, H: PowerHooks> Deref for IoGuard<'_, 'a, M, H> {
    type Target = PowerGuard<'a, M, H>;

    fn deref(&self) -> &Self::Target {
        self.guard
    }
}

impl<'a, M: Mmio, H: PowerHooks> DerefMut for IoGuard<'_, 'a, M, H> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.guard
    }
}

impl<M: Mmio, H: PowerHooks> Drop for IoGuard<'_, '_, M, H> {
    fn drop(&mut self) {
        self.guard.update_power(|cur| DevicePowerState::settled(cur.mode, cur.needs_reset));
    }
}

impl<M: Mmio, H: PowerHooks> IoGuard<'_, '_, M, H> {
    /// Waits up to `drain_inflight_timeout` for in-flight client transfers and transactions to
    /// finish.
    async fn drain(&self) -> Result<(), UfsError> {
        self.guard.processor.drain_inflight(self.guard.drain_inflight_timeout).await
    }

    /// Records `mode` and `needs_reset` while keeping client IO paused until this guard drops.
    fn set_state(&mut self, mode: PowerState, needs_reset: bool) {
        self.guard.record_state(DevicePowerState { mode, io_paused: true, needs_reset });
    }
}

impl<'a, M: Mmio, H: PowerHooks> PowerGuard<'a, M, H> {
    pub fn hooks(&self) -> &H {
        &self.inner.hooks
    }

    #[cfg(test)]
    pub fn hooks_mut(&mut self) -> &mut H {
        &mut self.inner.hooks
    }

    pub fn processor(&self) -> &'a TransferRequestProcessor<M> {
        self.processor
    }

    pub fn controller(&self) -> &'a UfsController<M> {
        self.processor.controller()
    }

    async fn drop_bus_bandwidth(&mut self) -> Result<(), UfsError> {
        let inner = &mut *self.inner;
        inner.manager.run_power_step(PowerStep::BusDrop, inner.hooks.drop_bus_bandwidth()).await
    }

    pub async fn restore_bus_bandwidth(&mut self) -> Result<(), UfsError> {
        let inner = &mut *self.inner;
        inner
            .manager
            .run_power_step(PowerStep::BusRestore, inner.hooks.restore_bus_bandwidth())
            .await
    }

    async fn init_phy(&mut self) -> Result<(), UfsError> {
        let inner = &mut *self.inner;
        inner.manager.run_power_step(PowerStep::PhyInit, inner.hooks.init_phy()).await
    }

    pub fn power(&self) -> DevicePowerState {
        self.processor.requests().power()
    }

    /// Records the settled [`DevicePowerState`] and updates power-state Inspect properties.
    pub fn set_state(&mut self, mode: PowerState, needs_reset: bool) {
        self.record_state(DevicePowerState::settled(mode, needs_reset));
    }

    fn record_state(&mut self, state: DevicePowerState) {
        self.inner.manager.sync_power_inspect(state.mode);
        let prev = self.update_power(|_| state).mode;
        debug_assert!(
            is_valid_transition(prev, state.mode),
            "invalid power state transition: {prev:?} -> {:?}",
            state.mode
        );
    }

    /// Pauses client IO admission and returns an [`IoGuard`] that restores settled admission on
    /// drop.
    fn pause_client_io(&mut self) -> IoGuard<'_, 'a, M, H> {
        self.update_power(|cur| DevicePowerState { io_paused: true, ..cur });
        IoGuard { guard: self }
    }

    /// Requests a power state on the attached PHY, if any, and counts failures in
    /// `link_vote_error_count`.
    pub async fn set_phy_power_state(
        &self,
        state: fufs_phy::PowerState,
        step: PowerStep,
    ) -> Result<(), UfsError> {
        self.inner.manager.set_phy_power_state(self.phy, state, step).await
    }

    /// Prepares the device and controller for a low-power state while the link is active.
    ///
    /// 1. Fail if the admin slot is quarantined, since `START STOP UNIT` needs it.
    /// 2. Clear active idle WriteBooster flush (`fWBBufferFlushEn`) so background flash migration
    ///    does not block low-power entry or draw sleep current.
    /// 3. Clear `fBackgroundOpsEn` while query commands can still run.
    /// 4. Disable hardware auto-hibernate, and wait for UIC command readiness so any in-flight
    ///    auto-hibernate transition settles.
    async fn prepare_low_power_entry(&mut self) -> Result<(), UfsError> {
        if self.is_admin_slot_timed_out() {
            warn!("Cannot suspend while the admin command slot is quarantined");
            return Err(UfsError::Timeout);
        }
        let processor = self.processor;
        // The WriteBooster buffer uses non-volatile single-level cell (SLC) flash rather than
        // volatile RAM (UFS 4.0 section 13.4.18.1), and `UFS-PowerDown` completes pending work
        // before power off (sections 7.4.1.8 and 13.4.5). Clear active idle flushing so
        // SLC-to-triple-level cell (TLC) migration does not block entry or draw sleep current.
        // The driver owns `fWBBufferFlushEn`, as it owns `fBackgroundOpsEn`: nothing sets it again
        // on resume, so a client's change through a flag query holds only until the next suspend.
        // TODO(https://fxbug.dev/588007694): Decide whether clients may write this flag.
        if self.flags.wb_flush_enabled() {
            let lun = self.write_booster_buffer_lun;
            if let Err(e) =
                self.inner.manager.clear_flag_at_index(processor, Flag::WbBufferFlushEn, lun).await
            {
                warn!(lun, err:% = e; "Failed to stop idle WriteBooster flush before suspend");
            } else {
                self.inner.manager.flags.set(fufs::FlagType::WriteBoosterBufferFlushEn, false);
            }
        }
        if let Err(e) =
            self.inner.manager.clear_flag_at_index(processor, Flag::BackgroundOpsEn, 0).await
        {
            warn!(err:% = e; "Failed to disable background operations before sleep");
        } else {
            self.inner.manager.flags.set(fufs::FlagType::BackgroundOpsEn, false);
        }
        // TODO(https://fxbug.dev/588008151): Exit auto-hibernate explicitly before clearing AHIT if
        // a controller needs it; matches the C++ driver today.
        processor.controller().disable_auto_hibernate();
        processor.controller().wait_uic_command_ready(HIBERNATE_TIMEOUT).await
    }

    /// Moves hardware from `Active` into `SystemSleep` with client IO already paused.
    ///
    /// 1. Stop WriteBooster flush, background operations, and hardware auto-hibernate.
    /// 2. Put the device in `Sleep` (`START STOP UNIT`) while the link is still active.
    /// 3. Put the link in hibernate (`DME_HIBERNATE_ENTER`) before gating PHY clocks.
    /// 4. Block controller register access, because the PHY contract forbids it in
    ///    `HibernateSleep`, then put the PHY in `HibernateSleep` and drop bus bandwidth.
    ///
    /// Stops at the first failure.
    async fn enter_sleep(&mut self) -> Result<(), UfsError> {
        self.prepare_low_power_entry().await?;
        self.send_start_stop_unit(StartStopPowerCondition::Sleep).await?;
        self.processor.controller().hibernate_enter(HIBERNATE_TIMEOUT).await?;
        self.block_register_access();
        self.set_phy_power_state(
            fufs_phy::PowerState::HibernateSleep,
            PowerStep::PhyHibernateSleep,
        )
        .await?;
        self.drop_bus_bandwidth().await
    }

    /// Moves hardware from `Active` into `PowerDown` with client IO already paused.
    ///
    /// 1. Stop WriteBooster flush, background operations, and hardware auto-hibernate.
    /// 2. Put the device in `PowerDown` (`START STOP UNIT`), which commits its cached data.
    /// 3. Put the link in hibernate (`DME_HIBERNATE_ENTER`).
    /// 4. Stop the transfer list and disable the host controller (`HCE = 0`), then set the PHY to
    ///    `PowerDown` and drop bus bandwidth.
    ///
    /// Stops at the first failure so PHY power and bus votes are never cut before the device
    /// confirms `PowerDown` and the host controller stops (`HCE = 0`).
    async fn enter_power_down(&mut self) -> Result<(), UfsError> {
        self.prepare_low_power_entry().await?;
        self.send_start_stop_unit(StartStopPowerCondition::PowerDown).await?;
        self.processor.controller().hibernate_enter(HIBERNATE_TIMEOUT).await?;
        self.stop_for_power_down().await?;
        // Drop the bus vote even if the PHY call fails, so a failed suspend does not keep it.
        let phy_res = self
            .set_phy_power_state(fufs_phy::PowerState::PowerDown, PowerStep::PhyPowerDown)
            .await;
        let bus_res = self.drop_bus_bandwidth().await;
        phy_res.and(bus_res)
    }

    /// Sends `START STOP UNIT` with `power_condition` to the UFS Device Well-Known LUN (`0xD0`) on
    /// the admin slot, bypassing client IO admission.
    async fn send_start_stop_unit(
        &self,
        power_condition: StartStopPowerCondition,
    ) -> Result<(), UfsError> {
        let cdb = ScsiOpcode::build_start_stop_unit_cdb(power_condition);
        self.processor.send_admin_scsi_command(UFS_DEVICE_WLUN, cdb).await
    }

    /// Clears post-reset Unit Attention conditions on the device well-known LUN (`0xD0`) and all
    /// bound LUNs.
    ///
    /// # Errors
    ///
    /// Fails only if `UFS_DEVICE_WLUN` stays not ready; failures on bound LUNs are logged and left
    /// for the first client command to report.
    async fn clear_unit_attentions(&self) -> Result<(), UfsError> {
        self.processor.send_admin_test_unit_ready(UFS_DEVICE_WLUN).await?;
        for &lun in &self.bound_luns {
            // Best effort: if a non-device LU is still not ready here, its first command will
            // report the condition to the caller without costing a full PHY power cycle.
            if let Err(e) = self.processor.send_admin_test_unit_ready(lun).await {
                warn!(lun, err:% = e; "Logical unit not ready after resume");
            }
        }
        Ok(())
    }

    /// Restores link and device configuration after `fDeviceInit` on `PowerDown` exit.
    ///
    /// 1. Restore `fPowerOnWPEn` if enabled before reset and currently cleared.
    /// 2. Set the reference clock (`bRefClkFreq`) before switching link gears.
    /// 3. Re-apply Intel link timing quirks, try to switch the link to high-speed mode (continuing
    ///    in PWM on failure), and restore the active current limit (`bActiveICCLevel`).
    /// 4. Re-enable WriteBooster and hibernate flush if previously active (disabling WriteBooster
    ///    if restoring either flag fails).
    pub async fn restore_config(&mut self) -> Result<(), UfsError> {
        let processor = self.processor;
        let phy = self.phy;
        let mgr = &mut self.inner.manager;
        // Power-on and hardware reset clear `fPowerOnWPEn`, leaving `bLUWriteProtect = 01h` units
        // writable (UFS 4.0 section 12.3.2). Read first and set only when 0, because the host may
        // write this flag once per power cycle or hardware reset (Table 14.25). Client IO is still
        // parked until `finish_resume`.
        if mgr.flags.power_on_wp_enabled() && !mgr.read_flag(processor, Flag::PowerOnWpEn).await? {
            mgr.set_flag(processor, Flag::PowerOnWpEn).await?;
        }
        mgr.init_reference_clock(processor).await;
        // The UniPro attribute reads are diagnostic only; on resume, only Intel hosts need the
        // peer PA_TActivate rewrite.
        if mgr.quirks.intel {
            mgr.init_unipro_attributes(processor).await?;
        }
        // Stay usable in PWM if the HS switch fails, as bring-up does. The steps below still
        // fail the reset if the link is down.
        if let Err(e) = mgr.init_uic_power_mode(processor, phy).await {
            warn!(err:% = e; "Failed to switch to HS Fast_Mode on wake, continuing in PWM");
        }
        mgr.write_attribute(processor, Attribute::ActiveIccLevel, 0, HIGHEST_ACTIVE_ICC_LEVEL)
            .await?;
        if mgr.flags.wb_enabled() {
            let lun = mgr.write_booster_buffer_lun;
            if let Err(e) = mgr.set_flag_at_index(processor, Flag::WriteBoosterEn, lun).await {
                warn!(lun, err:% = e; "Failed to re-enable WriteBooster on wake");
                mgr.disable_write_booster(processor).await;
                return Ok(());
            }
            if mgr.flags.wb_flush_hibernate_requested() {
                if let Err(e) =
                    mgr.set_flag_at_index(processor, Flag::WbBufferFlushDuringHibernate, lun).await
                {
                    warn!(lun, err:% = e; "Failed to restore WriteBooster hibernate flush on wake");
                    mgr.disable_write_booster(processor).await;
                    return Ok(());
                }
                mgr.flags.set(fufs::FlagType::WriteBoosterBufferFlushHibernate, true);
            }
        }
        Ok(())
    }

    /// Stops the host controller (`HCE = 0`) when a suspend fails.
    ///
    /// The framework cannot veto system suspend, so otherwise the controller would stay enabled
    /// across suspend with interrupts unmasked and an admin command possibly still in flight.
    /// PHY power and bus votes stay up: the device has not confirmed `Sleep` or `PowerDown`, so
    /// cutting its supply could lose cached data, and resume resets the hardware from here.
    /// Skipped once register access is blocked (PHY `HibernateSleep`) or the controller is already
    /// off, since `IE` is then masked and the registers may be unclocked. Not called after a
    /// drain timeout: requests remain in flight and data slots cannot be aborted, so the driver
    /// stays `Active`.
    async fn stop_after_failed_suspend(&mut self) {
        if !self.register_access().is_reachable() {
            return;
        }
        if let Err(e) = self.stop_for_power_down().await {
            warn!(err:% = e; "Failed to stop host controller after failed suspend");
        }
    }

    /// Pauses client IO, drains active transfers, and drives the hardware into `target`, leaving
    /// `paused` open on drain failure so IO resumes immediately.
    async fn enter_suspend(&mut self, target: PowerState) -> Result<(), UfsError> {
        let mut paused = self.pause_client_io();
        if let Err(e) = paused.drain().await {
            // Data requests cannot be aborted, so there is nothing safe to stop; suspend gives up,
            // resumes client IO, and stays `Active` while the system suspends.
            warn!(
                timeout_ms = paused.drain_inflight_timeout.into_millis(),
                err:% = e;
                "Timed out draining in-flight IO before suspend"
            );
            return Err(e);
        }
        let res = if paused.power_down_supported {
            paused.enter_power_down().await
        } else {
            paused.enter_sleep().await
        };
        if res.is_err() {
            paused.stop_after_failed_suspend().await;
        }
        paused.set_state(target, res.is_err());
        res
    }

    /// Suspends into [`PowerState::PowerDown`] (if supported) or [`PowerState::SystemSleep`].
    ///
    /// No-op when already suspended.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if in-flight client IO fails to drain, or the transition error. After a
    /// transition error, the target state is recorded with client IO paused, the hardware is
    /// marked for a reset on resume, and the host controller is stopped if still reachable.
    pub async fn suspend(&mut self) -> Result<(), UfsError> {
        let state = self.power();
        // `needs_reset` is set by a failed suspend and cleared by the `resume()` that follows,
        // which resets or panics, so it can appear before `resume()` only in `SystemSleep` or
        // `PowerDown`.
        debug_assert!(
            !state.needs_reset
                || matches!(state.mode, PowerState::SystemSleep | PowerState::PowerDown),
            "suspend called with needs_reset in unexpected mode {:?}",
            state.mode
        );
        match state.mode {
            PowerState::SystemSleep | PowerState::PowerDown => return Ok(()),
            PowerState::Active => {}
        }
        let start = zx::MonotonicInstant::get();
        let target =
            if self.power_down_supported { PowerState::PowerDown } else { PowerState::SystemSleep };
        let res = self.enter_suspend(target).await;
        log_transition(PowerState::Active, target, start, &res);
        match res {
            Ok(()) => record_elapsed_us(&self.inspect().suspend_latency_us, start),
            Err(_) => {
                self.inspect().suspend_failures.add(1);
            }
        }
        res
    }

    /// Wakes the PHY, link, and device from a clean `SystemSleep`.
    ///
    /// 1. Restore the bus bandwidth vote.
    /// 2. Set the PHY to `Active`, then allow controller register access again.
    /// 3. Exit link hibernate (`DME_HIBERNATE_EXIT`).
    /// 4. Return the device to `Active` (`START STOP UNIT`).
    ///
    /// Stops at the first failure; [`Self::resume`] then resets the hardware.
    async fn exit_sleep(&mut self) -> Result<(), UfsError> {
        self.restore_bus_bandwidth().await?;
        self.set_phy_power_state(fufs_phy::PowerState::Active, PowerStep::PhyActive).await?;
        self.allow_register_access();
        self.processor.controller().hibernate_exit(HIBERNATE_TIMEOUT).await?;
        self.send_start_stop_unit(StartStopPowerCondition::Active).await
    }

    /// Brings the host controller, PHY, link, and device up from scratch.
    ///
    /// Runs on resume from `PowerDown`, and whenever a failed transition left the hardware in an
    /// unknown state.
    /// 1. Restore bus bandwidth and PHY `Active` power, unblock register access, and stop the
    ///    transfer list and host controller if they are still running.
    /// 2. Reset and enable the host controller, initialize the PHY, and start the link.
    /// 3. Verify transport (`NOP OUT`) and run `fDeviceInit`.
    /// 4. Put the device in `Active` (`START STOP UNIT`), restore link/device settings, and clear
    ///    post-reset Unit Attentions.
    async fn reset_hardware(&mut self) -> Result<(), UfsError> {
        self.restore_bus_bandwidth().await?;
        self.set_phy_power_state(fufs_phy::PowerState::Active, PowerStep::PhyActive).await?;
        self.allow_register_access_for_reset();
        if !self.register_access().is_off() {
            self.stop_for_power_down().await?;
        }
        self.inner.manager.flags.clear_volatile_for_power_down();
        let bring_up = async {
            self.processor.controller().reset_and_enable().await?;
            self.init_phy().await?;
            self.processor.controller().link_startup().await?;
            self.reinit_after_power_down()?;
            self.processor.send_nop_out().await?;
            self.inner.manager.device_init(self.processor).await?;
            // UFS 4.0 section 7.1.6, Table 7.2 and section 14.1.4.2, Table 14.4 move the device to
            // `UFS-Sleep` after a power-on or hardware reset when `bInitPowerMode = 00h`, and
            // Table 7.4 allows only `START STOP UNIT` and `REQUEST SENSE` there, so wake to
            // `Active` before `restore_config` or `TEST UNIT READY`.
            self.send_start_stop_unit(StartStopPowerCondition::Active).await?;
            self.restore_config().await?;
            self.clear_unit_attentions().await
        }
        .await;
        if bring_up.is_err() {
            // Leave the host controller disabled (`HCE = 0`, `Off` / `OffUnconfirmed`) so it does
            // not sit enabled with an unconfigured link or device.
            let _ = self.stop_for_power_down().await;
        }
        bring_up
    }

    /// Finishes returning to `Active` after the device and link are awake.
    ///
    /// Re-enables background operations and hardware auto-hibernate before opening client IO
    /// admission. The driver owns `fBackgroundOpsEn` across suspend, so it sets the flag
    /// unconditionally: a client's change through a flag query holds only until the next suspend.
    // TODO(https://fxbug.dev/42075643): Handle the URGENT_BKOPS exception event
    // (UFS 4.0 section 13.4.4).
    async fn finish_resume(&mut self) {
        let processor = self.processor;
        match self.inner.manager.set_flag(processor, Flag::BackgroundOpsEn).await {
            Ok(()) => self.inner.manager.flags.set(fufs::FlagType::BackgroundOpsEn, true),
            Err(e) => warn!(err:% = e; "Failed to re-enable background operations on wake"),
        }
        processor.controller().configure_auto_hibernate(self.auto_hibernate_timer_us);
        self.set_state(PowerState::Active, false);
    }

    /// Resumes from [`PowerState::SystemSleep`] or [`PowerState::PowerDown`] to
    /// [`PowerState::Active`].
    ///
    /// Wakes a cleanly suspended `SystemSleep` directly, and resets the hardware from scratch
    /// for `PowerDown`, after an earlier failure, or if the direct wake fails. No-op when already
    /// `Active`.
    ///
    /// # Panics
    ///
    /// Panics if `reset_hardware` fails, so that `driver_manager` reboots the system. Failing
    /// client IO instead would leave the filesystem read-only without recovering the link, and
    /// parked IO is caught only by the storage stall watchdog, which is off in `user` builds. The
    /// panic can fire with the controller still enabled when the bus or PHY vote in
    /// `reset_hardware` fails after a failed sleep exit; no client transfer is in flight then.
    pub async fn resume(&mut self) {
        let state = self.power();
        if state.mode == PowerState::Active {
            return;
        }
        let start = zx::MonotonicInstant::get();
        let mut woke = false;
        if state.mode == PowerState::SystemSleep && !state.needs_reset {
            match self.exit_sleep().await {
                Ok(()) => woke = true,
                Err(e) => warn!(err:% = e; "Wake from sleep failed; resetting the hardware"),
            }
        }
        if !woke && let Err(err) = self.reset_hardware().await {
            panic!("UFS hardware reset failed during resume from {state:?}: {err}");
        }
        self.finish_resume().await;
        record_elapsed_us(&self.inspect().wake_latency_us, start);
        log_transition(state.mode, PowerState::Active, start, &Ok(()));
    }
}

#[cfg(test)]
mod tests {
    use crate::controller::RegisterAccess;
    use std::collections::VecDeque;
    use std::pin::pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc as sync_mpsc;
    use std::task::Poll;
    use std::thread;

    use diagnostics_assertions::assert_data_tree;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_hardware_ufs as fufs;
    use fuchsia_async as fasync;
    use fuchsia_inspect::{self as inspect, reader as inspect_reader};
    use fuchsia_sync::Mutex;
    use futures::channel::oneshot;
    use futures::{TryStreamExt, poll};
    use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
    use zerocopy::IntoBytes;

    use super::*;
    use crate::constants::{AUTO_HIBERNATE_TIMER_US, START_STOP_POWER_CONDITION_SHIFT};
    use crate::query::{QueryOpcode, RefClkFreq};
    use crate::quirks::HostQuirks;
    use crate::registers::{
        AutoHibernateIdleTimerReg, CapabilityReg, HostControllerEnableReg, InterruptEnableReg,
        InterruptStatusReg, PowerModeStatus, UtrListRunStopReg,
    };
    use crate::request_list::SlotState;
    use crate::scsi::{ScsiOpcode, StartStopPowerCondition};
    use crate::testing::fake_ufs::{
        CAP_AUTO_HIBERNATE_SUPPORT, HCS_DEVICE_PRESENT, complete_uic_command,
        hcs_power_mode_status, map_vmo, uic_hardware, uic_opcode, wait_until,
    };
    use crate::testing::{
        ADMIN_SLOT, MMIO_SIZE, OCS_SUCCESS, PAGE_SIZE, answer_admin_query, answer_admin_query_val,
        answer_link_startup_and_nop_out, answer_start_stop_unit, answer_system_suspend,
        complete_power_down_reinit, create_test_processor, create_test_processor_with_power_domain,
        expire_deadline, fake_phy, fake_utp, io_callback, reject_admin_query, scsi_response,
        ufs_histogram_samples, wait_async, wait_for_any_doorbell,
    };
    use crate::transfer_request_processor::DataTransfer;
    use crate::uic::UicCommandOpcode;
    use crate::upiu::CommandUpiu;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum PowerEvent {
        InitPhy,
        DropBandwidth,
        RestoreBandwidth,
        PhyState(fufs_phy::PowerState),
    }

    #[derive(Default)]
    struct TestPowerHooks {
        events: Arc<Mutex<Vec<PowerEvent>>>,
        init_phy_called: bool,
        drop_bandwidth_called: bool,
        drop_bandwidth_err: Option<UfsError>,
        restore_bandwidth_called: bool,
        /// Results of successive `restore_bus_bandwidth` calls; `Ok` once empty.
        restore_bandwidth_results: VecDeque<Result<(), UfsError>>,
    }

    impl TestPowerHooks {
        fn reset_calls(&mut self) {
            self.init_phy_called = false;
            self.drop_bandwidth_called = false;
            self.restore_bandwidth_called = false;
        }
    }

    impl PowerHooks for TestPowerHooks {
        fn init_phy(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
            async {
                self.init_phy_called = true;
                self.events.lock().push(PowerEvent::InitPhy);
                Ok(())
            }
        }

        fn drop_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
            async {
                self.drop_bandwidth_called = true;
                self.events.lock().push(PowerEvent::DropBandwidth);
                match self.drop_bandwidth_err {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        }

        fn restore_bus_bandwidth(&mut self) -> impl Future<Output = Result<(), UfsError>> + Send {
            async {
                self.restore_bandwidth_called = true;
                self.events.lock().push(PowerEvent::RestoreBandwidth);
                self.restore_bandwidth_results.pop_front().unwrap_or(Ok(()))
            }
        }
    }

    #[fuchsia::test]
    async fn test_sleep_suspend_and_resume_ordering() {
        let phy_powered = Arc::new(AtomicBool::new(true));
        let (mmio_vmo, processor) =
            create_test_processor_with_power_domain(Arc::clone(&phy_powered));
        processor
            .controller()
            .with_mmio(|mmio| mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT));
        processor.controller().configure_auto_hibernate(AUTO_HIBERNATE_TIMER_US);
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal);
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let events = Arc::new(Mutex::new(Vec::<PowerEvent>::new()));
        let events_for_phy = Arc::clone(&events);
        let phy_powered_for_phy = Arc::clone(&phy_powered);
        let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
        let phy_task = fasync::Task::spawn(async move {
            while let Some(req) = phy_stream.try_next().await.unwrap() {
                match req {
                    fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                        events_for_phy.lock().push(PowerEvent::PhyState(state));
                        phy_powered_for_phy
                            .store(state == fufs_phy::PowerState::Active, Ordering::Release);
                        responder.send(Ok(())).unwrap();
                    }
                    other => panic!("Unexpected UfsPhy request: {other:?}"),
                }
            }
        });

        let hardware = thread::spawn(move || {
            // 0. ClearFlag(WbBufferFlushEn) on admin slot before ClearFlag(BackgroundOpsEn) and
            //    START STOP UNIT.
            assert!(utp.wait_for_doorbell(ADMIN_SLOT), "ClearFlag(WbBufferFlushEn) not issued");
            let query: crate::query::QueryUpiu = utp.request(ADMIN_SLOT);
            assert_eq!(query.opcode, QueryOpcode::ClearFlag as u8);
            assert_eq!(query.idn, Flag::WbBufferFlushEn as u8);
            assert_eq!(query.index, 1);
            let mut resp = query;
            resp.header.trans_type = crate::upiu::UpiuTransactionCode::QueryResponse as u8;
            utp.complete(ADMIN_SLOT, resp.as_bytes(), OCS_SUCCESS);

            // 1. ClearFlag(BackgroundOpsEn) on admin slot.
            answer_admin_query(&mut utp, QueryOpcode::ClearFlag, Flag::BackgroundOpsEn as u8);

            // 2. START STOP UNIT (Sleep) to the Device W-LUN, before DME_HIBERNATE_ENTER.
            let ssu_slot = wait_for_any_doorbell(&utp);
            assert_eq!(
                uic_opcode(&uic_regs),
                0,
                "DME_HIBERNATE_ENTER issued before START STOP UNIT"
            );
            let cmd: CommandUpiu = utp.request(ssu_slot);
            assert_eq!(cmd.header.lun, UFS_DEVICE_WLUN);
            assert_eq!(cmd.cdb[0], ScsiOpcode::StartStopUnit as u8);
            assert_eq!(
                cmd.cdb[4] >> START_STOP_POWER_CONDITION_SHIFT,
                StartStopPowerCondition::Sleep as u8
            );
            utp.complete(ssu_slot, scsi_response(ssu_slot, 0).as_bytes(), OCS_SUCCESS);

            // 3. DME_HIBERNATE_ENTER.
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateEnter as u32),
                "DME_HIBERNATE_ENTER not issued"
            );
            complete_uic_command(&mut uic_regs, 0, 0, |is: &mut InterruptStatusReg| {
                is.set_uic_hibernate_enter_status(true);
            });

            // 4. Resume: DME_HIBERNATE_EXIT before START STOP UNIT (Active).
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateExit as u32),
                "DME_HIBERNATE_EXIT not issued"
            );
            assert_eq!(utp.doorbell(), 0, "START STOP UNIT issued before DME_HIBERNATE_EXIT");
            complete_uic_command(&mut uic_regs, 0, 0, |is: &mut InterruptStatusReg| {
                is.set_uic_hibernate_exit_status(true);
            });

            // 5. START STOP UNIT (Active) + SetFlag(BackgroundOpsEn).
            answer_start_stop_unit(&mut utp, StartStopPowerCondition::Active);
            answer_admin_query(&mut utp, QueryOpcode::SetFlag, Flag::BackgroundOpsEn as u8);
        });

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.init_inspect(&ufs_node);
        mgr.power_down_supported = false;
        mgr.flags.set(fufs::FlagType::WriteBoosterEn, true);
        mgr.flags.set(fufs::FlagType::WriteBoosterBufferFlushEn, true);
        mgr.write_booster_buffer_lun = 1;

        let hooks = TestPowerHooks { events: Arc::clone(&events), ..Default::default() };
        let power = PowerControl::new(mgr, Arc::clone(&processor), Some(phy_proxy), hooks);
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Ok(()));
        assert!(p.hooks().drop_bandwidth_called);
        assert!(!p.hooks().restore_bandwidth_called);
        assert!(!p.flags.wb_flush_enabled());
        assert_eq!(p.power().mode, PowerState::SystemSleep);
        assert!(p.power().io_paused);
        assert_eq!(
            &*events.lock(),
            &[
                PowerEvent::PhyState(fufs_phy::PowerState::HibernateSleep),
                PowerEvent::DropBandwidth,
            ]
        );
        assert_data_tree!(inspector, root: {
            ufs: contains { controller: contains { LinkState: LinkState::Hibernate as u64 } }
        });
        // The PHY contract forbids register access in HibernateSleep, so interrupts are masked
        // and the interrupt thread is parked until `allow_register_access`. Inspect `AHIT` and
        // `IE` via `raw_regs` so `FakeMmio::unpowered_accesses()` strictly checks driver accesses.
        assert_eq!(p.register_access(), RegisterAccess::Blocked);
        let raw_regs = map_vmo(&mmio_vmo, MMIO_SIZE);
        assert_eq!(AutoHibernateIdleTimerReg::read(&raw_regs).0, 0);
        assert_eq!(InterruptEnableReg::read(&raw_regs).0, 0);
        assert_eq!(processor.controller().with_mmio_unchecked(|m| m.unpowered_accesses()), 0);

        p.hooks_mut().reset_calls();
        p.resume().await;
        assert!(p.hooks().restore_bandwidth_called);
        assert!(!p.hooks().init_phy_called);
        assert!(!p.hooks().drop_bandwidth_called);
        assert_eq!(p.power().mode, PowerState::Active);
        assert!(!p.power().io_paused);
        assert_eq!(p.register_access(), RegisterAccess::Reachable);
        assert_eq!(processor.controller().with_mmio(|m| m.unpowered_accesses()), 0);
        assert_ne!(processor.controller().with_mmio(|mmio| InterruptEnableReg::read(mmio).0), 0);
        assert_data_tree!(inspector, root: {
            ufs: contains { controller: contains { LinkState: LinkState::Active as u64 } }
        });
        assert_ne!(
            processor
                .controller()
                .with_mmio_unchecked(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            0
        );
        assert_eq!(
            &*events.lock(),
            &[
                PowerEvent::PhyState(fufs_phy::PowerState::HibernateSleep),
                PowerEvent::DropBandwidth,
                PowerEvent::RestoreBandwidth,
                PowerEvent::PhyState(fufs_phy::PowerState::Active),
            ]
        );
        inspector.root().record(ufs_node);
        let hierarchy = inspect_reader::read(&inspector).await.unwrap();
        assert_eq!(ufs_histogram_samples(&hierarchy, "wake_latency_us"), 1);

        drop(p);
        drop(power);
        phy_task.await;
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_power_down_suspend_records_power_down_on_post_suspend_error() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            mmio.set_hce_stuck(true);
        });
        processor.controller().set_hce_timeout(zx::MonotonicDuration::from_millis(5));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let (phy_proxy, phy_calls, phy_task) = fake_phy(0);

        let hardware = thread::spawn(move || {
            // 1. First suspend: HCE stays stuck at 1.
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
            // 2. Resume resets the hardware once HCE is unstuck.
            complete_power_down_reinit(&mut utp, &mut uic_regs);
            // 3. Second suspend: HCE clears to 0, then dropping the bus vote fails.
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
        });

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = true;
        let power = PowerControl::new(
            mgr,
            Arc::clone(&processor),
            Some(phy_proxy),
            TestPowerHooks::default(),
        );
        let mut p = power.lock().await;

        // 1. When clearing `HCE` times out (`HCE == 1`), `enter_power_down` must not cut PHY power
        //    or bus votes (`ufs_phy.fidl` requires `HCE = 0` before `POWER_DOWN`), while still
        //    recording `PowerDown` and marking the hardware for reset.
        assert_eq!(p.suspend().await, Err(UfsError::Timeout));
        assert!(phy_calls.lock().is_empty());
        assert!(!p.hooks().drop_bandwidth_called);
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::PowerDown, io_paused: true, needs_reset: true }
        );
        assert_eq!(p.register_access(), RegisterAccess::OffUnconfirmed);

        processor.controller().with_mmio_unchecked(|mmio| mmio.set_hce_stuck(false));
        p.hooks_mut().reset_calls();
        p.resume().await;
        assert_eq!(p.power().mode, PowerState::Active);

        // 2. Simulate the bus vote failing to drop after the controller is disabled and the PHY
        //    is in `PowerDown`; `PowerDown` must still be recorded and marked for a reset, so
        //    resume performs a full re-init.
        p.hooks_mut().reset_calls();
        p.hooks_mut().drop_bandwidth_err = Some(UfsError::IoError);
        let res = p.suspend().await;
        assert!(p.hooks().drop_bandwidth_called);
        assert_eq!(res, Err(UfsError::IoError));
        assert_eq!(
            &*phy_calls.lock(),
            &[fufs_phy::PowerState::Active, fufs_phy::PowerState::PowerDown]
        );
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::PowerDown, io_paused: true, needs_reset: true }
        );
        assert_eq!(p.register_access(), RegisterAccess::Off);
        hardware.join().unwrap();

        drop(p);
        drop(power);
        phy_task.await;
    }

    #[fuchsia::test]
    async fn test_power_state_drives_client_io_admission() {
        let (_mmio_vmo, processor) = create_test_processor();
        let power = PowerControl::new(
            DeviceManager::new(HostQuirks::default()),
            Arc::clone(&processor),
            None,
            (),
        );
        let mut p = power.lock().await;
        assert!(!p.power().io_paused);

        {
            let paused = p.pause_client_io();
            assert_eq!(paused.power().mode, PowerState::Active);
            assert!(paused.power().io_paused);
        }
        assert!(!p.power().io_paused);

        p.set_state(PowerState::SystemSleep, false);
        assert!(p.power().io_paused);
        assert_eq!(p.power().mode, PowerState::SystemSleep);
        p.set_state(PowerState::Active, false);
        assert!(!p.power().io_paused);
    }

    #[fuchsia::test]
    async fn test_failed_sleep_exit_falls_back_to_reset() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        });
        processor.controller().configure_auto_hibernate(AUTO_HIBERNATE_TIMER_US);
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let (phy_proxy, phy_calls, phy_task) = fake_phy(0);

        let hardware = thread::spawn(move || {
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::Sleep);

            // Resume: DME_HIBERNATE_EXIT fails with a UIC error, so `resume` resets the hardware
            // from scratch in the same call.
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateExit as u32),
                "DME_HIBERNATE_EXIT not issued"
            );
            complete_uic_command(&mut uic_regs, 1, 0, |is: &mut InterruptStatusReg| {
                is.set_uic_hibernate_exit_status(true);
            });
            complete_power_down_reinit(&mut utp, &mut uic_regs);
        });

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        processor.controller().record_inspect(&ufs_node);
        processor.record_inspect(&ufs_node);
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.init_inspect(&ufs_node);
        let power = PowerControl::new(
            mgr,
            Arc::clone(&processor),
            Some(phy_proxy),
            TestPowerHooks::default(),
        );
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Ok(()));
        assert_eq!(p.power().mode, PowerState::SystemSleep);

        p.hooks_mut().reset_calls();
        p.resume().await;
        hardware.join().unwrap();
        assert_eq!(p.power(), DevicePowerState::ACTIVE);
        assert!(p.hooks().restore_bandwidth_called);
        assert!(p.hooks().init_phy_called);
        assert!(!p.hooks().drop_bandwidth_called);
        // The failed wake and the reset each set the PHY to `Active`.
        assert_eq!(
            &*phy_calls.lock(),
            &[
                fufs_phy::PowerState::HibernateSleep,
                fufs_phy::PowerState::Active,
                fufs_phy::PowerState::Active,
            ]
        );
        inspector.root().record(ufs_node);
        assert_data_tree!(inspector, root: {
            ufs: contains {
                auto_hibern8_error_count: 1u64,
                power_suspended: false,
            }
        });

        drop(p);
        drop(power);
        phy_task.await;
    }

    #[fuchsia::test]
    async fn test_failed_suspend_resets_hardware_on_resume() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        });
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);

        let hardware = thread::spawn(move || {
            // 1. SystemSleep attempt where DME_HIBERNATE_ENTER fails.
            answer_admin_query(&mut utp, QueryOpcode::ClearFlag, Flag::BackgroundOpsEn as u8);
            answer_start_stop_unit(&mut utp, StartStopPowerCondition::Sleep);
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateEnter as u32),
                "DME_HIBERNATE_ENTER not issued"
            );
            complete_uic_command(&mut uic_regs, 1, 0, |is: &mut InterruptStatusReg| {
                is.set_uic_hibernate_enter_status(true);
            });

            // 2. Resume resets the hardware from scratch; it starts with link startup, not
            //    DME_HIBERNATE_EXIT.
            complete_power_down_reinit(&mut utp, &mut uic_regs);

            // 3. PowerDown attempt where dropping the bus vote fails.
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
        });

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.init_inspect(&ufs_node);
        let power = PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
        let mut p = power.lock().await;

        // 1. A failed step stops the suspend without undoing earlier steps, keeps client IO paused,
        //    and marks the hardware for a reset; a repeated `suspend()` returns `Ok(())`.
        assert!(p.suspend().await.is_err());
        assert_eq!(p.suspend().await, Ok(()));
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::SystemSleep, io_paused: true, needs_reset: true }
        );
        // The framework still suspends, so the controller is stopped (`HCE = 0`) while PHY power
        // and bus votes stay up for the device, which never confirmed `Sleep`.
        assert_eq!(p.register_access(), RegisterAccess::Off);
        assert!(!processor.controller().with_mmio_unchecked(|mmio| {
            HostControllerEnableReg::read(mmio).host_controller_enable()
        }));
        assert!(!p.hooks().drop_bandwidth_called);
        assert!(p.power().io_paused);

        // 2. Resume resets the hardware instead of waking it from sleep.
        p.hooks_mut().reset_calls();
        p.resume().await;
        assert_eq!(p.power(), DevicePowerState::ACTIVE);
        assert!(p.hooks().init_phy_called);

        // 3. A failed PowerDown power-off also marks the hardware for a reset.
        p.power_down_supported = true;
        p.hooks_mut().reset_calls();
        p.hooks_mut().drop_bandwidth_err = Some(UfsError::IoError);
        assert_eq!(p.suspend().await, Err(UfsError::IoError));
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::PowerDown, io_paused: true, needs_reset: true }
        );

        inspector.root().record(ufs_node);
        assert_data_tree!(inspector, root: {
            ufs: contains {
                suspend_failures: 2u64,
            }
        });

        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_failed_sleep_entry_after_phy_sleep_leaves_registers_untouched() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| mmio.store32(HostControllerEnableReg::OFFSET, 1));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let (phy_proxy, phy_calls, phy_task) = fake_phy(0);

        let hardware = thread::spawn(move || {
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::Sleep);
            complete_power_down_reinit(&mut utp, &mut uic_regs);
        });

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = false;

        // 1. Dropping bus bandwidth fails after the PHY entered `HibernateSleep`: the controller
        //    registers may be unclocked, so suspend must not try to stop the controller.
        let hooks =
            TestPowerHooks { drop_bandwidth_err: Some(UfsError::IoError), ..Default::default() };
        let power = PowerControl::new(mgr, Arc::clone(&processor), Some(phy_proxy), hooks);
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Err(UfsError::IoError));
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::SystemSleep, io_paused: true, needs_reset: true }
        );
        assert_eq!(p.register_access(), RegisterAccess::Blocked);
        assert!(processor.controller().with_mmio_unchecked(|mmio| {
            HostControllerEnableReg::read(mmio).host_controller_enable()
        }));
        assert_eq!(&*phy_calls.lock(), &[fufs_phy::PowerState::HibernateSleep]);

        // 2. Resume wakes the PHY, then stops and resets the controller from scratch.
        p.hooks_mut().reset_calls();
        p.resume().await;
        assert_eq!(p.power().mode, PowerState::Active);
        assert!(!p.power().needs_reset);
        assert!(p.hooks().init_phy_called);
        hardware.join().unwrap();

        drop(p);
        drop(power);
        phy_task.await;
    }

    #[fuchsia::test]
    async fn test_reset_stops_controller_when_device_init_is_rejected() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| mmio.store32(HostControllerEnableReg::OFFSET, 1));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let (phy_proxy, _phy_calls, phy_task) = fake_phy(0);

        let hardware = thread::spawn(move || {
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
            answer_link_startup_and_nop_out(&mut utp, &mut uic_regs);
            reject_admin_query(&mut utp, QueryOpcode::SetFlag, Flag::DeviceInit as u8);
            assert_eq!(utp.doorbell(), 0);
        });

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = true;
        let power = PowerControl::new(
            mgr,
            Arc::clone(&processor),
            Some(phy_proxy),
            TestPowerHooks::default(),
        );
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Ok(()));
        assert_eq!(p.reset_hardware().await, Err(UfsError::IoError));
        hardware.join().unwrap();
        assert_eq!(p.register_access(), RegisterAccess::Off);

        drop(p);
        drop(power);
        phy_task.await;
    }

    #[fuchsia::test]
    #[should_panic(expected = "UFS hardware reset failed during resume from DevicePowerState { \
                    mode: SystemSleep, io_paused: true, needs_reset: false }: UFS I/O error")]
    async fn test_sleep_resume_panics_when_fallback_reset_fails() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| mmio.store32(HostControllerEnableReg::OFFSET, 1));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);

        let _hardware = thread::spawn(move || {
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::Sleep);
            assert!(
                wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateExit as u32),
                "DME_HIBERNATE_EXIT not issued"
            );
            complete_uic_command(&mut uic_regs, 1, 0, |is: &mut InterruptStatusReg| {
                is.set_uic_hibernate_exit_status(true);
            });
        });

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = false;
        let power = PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Ok(()));

        p.hooks_mut().reset_calls();
        p.hooks_mut().restore_bandwidth_results = [Ok(()), Err(UfsError::IoError)].into();
        p.resume().await;
    }

    #[fuchsia::test]
    async fn test_stuck_scheduled_or_timeout_slot_blocks_suspend() {
        let (mmio_vmo, processor) = create_test_processor();
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            let mut rsr = UtrListRunStopReg::from_raw(0);
            rsr.set_value(true);
            rsr.write(mmio);
        });
        processor.set_timeout(zx::MonotonicDuration::from_millis(5));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let _uic_regs = uic_hardware(&mmio_vmo, 0);

        // 1. Ring data slot 0 and expire its stall deadline: slot 0 stays `Scheduled` (data slots
        //    are never aborted on elapsed time), and system suspend refuses with
        //    `Err(UfsError::Timeout)` while reopening client IO.
        let mut send_fut = Box::pin(processor.send_test_unit_ready(0));
        assert_eq!(poll!(&mut send_fut), Poll::Pending);
        expire_deadline(&processor, 0);
        processor.handle_expired_slots().await;
        assert_eq!(processor.request_list().lock().slot(0).unwrap().state, SlotState::Scheduled);
        assert_eq!(processor.data_timeout_logs(), 1);

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.drain_inflight_timeout = zx::MonotonicDuration::from_millis(5);
        let power = PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
        let mut p = power.lock().await;

        assert_eq!(p.suspend().await, Err(UfsError::Timeout));
        assert_eq!(p.power().mode, PowerState::Active);
        assert!(!p.power().io_paused);

        utp.complete(0, scsi_response(0, 0).as_bytes(), OCS_SUCCESS);
        assert_eq!(send_fut.await, Ok(()));

        // 2. Ring the admin slot with `UTRLDBR[31]` stuck at `1` and expire it -> transitions to
        //    `SlotState::Timeout`; system suspend then fails.
        let mut nop_fut = Box::pin(processor.send_nop_out());
        assert_eq!(poll!(&mut nop_fut), Poll::Pending);
        expire_deadline(&processor, ADMIN_SLOT);
        processor.handle_expired_slots().await;
        assert_eq!(nop_fut.await, Err(UfsError::Timeout));
        assert_eq!(
            processor.request_list().lock().slot(ADMIN_SLOT).unwrap().state,
            SlotState::Timeout
        );

        // START STOP UNIT needs the admin slot, so suspend fails and marks the hardware for a
        // reset on resume.
        assert_eq!(p.suspend().await, Err(UfsError::Timeout));
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::SystemSleep, io_paused: true, needs_reset: true }
        );
        assert_eq!(p.register_access(), RegisterAccess::Off);
        assert_eq!(processor.busy_slot_count(), 0);
    }

    #[fuchsia::test]
    async fn test_suspend_hung_start_stop_unit_resets_hardware_on_resume() {
        for power_down_supported in [false, true] {
            let (mmio_vmo, processor) = create_test_processor();
            let mut utp = fake_utp(&mmio_vmo, &processor);
            let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
            let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
            processor.controller().with_mmio(|mmio| {
                mmio.store32(HostControllerEnableReg::OFFSET, 1);
                mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
                mmio.store32(UtrListRunStopReg::OFFSET, 1);
            });
            processor.controller().configure_auto_hibernate(AUTO_HIBERNATE_TIMER_US);

            let (ssu_rung_tx, ssu_rung_rx) = oneshot::channel();
            let (resume_tx, resume_rx) = sync_mpsc::channel();
            let hardware = thread::spawn(move || {
                answer_admin_query(&mut utp, QueryOpcode::ClearFlag, Flag::BackgroundOpsEn as u8);
                assert!(utp.wait_for_doorbell(ADMIN_SLOT));
                let cmd: CommandUpiu = utp.request(ADMIN_SLOT);
                assert_eq!(cmd.header.lun, UFS_DEVICE_WLUN);
                assert_eq!(cmd.cdb[0], ScsiOpcode::StartStopUnit as u8);
                ssu_rung_tx.send(()).unwrap();
                resume_rx.recv().unwrap();
                utp.clear_doorbell(ADMIN_SLOT);
                complete_power_down_reinit(&mut utp, &mut uic_regs);
            });

            let scope = fasync::Scope::new();
            let inspector = inspect::Inspector::default();
            let ufs_node = inspector.root().create_child("ufs");
            let mut mgr = DeviceManager::new(HostQuirks::default());
            mgr.auto_hibernate_timer_us = AUTO_HIBERNATE_TIMER_US;
            mgr.init_inspect(&ufs_node);
            mgr.power_down_supported = power_down_supported;
            inspector.root().record(ufs_node);

            let power =
                PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
            let mut p = power.lock().await;
            let suspend_res = {
                let mut suspend_fut = pin!(p.suspend());
                let mut ssu_rung_rx = pin!(ssu_rung_rx);
                loop {
                    let _ = poll!(&mut suspend_fut);
                    if poll!(&mut ssu_rung_rx).is_ready() {
                        break;
                    }
                    fasync::Timer::new(zx::MonotonicInstant::after(
                        zx::MonotonicDuration::from_millis(1),
                    ))
                    .await;
                }
                assert!(
                    wait_async(|| {
                        crate::testing::slot_state(&processor, ADMIN_SLOT) == SlotState::Scheduled
                    })
                    .await
                );
                expire_deadline(&processor, ADMIN_SLOT);
                let expire_processor = Arc::clone(&processor);
                scope.spawn(async move {
                    expire_processor.handle_expired_slots().await;
                });
                suspend_fut.await
            };

            // The suspend stops at the hung command: no link hibernate, no power-off, and the
            // hardware is marked for a reset with client IO paused.
            assert_eq!(suspend_res, Err(UfsError::Timeout));
            assert_eq!(processor.controller().with_mmio_unchecked(|mmio| mmio.utrlclr_writes()), 0);
            assert!(!p.hooks().drop_bandwidth_called);
            assert_data_tree!(inspector, root: {
                ufs: contains {
                    power_suspended: true,
                    suspend_failures: 1u64,
                }
            });
            let target =
                if power_down_supported { PowerState::PowerDown } else { PowerState::SystemSleep };
            assert_eq!(
                p.power(),
                DevicePowerState { mode: target, io_paused: true, needs_reset: true }
            );
            assert_eq!(p.register_access(), RegisterAccess::Off);
            assert_eq!(crate::testing::slot_state(&processor, ADMIN_SLOT), SlotState::Free);

            // Resume resets the controller, link, and device from scratch.
            resume_tx.send(()).unwrap();
            p.hooks_mut().reset_calls();
            p.resume().await;
            assert_eq!(p.power().mode, PowerState::Active);
            assert!(!p.power().io_paused);
            assert_eq!(crate::testing::slot_state(&processor, ADMIN_SLOT), SlotState::Free);

            let _ = p.quiesce().await;
            scope.cancel().await;
            hardware.join().unwrap();
        }
    }

    #[fuchsia::test]
    async fn test_suspend_fails_when_overdue_data_slot_remains_scheduled() {
        let (mmio_vmo, processor) = create_test_processor();
        let utp = fake_utp(&mmio_vmo, &processor);

        let (completion, _rx) = io_callback();
        let write_vmo = Arc::new(zx::Vmo::create(PAGE_SIZE).unwrap());
        processor.submit_io(
            0,
            ScsiOpcode::build_write_10_cdb(0, 1),
            Some(DataTransfer::write(write_vmo, 0, PAGE_SIZE as u32)),
            completion,
        );
        assert!(utp.wait_for_doorbell(0));
        expire_deadline(&processor, 0);
        processor.handle_expired_slots().await;
        assert_eq!(processor.data_timeout_logs(), 1);

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.init_inspect(&ufs_node);
        mgr.power_down_supported = true;
        mgr.drain_inflight_timeout = zx::MonotonicDuration::from_millis(5);
        inspector.root().record(ufs_node);

        let power = PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Err(UfsError::Timeout));
        assert_eq!(p.power().mode, PowerState::Active);
        assert!(!p.power().io_paused);
        assert_data_tree!(inspector, root: {
            ufs: contains {
                power_suspended: false,
                suspend_failures: 1u64,
            }
        });

        let _ = p.quiesce().await;
    }

    #[fuchsia::test]
    async fn test_admin_slot_timeout_fails_suspend_and_resume_resets_hardware() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        processor.controller().with_mmio(|mmio| {
            mmio.store32(HostControllerEnableReg::OFFSET, 1);
            mmio.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
            mmio.store32(UtrListRunStopReg::OFFSET, 1);
        });
        processor.controller().configure_auto_hibernate(AUTO_HIBERNATE_TIMER_US);

        // Put the admin slot into SlotState::Timeout while all data slots 0..30 are Free.
        let mut nop = Box::pin(processor.send_nop_out());
        assert!(poll!(nop.as_mut()).is_pending());
        expire_deadline(&processor, ADMIN_SLOT);
        processor.handle_expired_slots().await;
        assert_eq!(nop.await, Err(UfsError::Timeout));
        assert_eq!(crate::testing::slot_state(&processor, ADMIN_SLOT), SlotState::Timeout);
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.auto_hibernate_timer_us = AUTO_HIBERNATE_TIMER_US;
        mgr.drain_inflight_timeout = zx::MonotonicDuration::from_millis(5);
        let power = PowerControl::new(mgr, Arc::clone(&processor), None, TestPowerHooks::default());
        let mut p = power.lock().await;
        assert!(p.is_admin_slot_timed_out());

        // START STOP UNIT needs the admin slot, so suspend fails before clearing AHIT and marks
        // the hardware for a reset.
        assert_eq!(p.suspend().await, Err(UfsError::Timeout));
        assert_eq!(
            p.power(),
            DevicePowerState { mode: PowerState::SystemSleep, io_paused: true, needs_reset: true }
        );
        assert_ne!(
            processor
                .controller()
                .with_mmio_unchecked(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            0
        );

        // Resume disables the controller, which reclaims the admin slot, and resets from scratch.
        utp.clear_doorbell(ADMIN_SLOT);
        let hardware = thread::spawn(move || complete_power_down_reinit(&mut utp, &mut uic_regs));
        p.resume().await;
        hardware.join().unwrap();
        assert_eq!(p.power().mode, PowerState::Active);
        assert!(!p.power().io_paused);
        assert_eq!(crate::testing::slot_state(&processor, ADMIN_SLOT), SlotState::Free);
        assert!(!p.is_admin_slot_timed_out());
    }

    #[fuchsia::test]
    async fn test_power_down_reset_propagates_restore_config_failure_and_stops_controller() {
        let (mmio_vmo, processor) = create_test_processor();
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);
        let (phy_proxy, _phy_calls, phy_task) = fake_phy(0);

        let hardware = thread::spawn(move || {
            answer_system_suspend(&mut utp, &mut uic_regs, StartStopPowerCondition::PowerDown);
            answer_link_startup_and_nop_out(&mut utp, &mut uic_regs);
            answer_admin_query(&mut utp, QueryOpcode::SetFlag, Flag::DeviceInit as u8);
            answer_admin_query(&mut utp, QueryOpcode::ReadFlag, Flag::DeviceInit as u8);
            answer_start_stop_unit(&mut utp, StartStopPowerCondition::Active);
            answer_admin_query_val(
                &mut utp,
                QueryOpcode::ReadAttribute,
                Attribute::RefClkFreq as u8,
                RefClkFreq::Freq38_4MHz as u32,
            );
            assert!(wait_until(|| uic_opcode(&uic_regs) == UicCommandOpcode::DmeGet as u32));
            complete_uic_command(&mut uic_regs, 1, 0, |_| {});
            // `init_uic_power_mode` failure continues in PWM; reject `ActiveIccLevel`
            // so `restore_config` fails and `reset_hardware` stops the controller.
            reject_admin_query(
                &mut utp,
                QueryOpcode::WriteAttribute,
                Attribute::ActiveIccLevel as u8,
            );
            assert_eq!(utp.doorbell(), 0);
        });

        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = true;
        let power = PowerControl::new(
            mgr,
            Arc::clone(&processor),
            Some(phy_proxy),
            TestPowerHooks::default(),
        );
        let mut p = power.lock().await;
        assert_eq!(p.suspend().await, Ok(()));
        let res = p.reset_hardware().await;
        hardware.join().unwrap();
        assert_eq!(res, Err(UfsError::IoError));
        assert_eq!(p.register_access(), RegisterAccess::Off);

        drop(p);
        drop(power);
        phy_task.await;
    }

    fn power_step_inspect(
        exec: &mut fasync::TestExecutor,
        inspector: &inspect::Inspector,
    ) -> String {
        let mut read_fut = pin!(inspect_reader::read(inspector));
        let Poll::Ready(Ok(hierarchy)) = exec.run_until_stalled(&mut read_fut) else {
            panic!("Inspect read stalled unexpectedly");
        };
        hierarchy
            .get_child("ufs")
            .and_then(|ufs| ufs.get_property("power_step"))
            .and_then(|prop| prop.string())
            .expect("missing root/ufs/power_step")
            .to_string()
    }

    #[fuchsia::test]
    fn test_run_power_step_inspect_and_stall_warnings() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let t0 = fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get());
        exec.set_fake_time(t0);

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.init_inspect(&ufs_node);
        inspector.root().record(ufs_node);
        let warnings = &mgr.power_step_stall_warning_count;

        assert_eq!(power_step_inspect(&mut exec, &inspector), POWER_STEP_IDLE);

        let (tx, rx) = oneshot::channel::<i32>();
        let mut fut = pin!(mgr.run_power_step(PowerStep::PhyActive, rx));
        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(power_step_inspect(&mut exec, &inspector), "phy_active");
        assert_eq!(warnings.load(Ordering::Acquire), 0);

        exec.set_fake_time(
            t0 + (POWER_STEP_STALL_WARN - zx::MonotonicDuration::from_millis(1)).into(),
        );
        assert!(!exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(warnings.load(Ordering::Acquire), 0);

        exec.set_fake_time(t0 + POWER_STEP_STALL_WARN.into());
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(warnings.load(Ordering::Acquire), 1);

        exec.set_fake_time(t0 + (POWER_STEP_STALL_WARN + POWER_STEP_STALL_REPEAT).into());
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(warnings.load(Ordering::Acquire), 2);
        assert_eq!(power_step_inspect(&mut exec, &inspector), "phy_active");

        tx.send(42).unwrap();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(42)));
        assert_eq!(power_step_inspect(&mut exec, &inspector), POWER_STEP_IDLE);

        // Dropping a pending step still resets Inspect `power_step` to `idle`.
        {
            let mut dropped =
                pin!(mgr.run_power_step(PowerStep::BusDrop, std::future::pending::<()>()));
            assert!(exec.run_until_stalled(&mut dropped).is_pending());
            assert_eq!(power_step_inspect(&mut exec, &inspector), "bus_drop");
        }
        assert_eq!(power_step_inspect(&mut exec, &inspector), POWER_STEP_IDLE);
        assert_eq!(fasync::TestExecutor::next_timer(), None);
    }

    #[fuchsia::test]
    fn test_power_down_stall_reports_power_step() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let t0 = fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get());
        exec.set_fake_time(t0);

        let phy_powered = Arc::new(AtomicBool::new(true));
        let (mmio_vmo, processor) =
            create_test_processor_with_power_domain(Arc::clone(&phy_powered));
        let mut utp = fake_utp(&mmio_vmo, &processor);
        let hcs = hcs_power_mode_status(PowerModeStatus::PowerLocal) | HCS_DEVICE_PRESENT;
        let mut uic_regs = uic_hardware(&mmio_vmo, hcs);

        let (phy_tx, phy_rx) = oneshot::channel::<Result<(), i32>>();
        let phy_gate = Arc::new(Mutex::new(Some(phy_rx)));
        let phy_gate_srv = Arc::clone(&phy_gate);
        let phy_powered_srv = Arc::clone(&phy_powered);
        let (phy_proxy, mut phy_stream) = create_proxy_and_stream::<fufs_phy::UfsPhyMarker>();
        let _phy_task = fasync::Task::local(async move {
            while let Some(req) = phy_stream.try_next().await.unwrap() {
                match req {
                    fufs_phy::UfsPhyRequest::SetPowerState { state, responder } => {
                        let rx = phy_gate_srv.lock().take().unwrap();
                        let res = rx.await.unwrap();
                        if res.is_ok() {
                            phy_powered_srv
                                .store(state == fufs_phy::PowerState::Active, Ordering::Release);
                        }
                        let _ = responder.send(res);
                    }
                    other => panic!("Unexpected UfsPhy request: {other:?}"),
                }
            }
        });

        let inspector = inspect::Inspector::default();
        let ufs_node = inspector.root().create_child("ufs");
        let mut mgr = DeviceManager::new(HostQuirks::default());
        mgr.power_down_supported = true;
        mgr.init_inspect(&ufs_node);
        inspector.root().record(ufs_node);
        let warnings = Arc::clone(&mgr.power_step_stall_warning_count);

        let power = PowerControl::new(
            mgr,
            Arc::clone(&processor),
            Some(phy_proxy),
            TestPowerHooks::default(),
        );
        let Poll::Ready(mut p) = exec.run_until_stalled(&mut pin!(power.lock())) else {
            panic!("uncontended power lock stalled");
        };

        let mut fut = pin!(p.suspend());
        let mut uic_completed = false;
        loop {
            if exec.run_until_stalled(&mut fut).is_ready() {
                panic!("expected PowerDown suspend to stall on UfsPhy");
            }
            if utp.doorbell() != 0 {
                let mut resp: crate::query::QueryUpiu = utp.request(ADMIN_SLOT);
                if resp.header.trans_type == crate::upiu::UpiuTransactionCode::QueryRequest as u8 {
                    resp.header.trans_type = crate::upiu::UpiuTransactionCode::QueryResponse as u8;
                    utp.complete(ADMIN_SLOT, resp.as_bytes(), OCS_SUCCESS);
                } else {
                    utp.complete(
                        ADMIN_SLOT,
                        scsi_response(resp.header.task_tag, 0).as_bytes(),
                        OCS_SUCCESS,
                    );
                }
                continue;
            }
            if !uic_completed && uic_opcode(&uic_regs) == UicCommandOpcode::DmeHibernateEnter as u32
            {
                uic_completed = true;
                complete_uic_command(&mut uic_regs, 0, 0, |is| {
                    is.set_uic_command_completion_status(true);
                    is.set_uic_hibernate_enter_status(true);
                });
                if let Some(next) = fasync::TestExecutor::next_timer() {
                    exec.set_fake_time(next);
                    assert!(exec.wake_expired_timers());
                }
                continue;
            }
            break;
        }

        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(power_step_inspect(&mut exec, &inspector), "phy_power_down");

        exec.set_fake_time(fasync::MonotonicInstant::now() + POWER_STEP_STALL_WARN.into());
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut fut).is_pending());
        assert_eq!(warnings.load(Ordering::Acquire), 1);

        phy_tx.send(Ok(())).unwrap();
        assert_eq!(exec.run_until_stalled(&mut fut), Poll::Ready(Ok(())));
        assert_eq!(power_step_inspect(&mut exec, &inspector), POWER_STEP_IDLE);
        assert!(!phy_powered.load(Ordering::Acquire));
        assert_eq!(fasync::TestExecutor::next_timer(), None);
    }
}
