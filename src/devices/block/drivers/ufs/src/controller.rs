// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Low-level UFS Host Controller Interface (UFSHCI) MMIO operations.
//! Manages controller enablement, interrupt configuration, auto-hibernate, and UIC DME execution.
//!
//! [`UfsController`] owns the register block behind a private lock, so it can be shared freely
//! (`Arc<UfsController<M>>`) between the transfer request processor, the interrupt handler, and
//! administrative callers. The lock is held only around individual register accesses: every
//! `async` method here releases it before awaiting, so no caller can block the dispatcher
//! thread on it.

use crate::constants::{
    AUTO_HIBERNATE_DISABLED, AUTO_HIBERNATE_MAX_TIMER_SCALE, AUTO_HIBERNATE_MAX_TIMER_VALUE,
    HOST_CONTROLLER_TIMEOUT, ISR_ERROR_LOG_INTERVAL, LINK_STARTUP_ATTEMPTS,
    LINK_STARTUP_RETRY_DELAY, PA_PWRMODE, UIC_DATA_LINK_LAYER_PA_INIT_ERROR,
    UIC_PHY_ADAPTER_LAYER_LINE_RESET,
};
use crate::power::{PowerGuard, PowerHooks};
use crate::registers::{
    AutoHibernateIdleTimerReg, CapabilityReg, HostControllerEnableReg, HostControllerStatusReg,
    HostUicErrorCodeDataLinkLayerReg, HostUicErrorCodeDmeReg, HostUicErrorCodeNetworkLayerReg,
    HostUicErrorCodePhyAdapterLayerReg, HostUicErrorCodeTransportLayerReg, InterruptEnableReg,
    InterruptStatusReg, PowerModeStatus, UtrListDoorBellReg, VersionReg,
};
use crate::uic::{UicCommand, UicCommandOpcode};
use crate::{UfsError, uic};
use fuchsia_async as fasync;
use fuchsia_inspect::{Inspector, Node as InspectNode};
use fuchsia_sync::Mutex;
use log::{debug, error, info, warn};
use mmio::{Mmio, ReadableRegister, Register, WritableRegister};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Host controller register accessibility and controller off/quarantine state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterAccess {
    /// Controller power and PHY clocks are on; MMIO access is allowed.
    Reachable,
    /// PHY is in `HibernateSleep` (clocks off while `HCE = 1`); MMIO is forbidden.
    Blocked,
    /// Host controller is stopped (`HCE = 0` confirmed) and performs no DMA.
    Off,
    /// `HCE = 0` was requested but not read back (stop in progress, timed out, or PHY unreachable).
    ///
    /// The controller may still perform DMA, so buffers stay pinned or are quarantined.
    OffUnconfirmed,
}

impl RegisterAccess {
    /// Returns whether MMIO register access is currently permitted.
    pub const fn is_reachable(self) -> bool {
        matches!(self, Self::Reachable)
    }

    /// Returns whether the controller is stopped (`Off` or `OffUnconfirmed`).
    pub const fn is_off(self) -> bool {
        matches!(self, Self::Off | Self::OffUnconfirmed)
    }

    /// Maps a stopped/quarantined state to its latched stop result, returning `None` when the
    /// controller is still running.
    pub const fn latched_stop_result(self) -> Option<Result<(), UfsError>> {
        match self {
            Self::Off => Some(Ok(())),
            Self::OffUnconfirmed => Some(Err(UfsError::Timeout)),
            Self::Reachable | Self::Blocked => None,
        }
    }
}

/// Register block and UIC ownership state, guarded by [`UfsController::state`].
struct ControllerState<M: Mmio> {
    mmio: M,
    /// Set while a UIC command owns the UIC registers.
    uic_busy: bool,
    /// Number of active [`AutoHibernateHoldGuard`]s holding auto-hibernate disabled across
    /// multi-DME sequences.
    ah8_hold_count: u32,
    /// Configured `AHIT` register value (`0` when auto-hibernate is disabled). Cleared on hardware
    /// while `uic_busy` is true or `ah8_hold_count > 0`.
    ah8_timer_raw: u32,
    /// Set once `configure_auto_hibernate` has logged the initial `AHIT` timer at `INFO` level so
    /// subsequent runtime `Sleep`/wake cycles log at `DEBUG` instead of waking `archivist`/`fxfs`.
    ah8_logged: bool,
    /// Monotonic timestamp (ns) of the most recent ISR error log/register dump, used to rate-limit
    /// log formatting under noisy link conditions while atomic counters continue incrementing.
    last_isr_error_log_ns: Option<i64>,
    /// ISR error bursts suppressed since `last_isr_error_log_ns` and reported on the next dump.
    suppressed_isr_errors: u64,
    /// Current register reachability and host-controller off/quarantine state.
    access: RegisterAccess,
}

impl<M: Mmio> ControllerState<M> {
    /// Returns a shared reference to MMIO, asserting registers are currently reachable.
    fn mmio(&self) -> &M {
        debug_assert!(
            self.access.is_reachable(),
            "MMIO read attempted while controller register access is {:?}",
            self.access
        );
        &self.mmio
    }

    /// Returns a mutable reference to MMIO, asserting registers are currently reachable.
    fn mmio_mut(&mut self) -> &mut M {
        debug_assert!(
            self.access.is_reachable(),
            "MMIO write attempted while controller register access is {:?}",
            self.access
        );
        &mut self.mmio
    }

    fn is_ah8_enabled(&self) -> bool {
        self.access.is_reachable()
            && !self.uic_busy
            && self.ah8_hold_count == 0
            && self.ah8_timer_raw != 0
    }

    /// Writes `HCE = enable`.
    fn write_hce(&mut self, enable: bool) {
        let mut hce = HostControllerEnableReg::from_raw(0);
        hce.set_host_controller_enable(enable);
        hce.write(self.mmio_mut());
    }

    /// Writes `IE = 0`.
    fn disable_interrupts(&mut self) {
        InterruptEnableReg::from_raw(0).write(self.mmio_mut());
    }

    /// Writes the standard controller interrupt mask to `IE`.
    fn enable_interrupts(&mut self) {
        let ah8_enabled = self.is_ah8_enabled();
        let mut enable = InterruptEnableReg::from_raw(0);
        enable.set_utp_transfer_request_completion_enable(true);
        enable.set_uic_error_enable(true);
        enable.set_uic_hibernate_exit_status_enable(ah8_enabled);
        enable.set_uic_hibernate_enter_status_enable(ah8_enabled);
        enable.set_uic_link_lost_status_enable(true);
        enable.set_utp_error_enable(true);
        enable.set_device_fatal_error_enable(true);
        enable.set_host_controller_fatal_error_enable(true);
        enable.set_system_bus_fatal_error_enable(true);
        enable.set_crypto_engine_fatal_error_enable(true);
        enable.write(self.mmio_mut());
    }

    /// Enables or disables the `UHESE` and `UHXSE` interrupt bits in `IE` when controller
    /// interrupts are currently enabled (`IE != 0`).
    fn set_ah8_interrupts(&mut self, enabled: bool) {
        let mut enable = InterruptEnableReg::read(self.mmio());
        if enable.0 == 0 {
            return;
        }
        enable.set_uic_hibernate_enter_status_enable(enabled);
        enable.set_uic_hibernate_exit_status_enable(enabled);
        enable.write(self.mmio_mut());
    }

    /// Disables auto-hibernate on hardware (`AHIT = 0`, `IE.UHESE = 0`, `IE.UHXSE = 0`).
    fn disable_ah8(&mut self) {
        self.set_ah8_interrupts(false);
        AutoHibernateIdleTimerReg::from_raw(0).write(self.mmio_mut());
    }

    /// Restores auto-hibernate on hardware (`AHIT` and `IE.UHESE`/`IE.UHXSE`) if configured and
    /// no UIC command or hold guard is active.
    fn restore_ah8(&mut self) {
        if self.is_ah8_enabled() {
            let ahit = self.ah8_timer_raw;
            AutoHibernateIdleTimerReg::from_raw(ahit).write(self.mmio_mut());
            self.set_ah8_interrupts(true);
        }
    }

    /// Reads (and thereby clears) the five UIC error code registers (`UECPA..UECDME`) once.
    fn read_uic_error_regs(&self) -> (u32, u32, u32, u32, u32) {
        let mmio = self.mmio();
        let pa = HostUicErrorCodePhyAdapterLayerReg::read(mmio).0;
        let dl = HostUicErrorCodeDataLinkLayerReg::read(mmio).0;
        let n = HostUicErrorCodeNetworkLayerReg::read(mmio).0;
        let t = HostUicErrorCodeTransportLayerReg::read(mmio).0;
        let dme = HostUicErrorCodeDmeReg::read(mmio).0;
        (pa, dl, n, t, dme)
    }

    /// Classifies a `(UECPA..UECDME)` snapshot as fatal (PA error other than line reset, DL
    /// `PA_INIT_ERROR`, NL, TL, or DME).
    fn is_fatal_uic_error(uec_regs: (u32, u32, u32, u32, u32)) -> bool {
        let (pa, dl, n, t, dme) = uec_regs;
        let pa = HostUicErrorCodePhyAdapterLayerReg(pa);
        let dl = HostUicErrorCodeDataLinkLayerReg(dl);
        let n = HostUicErrorCodeNetworkLayerReg(n);
        let t = HostUicErrorCodeTransportLayerReg(t);
        let dme = HostUicErrorCodeDmeReg(dme);
        (pa.uic_phy_adapter_layer_error()
            && pa.uic_phy_adapter_layer_error_code() != UIC_PHY_ADAPTER_LAYER_LINE_RESET)
            || (dl.uic_data_link_layer_error()
                && (dl.uic_data_link_layer_error_code() & UIC_DATA_LINK_LAYER_PA_INIT_ERROR) != 0)
            || n.uic_network_layer_error()
            || t.uic_transport_layer_error()
            || dme.uic_dme_error()
    }

    /// Returns `Some(suppressed_count)` when at least [`ISR_ERROR_LOG_INTERVAL`] has elapsed since
    /// the last ISR error log, or `None` after incrementing [`Self::suppressed_isr_errors`].
    fn should_log_isr_error(&mut self) -> Option<u64> {
        let now_ns = fasync::MonotonicInstant::now().into_nanos();
        let interval_ns = ISR_ERROR_LOG_INTERVAL.into_nanos();
        if self
            .last_isr_error_log_ns
            .is_some_and(|last_ns| now_ns.saturating_sub(last_ns) < interval_ns)
        {
            self.suppressed_isr_errors = self.suppressed_isr_errors.saturating_add(1);
            return None;
        }
        self.last_isr_error_log_ns = Some(now_ns);
        Some(std::mem::replace(&mut self.suppressed_isr_errors, 0))
    }

    /// Logs a snapshot of the UFSHCI status, doorbell, and UIC error registers on fatal,
    /// link-lost, or hibernate error interrupts.
    fn dump_debug_registers(
        &self,
        status: InterruptStatusReg,
        uec_regs: (u32, u32, u32, u32, u32),
        suppressed_isr_errors: u64,
    ) {
        let (uecpa, uecdl, uecn, uect, uecdme) = uec_regs;
        let mmio = self.mmio();
        let hcs = HostControllerStatusReg::read(mmio);
        let ahit = AutoHibernateIdleTimerReg::read(mmio);
        let utrldbr = UtrListDoorBellReg::read(mmio);
        error!(
            is = status.0,
            hcs = hcs.0,
            upmcrs = hcs.uic_power_mode_change_request_status(),
            ahit = ahit.0,
            utrldbr = utrldbr.0,
            uecpa,
            uecdl,
            uecn,
            uect,
            uecdme,
            suppressed_isr_errors;
            "UFS controller state dump"
        );
    }
}

/// Guarantees the interrupt thread's MMIO passes never run concurrently with a paused gate
/// (`HibernateSleep` or controller power-off). Created by [`Self::new`] when building
/// [`UfsController`]. Dropping it frees the futex word without unparking waiters (teardown
/// calls [`Self::stop`] first).
///
/// Uses one futex word instead of a mutex because the interrupt thread passes through it on
/// every interrupt, so the common path must be one atomic update with no lock and no system call.
pub struct InterruptGate {
    /// `PAUSED`, `STOPPED`, and the number of active interrupt passes in the low bits.
    ///
    /// Blocked [`Self::enter`] and [`Self::pause`] callers wait on this word as a futex so the
    /// gate needs no lock.
    // ORDERING: Every access is a read-modify-write of this one word, so flags and pass count are
    // read together. `Release` in `InterruptGuard::drop` pairs with `Acquire` in `pause` so a
    // pass's register accesses finish before `pause` returns. `Release` in `resume` and `stop`
    // pairs with `Acquire` in `try_enter` so a pass starts only when registers are reachable.
    state: zx::Futex,
}

/// Guarantees an active interrupt pass stays open so [`InterruptGate::pause`] waits before
/// registers lose clocks or power. Created by [`InterruptGate::enter`] while unpaused and
/// unstopped. Dropping it decrements the pass count (`Release`) and wakes `pause()` callers when
/// the last pass exits.
pub struct InterruptGuard<'a>(&'a InterruptGate);

impl Drop for InterruptGuard<'_> {
    fn drop(&mut self) {
        let prev = self.0.state.fetch_sub(1, Ordering::Release);
        debug_assert_ne!(prev & !InterruptGate::FLAGS, 0);
        if prev & !InterruptGate::STOPPED == InterruptGate::PAUSED | 1 {
            // Last active pass exiting while `pause()` waits.
            self.0.state.wake_all();
        }
    }
}

impl InterruptGate {
    /// Set by [`Self::pause`] until [`Self::resume`]; new passes park.
    const PAUSED: i32 = 1 << 30;
    /// Set for good by [`Self::stop`]; new and parked passes exit.
    const STOPPED: i32 = 1 << 29;
    const FLAGS: i32 = Self::PAUSED | Self::STOPPED;

    pub fn new() -> Self {
        Self { state: zx::Futex::new(0) }
    }

    fn try_enter(&self) -> Result<i32, i32> {
        self.state.try_update(Ordering::Acquire, Ordering::Acquire, |s| {
            (s & Self::FLAGS == 0).then_some(s + 1)
        })
    }

    /// Enters the interrupt service section, parking while paused and returning `None` once
    /// stopped.
    pub fn enter(&self) -> Option<InterruptGuard<'_>> {
        loop {
            match self.try_enter() {
                Ok(_) => return Some(InterruptGuard(self)),
                Err(s) if s & Self::STOPPED != 0 => return None,
                Err(s) => {
                    // Returns at once if `state` no longer equals `s`, so a `resume` or `stop`
                    // since `try_enter` is never missed.
                    let _ = self.state.wait(s, None, zx::MonotonicInstant::INFINITE);
                }
            }
        }
    }

    /// Prevents the interrupt thread from touching registers and waits for any active service
    /// to finish.
    fn pause(&self) {
        loop {
            let s = self.state.fetch_or(Self::PAUSED, Ordering::Acquire);
            if s & !Self::FLAGS == 0 {
                return;
            }
            // The last pass to exit wakes this wait. Setting `PAUSED` again on every retry keeps
            // new passes out even if a `resume` ran in between.
            let _ = self.state.wait(s | Self::PAUSED, None, zx::MonotonicInstant::INFINITE);
        }
    }

    /// Allows a paused interrupt thread to resume servicing interrupts.
    fn resume(&self) {
        if self.state.fetch_and(!Self::PAUSED, Ordering::Release) & Self::PAUSED != 0 {
            self.state.wake_all();
        }
    }

    /// Permanently unparks and exits the interrupt thread during driver stop.
    pub fn stop(&self) {
        self.state.fetch_or(Self::STOPPED, Ordering::Release);
        self.state.wake_all();
    }
}

/// UfsController encapsulates low-level MMIO interactions with the UFS controller.
pub struct UfsController<M: Mmio> {
    /// Host controller registers, UIC busy state, auto-hibernate hold count, and interrupt state.
    state: Mutex<ControllerState<M>>,
    /// Synchronizes the interrupt thread around `HibernateSleep` and controller power-down.
    interrupt_gate: Arc<InterruptGate>,
    hibernate_error_count: AtomicU64,
    uic_error_count: AtomicU64,
    fatal_error_count: AtomicU64,
    missed_interrupt_count: AtomicU64,
    #[cfg(test)]
    host_controller_timeout_ns: AtomicU64,
}

impl<M: Mmio> fmt::Debug for UfsController<M> {
    /// Reports only the lock-free fields; `state` is not read so `fmt` never blocks on the
    /// register lock.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UfsController")
            .field("hibernate_error_count", &self.hibernate_error_count())
            .field("uic_error_count", &self.uic_error_count())
            .field("fatal_error_count", &self.fatal_error_count())
            .field("missed_interrupt_count", &self.missed_interrupt_count())
            .finish_non_exhaustive()
    }
}

impl<M: Mmio> UfsController<M> {
    /// Creates a new UfsController wrapping the provided MMIO region.
    pub fn new(mmio: M) -> Self {
        Self {
            state: Mutex::new(ControllerState {
                mmio,
                uic_busy: false,
                ah8_hold_count: 0,
                ah8_timer_raw: 0,
                ah8_logged: false,
                last_isr_error_log_ns: None,
                suppressed_isr_errors: 0,
                access: RegisterAccess::Reachable,
            }),
            interrupt_gate: Arc::new(InterruptGate::new()),
            hibernate_error_count: AtomicU64::new(0),
            uic_error_count: AtomicU64::new(0),
            fatal_error_count: AtomicU64::new(0),
            missed_interrupt_count: AtomicU64::new(0),
            #[cfg(test)]
            host_controller_timeout_ns: AtomicU64::new(0),
        }
    }

    pub fn interrupt_gate(&self) -> &Arc<InterruptGate> {
        &self.interrupt_gate
    }

    /// Records static controller version and capability nodes and lazy error counters under
    /// `ufs_node`.
    pub fn record_inspect(self: &Arc<Self>, ufs_node: &InspectNode)
    where
        M: Send + Sync + 'static,
    {
        let (version, cap) = {
            let state = self.state.lock();
            let mmio = state.mmio();
            (VersionReg::read(mmio), CapabilityReg::read(mmio))
        };
        ufs_node.record_child("version", |node| {
            node.record_uint("major_version_number", u64::from(version.major_version_number()));
            node.record_uint("minor_version_number", u64::from(version.minor_version_number()));
            node.record_uint("version_suffix", u64::from(version.version_suffix()));
        });
        ufs_node.record_child("capabilities", |node| {
            node.record_bool("crypto_support", cap.crypto_support());
            node.record_bool(
                "uic_dme_test_mode_command_supported",
                cap.uic_dme_test_mode_command_supported(),
            );
            node.record_bool(
                "out_of_order_data_delivery_supported",
                cap.out_of_order_data_delivery_supported(),
            );
            node.record_bool("64_bit_addressing_supported", cap.addressing_64_bit_supported());
            node.record_bool("auto_hibernation_support", cap.auto_hibernation_support());
            node.record_uint(
                "number_of_utp_task_management_request_slots",
                u64::from(cap.number_of_utp_task_management_request_slots()),
            );
            node.record_uint(
                "number_of_outstanding_rtt_requests_supported",
                u64::from(cap.number_of_outstanding_rtt_requests_supported()),
            );
            node.record_uint(
                "number_of_utp_transfer_request_slots",
                u64::from(cap.number_of_utp_transfer_request_slots()),
            );
        });
        // A `Weak` so the Inspect tree never keeps the controller (and its MMIO mapping) alive
        // after the driver has released it.
        let this = Arc::downgrade(self);
        ufs_node.record_lazy_values("controller_error_counters", move || {
            let this = this.clone();
            Box::pin(async move {
                let inspector = Inspector::default();
                if let Some(controller) = this.upgrade() {
                    let root = inspector.root();
                    root.record_uint("uic_error_count", controller.uic_error_count());
                    root.record_uint("fatal_error_count", controller.fatal_error_count());
                    root.record_uint(
                        "auto_hibern8_error_count",
                        controller.hibernate_error_count(),
                    );
                    root.record_uint("missed_interrupt_count", controller.missed_interrupt_count());
                }
                Ok(inspector)
            })
        });
    }

    /// Returns the cumulative count of auto-hibernate and software hibernate errors.
    pub fn hibernate_error_count(&self) -> u64 {
        // ORDERING: Standalone monotonic telemetry counter read by Inspect and tests.
        self.hibernate_error_count.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of `IS.UE` UIC error interrupts observed, fatal or not.
    pub fn uic_error_count(&self) -> u64 {
        // ORDERING: Standalone monotonic telemetry counter read by Inspect and tests.
        self.uic_error_count.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of fatal interrupt status bits observed (`ULLS`, `DFES`,
    /// `HCFES`, `SBFES`, `CEFES`); several bits raised together count once each.
    pub fn fatal_error_count(&self) -> u64 {
        // ORDERING: Standalone monotonic telemetry counter read by Inspect and tests.
        self.fatal_error_count.load(Ordering::Relaxed)
    }

    /// Returns the cumulative count of overdue UTP slots completed by the timeout poll without a
    /// completion interrupt.
    pub fn missed_interrupt_count(&self) -> u64 {
        self.missed_interrupt_count.load(Ordering::Relaxed)
    }

    /// Adds `count` to the cumulative count of overdue slots completed without an interrupt.
    pub fn record_missed_interrupts(&self, count: u64) {
        self.missed_interrupt_count.fetch_add(count, Ordering::Relaxed);
    }

    #[cfg(test)]
    pub fn capabilities(&self) -> CapabilityReg {
        CapabilityReg::read(self.state.lock().mmio())
    }

    /// Runs `f` with exclusive access to the register block. `f` runs under the controller lock
    /// and must not block; the lock cannot be held across an `.await`.
    pub fn with_mmio<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        f(self.state.lock().mmio_mut())
    }

    /// Runs `f` with exclusive register access without checking [`RegisterAccess::is_reachable`],
    /// for tests inspecting mock MMIO counters after power-off or `HibernateSleep`.
    #[cfg(test)]
    pub fn with_mmio_unchecked<R>(&self, f: impl FnOnce(&mut M) -> R) -> R {
        f(&mut self.state.lock().mmio)
    }

    /// Disables all controller interrupts.
    #[cfg(test)]
    pub fn disable_interrupts(&self) {
        self.state.lock().disable_interrupts();
    }

    /// Reads and clears the active interrupt status bits it handles, logging error sources and
    /// counting auto-hibernate, UIC, link-lost, and fatal controller/device errors.
    pub fn fetch_and_clear_interrupts(&self) -> InterruptStatusReg {
        use InterruptStatusReg as Is;
        let mut state = self.state.lock();
        let uic_busy = state.uic_busy;
        let ah8_configured = state.ah8_timer_raw != 0;
        let status = Is::read(state.mmio());
        let mut clear = Is::default();
        let mut dump_registers = false;
        let mut uec_regs = None;
        let mut failed_hibernate: [Option<&str>; 2] = [None; 2];
        let mut hibernate_upmcrs = 0;

        if status.utp_transfer_request_completion_status() {
            clear.set_utp_transfer_request_completion_status(true);
        }
        // Summary of `CQIS`, which the MCQ handler clears.
        if status.cq_event_status() {
            clear.set_cq_event_status(true);
        }
        if status.uic_error() {
            // ORDERING: Standalone monotonic telemetry counter read by Inspect and tests.
            self.uic_error_count.fetch_add(1, Ordering::Relaxed);
            let regs = state.read_uic_error_regs();
            if ControllerState::<M>::is_fatal_uic_error(regs) {
                dump_registers = true;
            }
            uec_regs = Some(regs);
            clear.set_uic_error(true);
        }
        if !uic_busy && (status.uic_hibernate_enter_status() || status.uic_hibernate_exit_status())
        {
            // Per UFSHCI 4.0 sections 5.3.1 and 8.1.7, with autonomous auto-hibernate enabled
            // (`ah8_configured`) the controller raises `IS.UHES`/`IS.UHXS` only when an autonomous
            // enter/exit fails; otherwise the transition failed if `HCS.UPMCRS` is not a valid
            // hibernate status.
            let hcs = HostControllerStatusReg::read(state.mmio());
            hibernate_upmcrs = hcs.uic_power_mode_change_request_status();
            let pm_ok = hcs.power_mode_status().is_some_and(PowerModeStatus::is_success);
            let hibernate_events: [(fn(&Is) -> bool, fn(&mut Is, bool), &str); 2] = [
                (Is::uic_hibernate_enter_status, Is::set_uic_hibernate_enter_status, "enter"),
                (Is::uic_hibernate_exit_status, Is::set_uic_hibernate_exit_status, "exit"),
            ];
            for (idx, (is_set, set_clear, transition)) in hibernate_events.into_iter().enumerate() {
                if is_set(&status) {
                    set_clear(&mut clear, true);
                    if ah8_configured || !pm_ok {
                        // ORDERING: Standalone monotonic telemetry counter read by Inspect and
                        // tests.
                        self.hibernate_error_count.fetch_add(1, Ordering::Relaxed);
                        failed_hibernate[idx] = Some(transition);
                        dump_registers = true;
                    }
                }
            }
        }
        if status.utp_error_status() {
            clear.set_utp_error_status(true);
        }
        let fatal_errors: [(fn(&Is) -> bool, fn(&mut Is, bool), &str); 5] = [
            (Is::uic_link_lost_status, Is::set_uic_link_lost_status, "UIC link lost"),
            (
                Is::device_fatal_error_status,
                Is::set_device_fatal_error_status,
                "device fatal error",
            ),
            (
                Is::host_controller_fatal_error_status,
                Is::set_host_controller_fatal_error_status,
                "host controller fatal error",
            ),
            (
                Is::system_bus_fatal_error_status,
                Is::set_system_bus_fatal_error_status,
                "system bus fatal error",
            ),
            (
                Is::crypto_engine_fatal_error_status,
                Is::set_crypto_engine_fatal_error_status,
                "crypto engine fatal error",
            ),
        ];
        let mut fired_fatals: [Option<&str>; 5] = [None; 5];
        for (idx, (is_set, set_clear, interrupt)) in fatal_errors.into_iter().enumerate() {
            if is_set(&status) {
                set_clear(&mut clear, true);
                // ORDERING: Standalone monotonic telemetry counter read by Inspect and tests.
                self.fatal_error_count.fetch_add(1, Ordering::Relaxed);
                fired_fatals[idx] = Some(interrupt);
                dump_registers = true;
            }
        }

        let has_error = uec_regs.is_some()
            || status.utp_error_status()
            || failed_hibernate.iter().any(Option::is_some)
            || fired_fatals.iter().any(Option::is_some);
        if dump_registers && uec_regs.is_none() {
            uec_regs = Some(state.read_uic_error_regs());
        }
        if has_error && let Some(suppressed_isr_errors) = state.should_log_isr_error() {
            if let Some((uecpa, uecdl, uecn, uect, uecdme)) = uec_regs
                && status.uic_error()
            {
                warn!(
                    uecpa,
                    uecdl,
                    uecn,
                    uect,
                    uecdme,
                    suppressed_isr_errors;
                    "UFS UIC error"
                );
            }
            for transition in failed_hibernate.into_iter().flatten() {
                error!(
                    status = status.0,
                    upmcrs = hibernate_upmcrs,
                    transition,
                    suppressed_isr_errors;
                    "UFS: Auto Hibern8 failed"
                );
            }
            if status.utp_error_status() {
                let hcs = HostControllerStatusReg::read(state.mmio());
                error!(
                    code:? = hcs.error_code(),
                    task_tag = hcs.task_tag_of_utp_error(),
                    lun = hcs.target_lun_of_utp_error(),
                    suppressed_isr_errors;
                    "UFS UTP error"
                );
            }
            for interrupt in fired_fatals.into_iter().flatten() {
                error!(interrupt, suppressed_isr_errors; "UFS fatal interrupt detected");
            }
            if dump_registers {
                state.dump_debug_registers(
                    status,
                    uec_regs.expect("dump_registers populates uec_regs"),
                    suppressed_isr_errors,
                );
            }
        }
        if clear.0 != 0 {
            clear.write(state.mmio_mut());
        }
        status
    }

    /// Configures auto-hibernate with an idle timer of about `timer_us` microseconds if
    /// supported. `timer_us == 0` disables auto-hibernate and clears `IE.UHESE`/`IE.UHXSE` before
    /// zeroing `AHIT` so software hibernate polling cannot race with the interrupt handler.
    pub fn configure_auto_hibernate(&self, timer_us: u32) {
        let mut state = self.state.lock();
        if !CapabilityReg::read(state.mmio()).auto_hibernation_support() {
            return;
        }
        if timer_us == 0 {
            state.ah8_timer_raw = 0;
            state.disable_ah8();
            debug!("UFS Auto-Hibernate (AH8) disabled");
            return;
        }
        // Scale n means units of 10^n us.
        let mut value = timer_us;
        let mut scale = 0;
        while value > AUTO_HIBERNATE_MAX_TIMER_VALUE && scale < AUTO_HIBERNATE_MAX_TIMER_SCALE {
            value = value.div_ceil(10);
            scale += 1;
        }
        let mut ahit = AutoHibernateIdleTimerReg::from_raw(0);
        ahit.set_timer_scale(scale);
        ahit.set_timer_value(value.min(AUTO_HIBERNATE_MAX_TIMER_VALUE));
        state.ah8_timer_raw = ahit.0;
        state.restore_ah8();
        if !state.ah8_logged {
            state.ah8_logged = true;
            info!(timer_us, scale, value; "UFS Auto-Hibernate (AH8) timer set");
        } else {
            debug!(timer_us, scale, value; "UFS Auto-Hibernate (AH8) timer set");
        }
    }

    /// Disables hardware auto-hibernate and its enter/exit error interrupts (`AHIT = 0`).
    pub fn disable_auto_hibernate(&self) {
        self.configure_auto_hibernate(AUTO_HIBERNATE_DISABLED);
    }

    /// Returns whether autonomous auto-hibernate is currently active in hardware (`AHIT != 0` and
    /// not held off by an active UIC command or [`AutoHibernateHoldGuard`]).
    #[cfg(test)]
    pub fn is_auto_hibernate_active(&self) -> bool {
        self.state.lock().is_ah8_enabled()
    }

    /// Polls `f` under [`Self::state`] until it returns `Some(res)` or `timeout` elapses, failing
    /// immediately with `IoError` if controller registers become unreachable.
    async fn poll_registers<T>(
        &self,
        timeout: zx::MonotonicDuration,
        mut f: impl FnMut(&mut ControllerState<M>) -> Option<Result<T, UfsError>>,
    ) -> Result<T, UfsError> {
        uic::poll_until(timeout, || {
            let mut state = self.state.lock();
            if !state.access.is_reachable() {
                return Some(Err(UfsError::IoError));
            }
            f(&mut state)
        })
        .await?
    }

    /// Keeps hardware auto-hibernate disabled for the lifetime of the returned guard.
    ///
    /// Used around multi-command DME sequences such as gear changes so auto-hibernate is not
    /// restored between back-to-back commands:
    /// 1. Clear `AHIT` and its error interrupts under the hold count.
    /// 2. Wait for the controller to accept UIC commands (`HCS.UCRDY`).
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCS.UCRDY` does not assert within `timeout`.
    pub async fn hold_auto_hibernate(
        &self,
        timeout: zx::MonotonicDuration,
    ) -> Result<AutoHibernateHoldGuard<'_, M>, UfsError> {
        let mut busy_guard: Option<UicBusyGuard<'_, M>> = None;
        let mut hold_guard: Option<AutoHibernateHoldGuard<'_, M>> = None;
        self.poll_registers(timeout, |state| {
            if busy_guard.is_none() {
                if state.uic_busy {
                    return None;
                }
                let was_enabled = state.is_ah8_enabled();
                state.uic_busy = true;
                if was_enabled {
                    state.disable_ah8();
                }
                busy_guard = Some(UicBusyGuard { controller: self });
            }
            if !uic::uic_command_ready(state.mmio()) {
                return None;
            }
            state.ah8_hold_count =
                state.ah8_hold_count.checked_add(1).expect("ah8_hold_count overflow");
            hold_guard = Some(AutoHibernateHoldGuard { controller: self });
            uic::clear_interrupt_status(state.mmio_mut(), |is| {
                is.set_uic_hibernate_enter_status(true);
                is.set_uic_hibernate_exit_status(true);
            });
            Some(Ok(()))
        })
        .await?;
        drop(busy_guard);
        Ok(hold_guard.expect("AutoHibernateHoldGuard must be acquired before poll_until completes"))
    }

    /// Returns whether the controller is powered and the device answered link startup (`HCS.DP`).
    #[cfg(test)]
    pub fn device_present(&self) -> bool {
        let state = self.state.lock();
        state.access.is_reachable() && HostControllerStatusReg::read(&state.mmio).device_present()
    }

    /// Returns whether a UIC command currently owns the UIC registers.
    #[cfg(test)]
    pub fn uic_busy(&self) -> bool {
        self.state.lock().uic_busy
    }

    /// Overrides [`HOST_CONTROLLER_TIMEOUT`] for `HCE` polling in unit tests.
    #[cfg(test)]
    pub fn set_hce_timeout(&self, timeout: zx::MonotonicDuration) {
        self.host_controller_timeout_ns.store(timeout.into_nanos() as u64, Ordering::SeqCst);
    }

    fn host_controller_timeout(&self) -> zx::MonotonicDuration {
        #[cfg(test)]
        {
            let ns = self.host_controller_timeout_ns.load(Ordering::SeqCst);
            if ns != 0 {
                return zx::MonotonicDuration::from_nanos(ns as i64);
            }
        }
        HOST_CONTROLLER_TIMEOUT
    }

    /// Waits for `HCE` to read back as `enable`.
    async fn wait_for_hce(&self, enable: bool) -> Result<(), UfsError> {
        self.poll_registers(self.host_controller_timeout(), |state| {
            (HostControllerEnableReg::read(state.mmio()).host_controller_enable() == enable)
                .then_some(Ok(()))
        })
        .await
    }

    /// Resets and enables the host controller (`HCE`).
    ///
    /// Clears any powered-off or auto-hibernate state, masks interrupts, pulses `HCE` low first if
    /// the controller is already enabled, and waits for `HCE` to assert. Not on [`PowerGuard`]
    /// because bring-up calls it before `PowerControl` exists.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCE` does not settle within [`HOST_CONTROLLER_TIMEOUT`].
    pub async fn reset_and_enable(&self) -> Result<(), UfsError> {
        let was_enabled = {
            let mut state = self.state.lock();
            state.access = RegisterAccess::Reachable;
            state.ah8_timer_raw = 0;
            state.ah8_logged = false;
            state.disable_interrupts();
            let enabled = HostControllerEnableReg::read(state.mmio()).host_controller_enable();
            if enabled {
                state.write_hce(false);
            }
            enabled
        };
        if was_enabled {
            self.wait_for_hce(false).await?;
        }
        self.state.lock().write_hce(true);
        self.wait_for_hce(true).await
    }

    /// Masks interrupts, disables the host controller (`HCE = 0`), and waits for it to stop.
    ///
    /// Once this returns `Ok`, the controller performs no more DMA. Not on [`PowerGuard`]: callers
    /// run under `PowerControl`, except the two bring-up failure paths that run before
    /// `PowerControl` exists.
    ///
    /// # Errors
    ///
    /// Returns `Timeout` if `HCE` does not clear within [`HOST_CONTROLLER_TIMEOUT`].
    pub async fn disable_host_controller(&self) -> Result<(), UfsError> {
        {
            let mut state = self.state.lock();
            debug_assert_ne!(
                state.access,
                RegisterAccess::Blocked,
                "disable_host_controller called while register access is blocked"
            );
            state.ah8_timer_raw = 0;
            state.ah8_logged = false;
            state.access = RegisterAccess::OffUnconfirmed;
            InterruptEnableReg::from_raw(0).write(&mut state.mmio);
            HostControllerEnableReg::from_raw(0).write(&mut state.mmio);
        }
        // `poll_registers` refuses while access is `OffUnconfirmed`, and `mmio()` asserts
        // `Reachable`, so poll `HCE` here through the field.
        uic::poll_until(self.host_controller_timeout(), || {
            let state = self.state.lock();
            (!HostControllerEnableReg::read(&state.mmio).host_controller_enable()).then_some(())
        })
        .await?;
        let mut state = self.state.lock();
        if state.access == RegisterAccess::OffUnconfirmed {
            state.access = RegisterAccess::Off;
        }
        Ok(())
    }

    /// Waits until `HCS.UCRDY` is 1 so any in-flight autonomous auto-hibernate transition has
    /// settled.
    ///
    /// # Errors
    ///
    /// Returns [`UfsError::Timeout`] if `HCS.UCRDY` does not assert within `timeout`.
    pub async fn wait_uic_command_ready(
        &self,
        timeout: zx::MonotonicDuration,
    ) -> Result<(), UfsError> {
        self.poll_registers(timeout, |state| uic::uic_command_ready(state.mmio()).then_some(Ok(())))
            .await
    }

    /// Acquires exclusive ownership of the UIC registers and issues `command`.
    ///
    /// Auto-hibernate cannot have parked the link when a non-hibernate command is issued, because
    /// bring-up and resume arm `AHIT` only after their last DME command and `reset_and_enable`
    /// clears it.
    ///
    /// 1. Claim `uic_busy` and hold hardware auto-hibernate off.
    /// 2. Wait for `HCS.UCRDY`.
    /// 3. Run `before_issue` and write `command` to the UIC registers.
    async fn issue_uic_with_guard(
        &self,
        command: &UicCommand,
        timeout: zx::MonotonicDuration,
        before_issue: impl FnOnce(&mut M),
    ) -> Result<UicBusyGuard<'_, M>, UfsError> {
        let mut guard: Option<UicBusyGuard<'_, M>> = None;
        let mut before_issue = Some(before_issue);
        self.poll_registers(timeout, |state| {
            if guard.is_none() {
                if state.uic_busy {
                    return None;
                }
                let was_ah8_enabled = state.is_ah8_enabled();
                state.uic_busy = true;
                if was_ah8_enabled {
                    state.disable_ah8();
                }
                guard = Some(UicBusyGuard { controller: self });
            }
            if !uic::uic_command_ready(state.mmio()) {
                return None;
            }
            if let Some(hook) = before_issue.take() {
                hook(state.mmio_mut());
            }
            uic::issue_uic_command(state.mmio_mut(), command);
            Some(Ok(()))
        })
        .await?;
        Ok(guard.expect("UicBusyGuard must be acquired before poll_until completes"))
    }

    /// Polls `IS.UCCS` for an already-issued UIC command while the caller holds [`UicBusyGuard`].
    async fn poll_uic_completion(
        &self,
        opcode: UicCommandOpcode,
        timeout: zx::MonotonicDuration,
    ) -> Result<u32, UfsError> {
        let result = self
            .poll_registers(timeout, |state| {
                uic::poll_uic_command_completion(state.mmio_mut(), opcode)
            })
            .await;
        if matches!(result, Err(UfsError::Timeout)) {
            let mut state = self.state.lock();
            if state.access.is_reachable() {
                uic::clear_interrupt_status(state.mmio_mut(), |is| {
                    is.set_uic_command_completion_status(true)
                });
            }
        }
        result
    }

    /// Sends a UIC command that completes through `IS.UCCS`. Concurrent commands are serialized.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` for hibernate commands, `Timeout`, or `UicError`.
    pub async fn send_uic_command(&self, command: &UicCommand) -> Result<u32, UfsError> {
        if !command.opcode.completes_with_uccs() {
            return Err(UfsError::InvalidParameter);
        }
        let _busy = self.issue_uic_with_guard(command, HOST_CONTROLLER_TIMEOUT, |_| {}).await?;
        self.poll_uic_completion(command.opcode, HOST_CONTROLLER_TIMEOUT).await
    }

    /// Runs `DME_LINKSTARTUP` up to [`LINK_STARTUP_ATTEMPTS`] times until it succeeds and
    /// `HCS.DP` reads 1, then clears `UECPA`, `UECDL`, and `IS.UE` left over from link training.
    ///
    /// # Errors
    ///
    /// Returns the last UIC error, or `IoError` if the device never became present.
    pub async fn link_startup(&self) -> Result<(), UfsError> {
        let mut last_error = UfsError::IoError;
        let command = UicCommand::new(UicCommandOpcode::DmeLinkStartup);
        for attempt in 1..=LINK_STARTUP_ATTEMPTS {
            match self.send_uic_command(&command).await {
                Ok(_) => {
                    let mut state = self.state.lock();
                    if HostControllerStatusReg::read(state.mmio()).device_present() {
                        // The error code registers clear on read; otherwise the first `IS.UE`
                        // after bring-up would report stale training-time `PA_INIT_ERROR` bits as
                        // a fatal UIC error.
                        let _ = HostUicErrorCodePhyAdapterLayerReg::read(state.mmio());
                        let _ = HostUicErrorCodeDataLinkLayerReg::read(state.mmio());
                        uic::clear_interrupt_status(state.mmio_mut(), |is| is.set_uic_error(true));
                        return Ok(());
                    }
                    drop(state);
                    warn!(attempt; "UFS device not present after link startup");
                }
                Err(e) => {
                    warn!(attempt, err:? = e; "UFS link startup attempt failed");
                    last_error = e;
                }
            }
            if attempt < LINK_STARTUP_ATTEMPTS {
                fasync::Timer::new(zx::MonotonicInstant::after(LINK_STARTUP_RETRY_DELAY)).await;
            }
        }
        Err(last_error)
    }

    /// Sends `DME_GET` (local). See [`Self::send_uic_command`].
    pub async fn dme_get(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
    ) -> Result<u32, UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmeGet, mib_attribute, gen_selector_index, 0);
        self.send_uic_command(&command).await
    }

    /// Sends volatile `DME_SET` (local). See [`Self::send_uic_command`].
    pub async fn dme_set(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
        value: u32,
    ) -> Result<(), UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmeSet, mib_attribute, gen_selector_index, value);
        self.send_uic_command(&command).await.map(|_| ())
    }

    /// Sends `DME_PEER_GET` (device). See [`Self::send_uic_command`].
    pub async fn dme_peer_get(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
    ) -> Result<u32, UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmePeerGet, mib_attribute, gen_selector_index, 0);
        self.send_uic_command(&command).await
    }

    /// Sends volatile `DME_PEER_SET` (device). See [`Self::send_uic_command`].
    pub async fn dme_peer_set(
        &self,
        mib_attribute: u16,
        gen_selector_index: u16,
        value: u32,
    ) -> Result<(), UfsError> {
        let command =
            UicCommand::dme(UicCommandOpcode::DmePeerSet, mib_attribute, gen_selector_index, value);
        self.send_uic_command(&command).await.map(|_| ())
    }

    /// Sets `PA_PWRMode` and waits for `IS.UPMS`, then requires `HCS.UPMCRS == PWR_LOCAL`
    /// (UFSHCI 3.0/4.0 section 7.4).
    ///
    /// Holds the UIC registers across the entire power-mode transition so concurrent UIC callers
    /// cannot interleave between `DME_SET(PA_PWRMode)` and `IS.UPMS`.
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `UicError` with the `DME_SET` result code or the `UPMCRS` value.
    pub async fn change_power_mode(
        &self,
        power_mode: u32,
        timeout: zx::MonotonicDuration,
    ) -> Result<(), UfsError> {
        let command = UicCommand::dme(UicCommandOpcode::DmeSet, PA_PWRMODE, 0, power_mode);
        let _busy = self
            .issue_uic_with_guard(&command, timeout, |mmio| {
                uic::clear_interrupt_status(mmio, |is| is.set_uic_power_mode_status(true));
            })
            .await?;
        self.poll_uic_completion(command.opcode, timeout).await?;
        self.poll_registers(timeout, |state| {
            if !InterruptStatusReg::read(state.mmio()).uic_power_mode_status() {
                return None;
            }
            let result = uic::check_power_mode_status(state.mmio(), UicCommandOpcode::DmeSet);
            uic::clear_interrupt_status(state.mmio_mut(), |is| is.set_uic_power_mode_status(true));
            Some(result)
        })
        .await
    }

    /// Puts the link into hibernate (`DME_HIBERNATE_ENTER`).
    ///
    /// Waits for command acceptance (`IS.UCCS`) followed by hibernate entry (`IS.UHES`, UFSHCI
    /// 4.0 section 5.3.1), then verifies power-mode status (`HCS.UPMCRS`, section 5.3.4).
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `UicError`.
    pub async fn hibernate_enter(&self, timeout: zx::MonotonicDuration) -> Result<(), UfsError> {
        self.run_hibernate(UicCommandOpcode::DmeHibernateEnter, timeout).await
    }

    /// Wakes the link from hibernate (`DME_HIBERNATE_EXIT`).
    ///
    /// Waits for command acceptance (`IS.UCCS`) followed by hibernate exit (`IS.UHXS`, UFSHCI
    /// 4.0 section 5.3.1), then verifies power-mode status (`HCS.UPMCRS`, section 5.3.4).
    ///
    /// # Errors
    ///
    /// Returns `Timeout`, or `UicError`.
    pub async fn hibernate_exit(&self, timeout: zx::MonotonicDuration) -> Result<(), UfsError> {
        self.run_hibernate(UicCommandOpcode::DmeHibernateExit, timeout).await
    }

    /// Runs `DME_HIBERNATE_ENTER` or `DME_HIBERNATE_EXIT` while holding the UIC registers across
    /// both completion stages, counting failures in [`Self::hibernate_error_count`].
    async fn run_hibernate(
        &self,
        opcode: UicCommandOpcode,
        timeout: zx::MonotonicDuration,
    ) -> Result<(), UfsError> {
        let is_enter = opcode == UicCommandOpcode::DmeHibernateEnter;
        let command = UicCommand::new(opcode);
        let clear_hiber = |is: &mut InterruptStatusReg| {
            if is_enter {
                is.set_uic_hibernate_enter_status(true);
            } else {
                is.set_uic_hibernate_exit_status(true);
            }
        };
        let _busy = self
            .issue_uic_with_guard(&command, timeout, |mmio| {
                uic::clear_interrupt_status(mmio, clear_hiber)
            })
            .await?;
        let result = async {
            self.poll_uic_completion(opcode, timeout).await?;
            self.poll_registers(timeout, |state| {
                let is = InterruptStatusReg::read(state.mmio());
                let done = if is_enter {
                    is.uic_hibernate_enter_status()
                } else {
                    is.uic_hibernate_exit_status()
                };
                if !done {
                    return None;
                }
                let result = uic::check_power_mode_status(state.mmio(), opcode);
                uic::clear_interrupt_status(state.mmio_mut(), clear_hiber);
                Some(result)
            })
            .await
        }
        .await;
        if let Err(ref err) = result {
            self.hibernate_error_count.fetch_add(1, Ordering::Relaxed);
            warn!(
                direction = if is_enter { "enter" } else { "exit" },
                err:%;
                "Link hibernate transition failed"
            );
        }
        result
    }
}

/// Scope guard that inhibits auto-hibernate (`AHIT`). Used to prevent hibernate during
/// multi-command UIC sequences such as gear changes.
#[must_use = "dropping the guard immediately re-enables Auto-Hibernate"]
pub struct AutoHibernateHoldGuard<'a, M: Mmio> {
    controller: &'a UfsController<M>,
}

impl<M: Mmio> Drop for AutoHibernateHoldGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.controller.state.lock();
        state.ah8_hold_count = state.ah8_hold_count.saturating_sub(1);
        state.restore_ah8();
    }
}

/// Releases the UIC registers when a UIC command finishes or is cancelled.
#[must_use = "dropping the guard immediately releases the UIC registers"]
struct UicBusyGuard<'a, M: Mmio> {
    controller: &'a UfsController<M>,
}

impl<M: Mmio> Drop for UicBusyGuard<'_, M> {
    fn drop(&mut self) {
        let mut state = self.controller.state.lock();
        state.uic_busy = false;
        state.restore_ah8();
    }
}

impl<M: Mmio, H: PowerHooks> PowerGuard<'_, M, H> {
    /// Masks interrupts if reachable, waits until all in-flight interrupt passes exit, and
    /// returns the observed register access state.
    pub fn pause_interrupts_for_power_off(&mut self) -> RegisterAccess {
        let controller = self.controller();
        let access = {
            let mut state = controller.state.lock();
            if state.access.is_reachable() {
                state.disable_interrupts();
            }
            state.access
        };
        controller.interrupt_gate.pause();
        access
    }

    /// Returns the controller's current register access state while the power state is held.
    pub fn register_access(&self) -> RegisterAccess {
        self.controller().state.lock().access
    }

    /// Runs `f` on the register block and marks the host controller powered off under one lock.
    ///
    /// Transitions to [`RegisterAccess::Off`] if `HCE == 0`, or [`RegisterAccess::OffUnconfirmed`]
    /// until [`UfsController::disable_host_controller`] confirms it has stopped.
    pub fn power_off_after(&mut self, f: impl FnOnce(&mut M)) {
        let mut state = self.controller().state.lock();
        debug_assert!(
            state.access.is_reachable(),
            "power_off_after called while register access is {:?}",
            state.access
        );
        f(&mut state.mmio);
        state.access = if HostControllerEnableReg::read(&state.mmio).host_controller_enable() {
            RegisterAccess::OffUnconfirmed
        } else {
            RegisterAccess::Off
        };
    }

    /// Marks the host controller unreachable and unquiesced without touching MMIO.
    ///
    /// Used during teardown when waking the PHY fails: `quiesce` will quarantine live DMA buffers
    /// instead of reading or writing unclocked controller registers.
    pub fn mark_unreachable_unquiesced(&mut self) {
        self.controller().state.lock().access = RegisterAccess::OffUnconfirmed;
    }

    /// Masks controller interrupts, pauses the interrupt thread, and blocks register access until
    /// [`Self::allow_register_access`], for while the PHY is in `HibernateSleep`.
    pub fn block_register_access(&mut self) {
        let controller = self.controller();
        {
            let mut state = controller.state.lock();
            if state.access.is_reachable() {
                state.disable_interrupts();
            }
        }
        controller.interrupt_gate.pause();
        let mut state = controller.state.lock();
        // Callers hold `PowerGuard`, and the PHY sends DME requests only while its `Init` or
        // `PrePowerModeChange` call is in progress, so no UIC command is in flight.
        debug_assert!(!state.uic_busy, "register access blocked during a UIC command");
        if state.access.is_reachable() {
            state.access = RegisterAccess::Blocked;
        }
    }

    /// Allows register access again once the PHY is `Active`, restores `IE` if
    /// [`Self::block_register_access`] masked it, and resumes the interrupt thread.
    pub fn allow_register_access(&mut self) {
        let controller = self.controller();
        let reachable = {
            let mut state = controller.state.lock();
            if state.access == RegisterAccess::Blocked {
                state.access = RegisterAccess::Reachable;
                state.enable_interrupts();
            }
            state.access.is_reachable()
        };
        if reachable {
            controller.interrupt_gate.resume();
        }
    }

    /// Clears the `HibernateSleep` register-access block without unmasking interrupts.
    ///
    /// Used before stopping or resetting the controller (`reset_hardware`, `prepare_phy_for_stop`),
    /// where `IE` must stay masked so status bits latched in sleep do not fire on the interrupt
    /// thread before `HCE` is cleared.
    pub fn allow_register_access_for_reset(&mut self) {
        let mut state = self.controller().state.lock();
        if state.access == RegisterAccess::Blocked {
            state.access = RegisterAccess::Reachable;
        }
    }

    /// Enables standard controller interrupts and resumes the interrupt thread.
    pub fn enable_interrupts(&mut self) {
        let controller = self.controller();
        controller.state.lock().enable_interrupts();
        controller.interrupt_gate.resume();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{PA_MAXRXHSGEAR, PA_TXGEAR};
    use crate::registers::{PowerModeStatus, UicCommandArgument1Reg, UicCommandReg};
    use crate::testing::fake_ufs::{
        CAP_AUTO_HIBERNATE_SUPPORT, FakeMmio, HCS_ACTIVE_LINK, HCS_DEVICE_PRESENT,
        complete_uic_command, hcs_power_mode_status, uic_hardware, uic_opcode, wait_until,
    };
    use diagnostics_assertions::assert_data_tree;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    async fn test_power_for(
        controller: Arc<UfsController<FakeMmio>>,
    ) -> crate::power::PowerControl<FakeMmio, ()> {
        let bti = fake_bti::FakeBti::create().unwrap();
        let processor = Arc::new(
            crate::transfer_request_processor::TransferRequestProcessor::new(&bti, controller)
                .unwrap(),
        );
        crate::testing::test_power(&processor)
    }

    #[fuchsia::test]
    async fn test_reset_enable_and_link_startup() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeLinkStartup as u32));
            // The device answers link startup: HCS.DP.
            let hcs = regs.load32(HostControllerStatusReg::OFFSET);
            regs.store32(HostControllerStatusReg::OFFSET, hcs | HCS_DEVICE_PRESENT);
            complete_uic_command(&mut regs, 0, 0, |_| {});
        });

        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        controller.reset_and_enable().await.unwrap();
        assert!(
            controller
                .with_mmio(|mmio| HostControllerEnableReg::read(mmio).host_controller_enable())
        );

        controller.link_startup().await.unwrap();
        assert!(controller.device_present());
        hardware.join().unwrap();
    }

    #[fuchsia::test]
    async fn test_send_uic_command() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            let arg1 = UicCommandArgument1Reg::read(&regs);
            assert_eq!(arg1.mib_attribute() as u16, PA_MAXRXHSGEAR);
            complete_uic_command(&mut regs, 0, 0xABCD, |_| {});
            regs
        });

        let value = controller.dme_get(PA_MAXRXHSGEAR, 0).await;
        assert_eq!(value, Ok(0xABCD));
        let regs = hardware.join().unwrap();
        assert!(!InterruptStatusReg::read(&regs).uic_command_completion_status());
        assert!(!controller.uic_busy());
    }

    #[fuchsia::test]
    async fn test_send_uic_command_error_clears_status() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        let hardware = thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeSet as u32));
            complete_uic_command(&mut regs, 1, 0, |_| {});
            regs
        });

        let result = controller.dme_set(PA_TXGEAR, 0, 4).await;
        assert_eq!(
            result,
            Err(UfsError::UicError { opcode: UicCommandOpcode::DmeSet as u8, code: 1 })
        );
        let regs = hardware.join().unwrap();
        assert!(!InterruptStatusReg::read(&regs).uic_command_completion_status());
        assert!(!controller.uic_busy());
    }

    #[fuchsia::test]
    async fn test_send_uic_command_rejects_hibernate() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let _regs = uic_hardware(&vmo, 0);
        let result = controller
            .send_uic_command(&UicCommand::new(UicCommandOpcode::DmeHibernateEnter))
            .await;
        assert_eq!(result, Err(UfsError::InvalidParameter));
        assert_eq!(controller.with_mmio(|mmio| UicCommandReg::read(mmio).command_opcode()), 0);
    }

    #[fuchsia::test]
    async fn test_auto_hibernate_toggles_uhese_uhxse_and_holds_during_dme() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, 1024)));
        let power = test_power_for(Arc::clone(&controller)).await;
        let mut p = power.lock().await;
        let mut regs = uic_hardware(&vmo, HCS_ACTIVE_LINK);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        assert!(controller.capabilities().auto_hibernation_support());

        p.enable_interrupts();
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());
        assert!(ie.uic_link_lost_status_enable());

        controller.configure_auto_hibernate(2000);
        let expected_ahit = controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0);
        assert_ne!(expected_ahit, 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());

        // While a DME command is in flight, AHIT is cleared to 0 and UHESE/UHXSE are disabled so
        // autonomous auto-hibernate cannot race with the UIC command.
        let hardware = thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            let ie_during = InterruptEnableReg::read(&regs);
            assert!(!ie_during.uic_hibernate_enter_status_enable());
            assert!(!ie_during.uic_hibernate_exit_status_enable());
            complete_uic_command(&mut regs, 0, 0x1234, |_| {});
        });

        assert_eq!(controller.dme_get(PA_MAXRXHSGEAR, 0).await, Ok(0x1234));
        hardware.join().unwrap();

        // After the DME command finishes, AHIT and UHESE/UHXSE are restored.
        assert_eq!(
            controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            expected_ahit
        );
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());

        // Disabling auto-hibernate clears UHESE/UHXSE and AHIT.
        controller.configure_auto_hibernate(0);
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());

        controller.disable_interrupts();
        assert_eq!(controller.with_mmio(|mmio| InterruptEnableReg::read(mmio).0), 0);
    }

    #[fuchsia::test]
    async fn test_hold_auto_hibernate_across_multiple_dme_commands() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, 1024)));
        let power = test_power_for(Arc::clone(&controller)).await;
        let mut p = power.lock().await;
        let mut regs = uic_hardware(&vmo, HCS_ACTIVE_LINK);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        assert!(controller.capabilities().auto_hibernation_support());

        p.enable_interrupts();
        controller.configure_auto_hibernate(2000);
        let expected_ahit = controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0);
        assert_ne!(expected_ahit, 0);
        assert!(controller.is_auto_hibernate_active());

        let outer_guard = controller.hold_auto_hibernate(HOST_CONTROLLER_TIMEOUT).await.unwrap();
        assert!(!controller.is_auto_hibernate_active());
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(!ie.uic_hibernate_enter_status_enable());
        assert!(!ie.uic_hibernate_exit_status_enable());

        let inner_guard = controller.hold_auto_hibernate(HOST_CONTROLLER_TIMEOUT).await.unwrap();

        let hardware = thread::spawn(move || {
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeGet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            complete_uic_command(&mut regs, 0, 0x1111, |_| {});

            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeSet as u32));
            assert_eq!(AutoHibernateIdleTimerReg::read(&regs).0, 0);
            complete_uic_command(&mut regs, 0, 0, |_| {});
        });

        assert_eq!(controller.dme_get(PA_MAXRXHSGEAR, 0).await, Ok(0x1111));
        // Between DME commands while a hold guard is active, AHIT remains 0.
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        assert_eq!(controller.dme_set(PA_TXGEAR, 0, 4).await, Ok(()));
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);
        hardware.join().unwrap();

        drop(inner_guard);
        assert!(!controller.is_auto_hibernate_active());
        assert_eq!(controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0), 0);

        drop(outer_guard);
        assert!(controller.is_auto_hibernate_active());
        assert_eq!(
            controller.with_mmio(|mmio| AutoHibernateIdleTimerReg::read(mmio).0),
            expected_ahit
        );
        let ie = controller.with_mmio(|mmio| InterruptEnableReg::read(mmio));
        assert!(ie.uic_hibernate_enter_status_enable());
        assert!(ie.uic_hibernate_exit_status_enable());
    }

    /// Returns the raw `IS` value with each `set` bit asserted.
    fn is_bits(set: &[fn(&mut InterruptStatusReg, bool)]) -> u32 {
        let mut is = InterruptStatusReg::from_raw(0);
        for setter in set {
            setter(&mut is, true);
        }
        is.0
    }

    #[fuchsia::test]
    async fn test_fetch_and_clear_interrupts_counts_uic_hibernate_and_fatal_errors() {
        use InterruptStatusReg as Is;
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, 1024)));
        let power = test_power_for(Arc::clone(&controller)).await;
        let mut p = power.lock().await;
        let mut regs = uic_hardware(&vmo, 0);
        regs.store32(CapabilityReg::OFFSET, CAP_AUTO_HIBERNATE_SUPPORT);
        p.enable_interrupts();
        controller.configure_auto_hibernate(2000);
        let inspector = Inspector::default();
        controller.record_inspect(inspector.root());
        // Every hibernate status interrupt below arrives with UPMCRS = PWR_FATAL_ERROR.
        regs.store32(
            HostControllerStatusReg::OFFSET,
            hcs_power_mode_status(PowerModeStatus::PowerFatalError),
        );

        struct Case {
            name: &'static str,
            uecpa: u32,
            uecdl: u32,
            is: u32,
            uic_busy: bool,
            /// Cumulative (`uic_error_count`, `hibernate_error_count`, `fatal_error_count`)
            /// after the interrupt is serviced.
            counts: (u64, u64, u64),
            /// `IS` bits left set for another consumer.
            leftover: u32,
        }
        let cases = [
            // Simulate an auto-hibernate exit failure interrupt (IS.UHXS + UPMCRS =
            // PWR_FATAL_ERROR).
            Case {
                name: "AH8 exit failure",
                uecpa: 0,
                uecdl: 0,
                is: is_bits(&[Is::set_uic_hibernate_exit_status]),
                uic_busy: false,
                counts: (0, 1, 0),
                leftover: 0,
            },
            // Simulate UIC error with PA line-reset code (0x10): non-fatal, counted only.
            Case {
                name: "PA line reset",
                uecpa: (1 << 31) | UIC_PHY_ADAPTER_LAYER_LINE_RESET,
                uecdl: 0,
                is: is_bits(&[Is::set_uic_error]),
                uic_busy: false,
                counts: (1, 1, 0),
                leftover: 0,
            },
            // Simulate a DL error without PA_INIT_ERROR: non-fatal, counted only.
            Case {
                name: "DL non-fatal",
                uecpa: 0,
                uecdl: (1 << 31) | 0x01,
                is: is_bits(&[Is::set_uic_error]),
                uic_busy: false,
                counts: (2, 1, 0),
                leftover: 0,
            },
            // While `uic_busy` is true, IS.UHES is left for the synchronous UIC poller, while
            // IS.UE (`PA_INIT_ERROR`) is still cleared and counted.
            Case {
                name: "PA_INIT_ERROR during UIC command",
                uecpa: 0,
                uecdl: (1 << 31) | UIC_DATA_LINK_LAYER_PA_INIT_ERROR,
                is: is_bits(&[Is::set_uic_error, Is::set_uic_hibernate_enter_status]),
                uic_busy: true,
                counts: (3, 1, 0),
                leftover: is_bits(&[Is::set_uic_hibernate_enter_status]),
            },
            // Simulate UIC error with non-line-reset PA error code (0x01): fatal, counted once.
            Case {
                name: "PA fatal",
                uecpa: (1 << 31) | 0x01,
                uecdl: 0,
                is: is_bits(&[Is::set_uic_error]),
                uic_busy: false,
                counts: (4, 1, 0),
                leftover: 0,
            },
            // Simulate UIC link lost (IS.ULLS).
            Case {
                name: "link lost",
                uecpa: 0,
                uecdl: 0,
                is: is_bits(&[Is::set_uic_link_lost_status]),
                uic_busy: false,
                counts: (4, 1, 1),
                leftover: 0,
            },
            // Simulate host controller fatal error (IS.HCFES).
            Case {
                name: "host controller fatal error",
                uecpa: 0,
                uecdl: 0,
                is: is_bits(&[Is::set_host_controller_fatal_error_status]),
                uic_busy: false,
                counts: (4, 1, 2),
                leftover: 0,
            },
            // All 5 fatal status bits at once (ULLS | DFES | HCFES | SBFES | CEFES): each set bit
            // increments `fatal_error_count`.
            Case {
                name: "all fatal bits",
                uecpa: 0,
                uecdl: 0,
                is: is_bits(&[
                    Is::set_uic_link_lost_status,
                    Is::set_device_fatal_error_status,
                    Is::set_host_controller_fatal_error_status,
                    Is::set_system_bus_fatal_error_status,
                    Is::set_crypto_engine_fatal_error_status,
                ]),
                uic_busy: false,
                counts: (4, 1, 7),
                leftover: 0,
            },
        ];
        for case in cases {
            regs.store32(HostUicErrorCodePhyAdapterLayerReg::OFFSET, case.uecpa);
            regs.store32(HostUicErrorCodeDataLinkLayerReg::OFFSET, case.uecdl);
            regs.store32(InterruptStatusReg::OFFSET, case.is);
            controller.state.lock().uic_busy = case.uic_busy;

            let observed = controller.fetch_and_clear_interrupts();
            assert_eq!(observed.0, case.is, "{}", case.name);
            assert_eq!(InterruptStatusReg::read(&regs).0, case.leftover, "{}", case.name);
            let counts = (
                controller.uic_error_count(),
                controller.hibernate_error_count(),
                controller.fatal_error_count(),
            );
            assert_eq!(counts, case.counts, "{}", case.name);
            // Leftover status belongs to the UIC poller the row stands in for; drop it here.
            regs.store32(InterruptStatusReg::OFFSET, 0);
        }
        controller.state.lock().uic_busy = false;

        assert_data_tree!(inspector, root: contains {
            uic_error_count: 4u64,
            fatal_error_count: 7u64,
            auto_hibern8_error_count: 1u64,
        });
    }

    #[fuchsia::test]
    fn test_fetch_and_clear_interrupts_reads_uec_on_dump_and_once_when_combined() {
        let exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        // 1. Fatal interrupt (`IS.ULLS`) without `IS.UE` still snapshots and clears `UECPA..UECDME`
        //    once during `dump_debug_registers`.
        regs.store32(HostUicErrorCodePhyAdapterLayerReg::OFFSET, (1 << 31) | 0x02);
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_link_lost_status(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        controller.fetch_and_clear_interrupts();
        assert_eq!(controller.with_mmio(|mmio| mmio.uic_error_reads()), 5);
        assert_eq!(HostUicErrorCodePhyAdapterLayerReg::read(&regs).0, 0);

        // Advance past the ISR log rate-limit window so the next error pass also dumps registers.
        exec.set_fake_time(fasync::MonotonicInstant::now() + ISR_ERROR_LOG_INTERVAL.into());

        // 2. Combined `IS.UE | IS.ULLS` reads `UECPA..UECDME` once (5 loads total, not 10).
        regs.store32(HostUicErrorCodePhyAdapterLayerReg::OFFSET, (1 << 31) | 0x01);
        let mut is = InterruptStatusReg::from_raw(0);
        is.set_uic_error(true);
        is.set_uic_link_lost_status(true);
        regs.store32(InterruptStatusReg::OFFSET, is.0);
        controller.fetch_and_clear_interrupts();
        assert_eq!(controller.with_mmio(|mmio| mmio.uic_error_reads()), 10);
        assert_eq!(HostUicErrorCodePhyAdapterLayerReg::read(&regs).0, 0);
        assert_eq!(InterruptStatusReg::read(&regs).0, 0);
    }

    #[fuchsia::test]
    fn test_fetch_and_clear_interrupts_rate_limits_logs_without_dropping_counts() {
        let exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_zx(zx::MonotonicInstant::get()));
        let vmo = zx::Vmo::create(1024).unwrap();
        let controller = UfsController::new(FakeMmio::new(&vmo, 1024));
        let mut regs = uic_hardware(&vmo, 0);

        let fire_line_reset = |regs: &mut mmio::region::MmioRegion<mmio::vmo::VmoMemory>| {
            regs.store32(
                HostUicErrorCodePhyAdapterLayerReg::OFFSET,
                (1 << 31) | UIC_PHY_ADAPTER_LAYER_LINE_RESET,
            );
            let mut is = InterruptStatusReg::from_raw(0);
            is.set_uic_error(true);
            regs.store32(InterruptStatusReg::OFFSET, is.0);
            controller.fetch_and_clear_interrupts();
        };

        // First burst logs immediately; the next three within 5s are suppressed from syslog while
        // still clearing `UEC*`/`IS` and incrementing `uic_error_count`.
        fire_line_reset(&mut regs);
        assert_eq!(controller.uic_error_count(), 1);
        assert_eq!(controller.state.lock().suppressed_isr_errors, 0);

        for expected in 2..=4 {
            fire_line_reset(&mut regs);
            assert_eq!(controller.uic_error_count(), expected);
            assert_eq!(HostUicErrorCodePhyAdapterLayerReg::read(&regs).0, 0);
            assert_eq!(InterruptStatusReg::read(&regs).0, 0);
        }
        assert_eq!(controller.state.lock().suppressed_isr_errors, 3);

        // Once the rate-limit interval elapses, the next interrupt logs again and resets the
        // suppressed counter.
        exec.set_fake_time(fasync::MonotonicInstant::now() + ISR_ERROR_LOG_INTERVAL.into());
        fire_line_reset(&mut regs);
        assert_eq!(controller.uic_error_count(), 5);
        assert_eq!(controller.state.lock().suppressed_isr_errors, 0);
    }

    #[fuchsia::test]
    async fn test_hibernate_exit_clears_args() {
        let vmo = zx::Vmo::create(1024).unwrap();
        let mut regs = uic_hardware(&vmo, HCS_ACTIVE_LINK);
        UicCommandArgument1Reg::from_raw(0xDEAD_BEEF).write(&mut regs);

        let controller = Arc::new(UfsController::new(FakeMmio::new(&vmo, 1024)));
        // `hibernate_exit` writes `UICCMDARG1..3 = 0` before `UICCMD` (UFSHCI 4.0 section 7.1.2).
        let ready = Arc::new(Barrier::new(2));
        let ready_hw = Arc::clone(&ready);
        let exit_hw = thread::spawn(move || {
            ready_hw.wait();
            assert!(wait_until(|| uic_opcode(&regs) == UicCommandOpcode::DmeHibernateExit as u32));
            assert_eq!(UicCommandArgument1Reg::read(&regs).0, 0);
            regs.store32(
                HostControllerStatusReg::OFFSET,
                HCS_ACTIVE_LINK | hcs_power_mode_status(PowerModeStatus::PowerLocal),
            );
            complete_uic_command(&mut regs, 0, 0, |is| is.set_uic_hibernate_exit_status(true));
        });
        ready.wait();
        controller.hibernate_exit(HOST_CONTROLLER_TIMEOUT).await.unwrap();
        exit_hw.join().unwrap();
    }

    #[fuchsia::test]
    fn test_interrupt_gate_pause_resume_and_stop() {
        let gate = Arc::new(InterruptGate::new());

        // 1. `pause()` waits for an active pass to drop before returning.
        let pass = gate.enter().unwrap();
        let pause_gate = Arc::clone(&gate);
        let paused_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused_done_bg = Arc::clone(&paused_done);
        let pauser = thread::spawn(move || {
            pause_gate.pause();
            paused_done_bg.store(true, Ordering::Release);
        });
        assert!(wait_until(|| gate.state.load(Ordering::Acquire) & InterruptGate::PAUSED != 0));
        assert!(!paused_done.load(Ordering::Acquire));
        drop(pass);
        pauser.join().unwrap();
        assert!(paused_done.load(Ordering::Acquire));

        let spawn_parked = |gate: &Arc<InterruptGate>| {
            let worker_gate = Arc::clone(gate);
            let (tx, rx) = std::sync::mpsc::channel();
            let handle = thread::spawn(move || {
                let th = fuchsia_runtime::with_thread_self(|t| {
                    t.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()
                });
                tx.send(th).unwrap();
                worker_gate.enter().is_some()
            });
            let th = rx.recv().unwrap();
            assert!(wait_until(|| {
                th.info().unwrap().state == zx::ThreadState::Blocked(zx::ThreadBlockType::Futex)
            }));
            handle
        };

        // 2. While paused, `enter()` parks until `resume()`, then allows a pass again.
        let resumed = spawn_parked(&gate);
        gate.resume();
        assert!(resumed.join().unwrap());

        // 3. `stop()` unparks a paused worker and makes `enter()` return `None`.
        gate.pause();
        let stopped = spawn_parked(&gate);
        gate.stop();
        assert!(!stopped.join().unwrap());
        assert!(gate.enter().is_none());
    }

    #[fuchsia::test]
    fn test_interrupt_gate_pause_excludes_concurrent_passes() {
        let gate = Arc::new(InterruptGate::new());
        let active = Arc::new(AtomicUsize::new(0));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let gate = Arc::clone(&gate);
                let active = Arc::clone(&active);
                thread::spawn(move || {
                    while let Some(_pass) = gate.enter() {
                        active.fetch_add(1, Ordering::AcqRel);
                        std::hint::spin_loop();
                        active.fetch_sub(1, Ordering::AcqRel);
                    }
                })
            })
            .collect();

        // No pass may be active once `pause()` returns, and none may start before `resume()`.
        for _ in 0..500 {
            gate.pause();
            assert_eq!(active.load(Ordering::Acquire), 0);
            gate.resume();
        }
        gate.stop();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
