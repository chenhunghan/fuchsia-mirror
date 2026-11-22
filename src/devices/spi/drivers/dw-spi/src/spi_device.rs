// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{
    DriverError, HealthAlertLatch, HealthCheckError, HealthCheckRequest, HealthCheckResponse,
    HealthCheckResult, HealthCondition, HealthDiagnosticItem, HealthStatus, diag_bool, diag_str,
    diag_uint,
};
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_hardware_clock as fclock;
use fidl_next_fuchsia_hardware_gpio as fgpio;
use fidl_next_fuchsia_hardware_pin as fpin;
use fidl_next_fuchsia_hardware_powerdomain as fpowerdomain;
use fidl_next_fuchsia_hardware_reset as freset;
use fidl_next_fuchsia_hardware_sharedmemory as fsharedmemory;
use fidl_next_fuchsia_hardware_sharedmemory::natural::SharedVmoRight;
use fidl_next_fuchsia_hardware_spiimpl::{
    self, SpiImplExchangeVectorResponse, SpiImplReceiveVectorResponse,
    SpiImplUnregisterVmoResponse, spi_impl as fspi_impl,
};
use fidl_next_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_async::TimeoutExt;
use fuchsia_inspect as inspect;
use futures::lock::{Mutex, MutexGuard};
use futures::{FutureExt, StreamExt};
use log::{debug, error, warn};
use mmio::Register;
use mmio::region::MmioRegion;
use mmio::vmo::VmoMemory;
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use zx::Status;

pub(crate) mod registers;
use registers::DwSpiRegsBlock;

const FIFO_SIZE: usize = 256;

/// The longest a transfer waits for each FIFO threshold interrupt. A FIFO's worth of data takes far
/// less than this at any supported bus clock.
pub const INTERRUPT_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);

/// How long the controller stays powered after the last transfer before it is runtime suspended,
/// avoiding the power-up latency on every transfer of a burst.
const AUTOSUSPEND_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(200);

/// The autosuspend delay a new device starts with. Unit tests start with autosuspend disabled so
/// that they are not timing sensitive, and enable it explicitly with `set_autosuspend_delay()`.
const DEFAULT_AUTOSUSPEND_DELAY: Option<zx::MonotonicDuration> =
    if cfg!(test) { None } else { Some(AUTOSUSPEND_DELAY) };

/// Health condition reason code latched when powering up the controller fails.
pub const REASON_POWER_UP_FAILED: &str = "SPI_POWER_UP_FAILED";
/// Health condition reason code latched when releasing a power resource fails.
pub const REASON_POWER_RELEASE_FAILED: &str = "SPI_POWER_RELEASE_FAILED";

/// Health diagnostic keys.
pub const DIAG_POWERED: &str = "powered";
pub const DIAG_POWER_UP_COUNT: &str = "power_up_count";
pub const DIAG_POWER_DOWN_COUNT: &str = "power_down_count";
pub const DIAG_POWER_UP_FAILURES: &str = "power_up_failures";
pub const DIAG_POWER_DOWN_FAILURES: &str = "power_down_failures";
pub const DIAG_POWER_UP_WAITS: &str = "power_up_waits";
pub const DIAG_LAST_POWER_UP_NS: &str = "last_power_up_ns";
pub const DIAG_MAX_POWER_UP_NS: &str = "max_power_up_ns";
pub const DIAG_TOTAL_POWER_UP_NS: &str = "total_power_up_ns";
pub const DIAG_REASON: &str = "reason";

/// Pin states selected on power-up and power-down.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PinState {
    Default,
    Sleep,
}

impl PinState {
    fn as_str(self) -> &'static str {
        match self {
            PinState::Default => "default",
            PinState::Sleep => "sleep",
        }
    }
}

pub struct RegisteredVmo {
    pub vmo: fmem::natural::Range,
    pub rights: SharedVmoRight,
}

pub struct DwSpiResources {
    pub powerdomain: fidl_next::Client<fpowerdomain::Domain>,
    /// Vote on the power domain of the interrupt controller (interrupt aggregator) that aggregates
    /// our interrupt, which loses its mask state when its domain collapses. `None` when the
    /// interrupt is not routed through such a controller or the controller does not offer the
    /// vote.
    pub interrupt_controller: Option<fidl_next::Client<fpowerdomain::Domain>>,
    pub clock_bus: fidl_next::Client<fclock::Clock>,
    pub clock_regs: fidl_next::Client<fclock::Clock>,
    pub reset: fidl_next::Client<freset::Reset>,
    pub pin_states: Vec<fidl_next::Client<fpin::PinStates>>,
}

#[derive(Debug)]
pub struct DwSpiTiming {
    pub max_bus_clock_hz: u64,
    pub rx_sample_delay_ns: u64,
}

/// Runtime PM state and transition metrics, exported to Inspect under `runtime_pm` and to
/// `health_check()` diagnostics.
///
/// It is kept in atomics outside of the device mutex, so that the lazy Inspect node computes the
/// values when Inspect is read without waiting for a transfer or a power transition (see the
/// invariant on `DwSpiDevice::inner`). It is only modified with the device mutex held, and
/// `powered` uses release/acquire ordering so lock-free readers observe updated transition
/// counters (and any latched fault) once a transition finishes.
#[derive(Default)]
struct RuntimePmState {
    powered: AtomicBool,
    irq_controller_vote: AtomicBool,
    power_up_count: AtomicU64,
    power_down_count: AtomicU64,
    power_up_failures: AtomicU64,
    power_down_failures: AtomicU64,
    /// Transfers that found the controller unpowered and had to wait for a power-up.
    power_up_waits: AtomicU64,
    last_power_up_ns: AtomicU64,
    max_power_up_ns: AtomicU64,
    total_power_up_ns: AtomicU64,
}

impl RuntimePmState {
    fn increment(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn record_power_up(&self, elapsed_ns: u64) {
        self.power_up_count.fetch_add(1, Ordering::Relaxed);
        self.last_power_up_ns.store(elapsed_ns, Ordering::Relaxed);
        self.max_power_up_ns.fetch_max(elapsed_ns, Ordering::Relaxed);
        self.total_power_up_ns.fetch_add(elapsed_ns, Ordering::Relaxed);
    }

    fn record(&self, node: &inspect::Node) {
        let powered = self.powered.load(Ordering::Acquire);
        node.record_bool(DIAG_POWERED, powered);
        node.record_bool("irq_controller_vote", self.irq_controller_vote.load(Ordering::Relaxed));
        node.record_uint(DIAG_POWER_UP_COUNT, self.power_up_count.load(Ordering::Relaxed));
        node.record_uint(DIAG_POWER_DOWN_COUNT, self.power_down_count.load(Ordering::Relaxed));
        node.record_uint(DIAG_POWER_UP_FAILURES, self.power_up_failures.load(Ordering::Relaxed));
        node.record_uint(
            DIAG_POWER_DOWN_FAILURES,
            self.power_down_failures.load(Ordering::Relaxed),
        );
        node.record_uint(DIAG_POWER_UP_WAITS, self.power_up_waits.load(Ordering::Relaxed));
        node.record_uint(DIAG_LAST_POWER_UP_NS, self.last_power_up_ns.load(Ordering::Relaxed));
        node.record_uint(DIAG_MAX_POWER_UP_NS, self.max_power_up_ns.load(Ordering::Relaxed));
        node.record_uint(DIAG_TOTAL_POWER_UP_NS, self.total_power_up_ns.load(Ordering::Relaxed));
    }

    fn diagnostics(&self) -> Vec<HealthDiagnosticItem> {
        let powered = self.powered.load(Ordering::Acquire);
        vec![
            diag_bool(DIAG_POWERED, powered),
            diag_uint(DIAG_POWER_UP_COUNT, self.power_up_count.load(Ordering::Relaxed)),
            diag_uint(DIAG_POWER_DOWN_COUNT, self.power_down_count.load(Ordering::Relaxed)),
            diag_uint(DIAG_POWER_UP_FAILURES, self.power_up_failures.load(Ordering::Relaxed)),
            diag_uint(DIAG_POWER_DOWN_FAILURES, self.power_down_failures.load(Ordering::Relaxed)),
            diag_uint(DIAG_POWER_UP_WAITS, self.power_up_waits.load(Ordering::Relaxed)),
            diag_uint(DIAG_LAST_POWER_UP_NS, self.last_power_up_ns.load(Ordering::Relaxed)),
            diag_uint(DIAG_MAX_POWER_UP_NS, self.max_power_up_ns.load(Ordering::Relaxed)),
            diag_uint(DIAG_TOTAL_POWER_UP_NS, self.total_power_up_ns.load(Ordering::Relaxed)),
        ]
    }
}

/// The resources acquired so far by a power-up, in power-up order.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum PowerStage {
    Off,
    Domain,
    InterruptController,
    BusClock,
    RegistersClock,
}

/// Converts a FIDL transport error to a `Status`, keeping `PEER_CLOSED` distinguishable.
fn transport_status(error: &fidl_next::Error) -> Status {
    match error {
        fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed) => Status::PEER_CLOSED,
        fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosedWithEpitaph(status)) => {
            Status::try_from_raw(*status).unwrap_or(Status::PEER_CLOSED)
        }
        _ => Status::INTERNAL,
    }
}

/// Converts the result of a two-way FIDL call with an error syntax to a `Status`, logging any
/// failure at error.
fn check_fidl<T>(
    result: Result<Result<T, Status>, fidl_next::Error>,
    action: &str,
) -> Result<T, Status> {
    let status = match result {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(status)) => status,
        Err(e) => {
            debug!("FIDL transport error: {e}");
            transport_status(&e)
        }
    };
    error!("Failed to {action}: {status}");
    Err(status)
}

struct DwSpiDeviceInner {
    mmio: DwSpiRegsBlock<MmioRegion<VmoMemory>>,
    cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
    /// The controller interrupt. Waited on without blocking the executor.
    irq: Pin<Box<fasync::OnInterrupt>>,
    /// Whether the last interrupt has not been acknowledged yet. Like `zx_interrupt_wait()`, the
    /// previous interrupt is acknowledged right before waiting for the next one.
    irq_needs_ack: bool,
    /// The longest a transfer waits for each interrupt.
    interrupt_timeout: zx::MonotonicDuration,
    resources: DwSpiResources,
    registered_vmos: HashMap<u32, RegisteredVmo>,
    loopback_registered_vmos: HashMap<u32, RegisteredVmo>,
    // Whether the power domain, clock and interrupt controller votes are held (`powered()`),
    // whether the interrupt controller vote is held, and the power transition counters.
    runtime_pm: Arc<RuntimePmState>,
    health_latch: HealthAlertLatch,
    // Set between system suspend and resume. Transfers are rejected rather than powering the
    // controller up while the system suspends.
    system_suspended: bool,
    // Set when the driver stops. Transfers are rejected so the votes can't be taken again.
    stopped: bool,
    // For each of `resources.pin_states`, whether it reported that it has no "sleep" state.
    pin_states_without_sleep: Vec<bool>,
    // Incremented whenever the pending autosuspend (if any) becomes stale.
    autosuspend_generation: u64,
    autosuspend_delay: Option<zx::MonotonicDuration>,
    // Dropping the task cancels the pending autosuspend.
    autosuspend_timer: Option<fasync::Task<()>>,
    // Cached SCKDV and RSD register field values, restored on every power-up.
    sckdv: Option<u32>,
    rsd: Option<u32>,
}

#[derive(Clone)]
pub struct DwSpiDevice {
    // Using a Mutex to guard DwSpiDeviceInner guarantees that we can make async calls without
    // worrying about SPI requests, autosuspend and suspend/resume requests being interleaved.
    //
    // Invariant: no task on a power-managed dispatcher may await this mutex or hold it across an
    // `.await`. Such a task can be parked when its dispatcher suspends, and its wakeup is then
    // deferred until resume, so `system_suspend()` would wait for the mutex forever. Everything
    // that takes it runs on the driver's fuchsia-async executor, which is not power managed.
    inner: Arc<Mutex<DwSpiDeviceInner>>,
    // The driver's scope on the fuchsia-async executor, which runs the autosuspend timer.
    scope: fasync::ScopeHandle,
    // Shared with `inner`, read by the lazy Inspect node and `health_check()` without taking the
    // mutex.
    runtime_pm: Arc<RuntimePmState>,
    health_latch: HealthAlertLatch,
}

/// Access to a powered controller for the duration of a transfer. Arms the autosuspend timer when
/// dropped.
struct PoweredDevice<'a> {
    device: &'a DwSpiDevice,
    inner: MutexGuard<'a, DwSpiDeviceInner>,
}

impl Deref for PoweredDevice<'_> {
    type Target = DwSpiDeviceInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for PoweredDevice<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for PoweredDevice<'_> {
    fn drop(&mut self) {
        self.device.schedule_autosuspend(&mut self.inner);
    }
}

impl DwSpiDevice {
    pub fn new(
        mmio: MmioRegion<VmoMemory>,
        cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
        interrupt: zx::Interrupt,
        resources: DwSpiResources,
        scope: fasync::ScopeHandle,
        health_latch: HealthAlertLatch,
    ) -> Self {
        let runtime_pm = Arc::new(RuntimePmState::default());
        Self {
            inner: Arc::new(Mutex::new(DwSpiDeviceInner::new(
                mmio,
                cs_gpio,
                interrupt,
                resources,
                runtime_pm.clone(),
                health_latch.clone(),
            ))),
            scope,
            runtime_pm,
            health_latch,
        }
    }

    pub fn health_check(&self, request: HealthCheckRequest) -> HealthCheckResult {
        match request {
            HealthCheckRequest::Ping => {
                let mut response = HealthCheckResponse {
                    diagnostics: Some(self.runtime_pm.diagnostics()),
                    conditions: None,
                };
                self.health_latch.merge_into(&mut response);
                Ok(response)
            }
            _ => Err(HealthCheckError::NotSupported),
        }
    }

    /// Exports the runtime PM state to Inspect as the lazy child `runtime_pm` of `node`. The values
    /// are only computed when Inspect is read, without taking the device mutex.
    pub fn record_inspect(&self, node: &inspect::Node) {
        let runtime_pm = self.runtime_pm.clone();
        node.record_lazy_child("runtime_pm", move || {
            let inspector = inspect::Inspector::default();
            runtime_pm.record(inspector.root());
            futures::future::ok(inspector).boxed()
        });
    }

    /// Powers up and initializes the controller, then arms the autosuspend timer so that it is
    /// powered down once the bus has been idle for the autosuspend delay.
    pub async fn init(&self, timing: DwSpiTiming) -> Result<(), DriverError> {
        let mut inner = self.inner.lock().await;
        inner.init(timing).await?;
        self.schedule_autosuspend(&mut inner);
        Ok(())
    }

    /// Powers the controller down immediately if it is powered.
    pub async fn power_off(&self) -> Result<(), DriverError> {
        let mut inner = self.inner.lock().await;
        inner.power_off().await?;
        Ok(())
    }

    /// Powers the controller down for good: transfers are rejected from now on, so the votes
    /// cannot be taken again before the driver is gone.
    pub async fn stop(&self) -> Result<(), DriverError> {
        let mut inner = self.inner.lock().await;
        inner.stopped = true;
        inner.power_off().await?;
        Ok(())
    }

    /// Powers the controller down for system suspend. This never fails: even on failure the
    /// controller is no longer considered powered, and any failed release is surfaced in
    /// `health_check` as `SPI_POWER_RELEASE_FAILED`. Failing suspend would make the driver host
    /// stop the driver for no benefit, so the failure is only logged.
    pub async fn suspend(&self) {
        let mut inner = self.inner.lock().await;
        inner.system_suspended = true;
        if let Err(e) = inner.power_off().await {
            warn!("Failed to release all power votes on suspend: {e}");
        }
    }

    /// Does not power up eagerly. The next transfer powers the controller up and reinitializes it.
    pub async fn resume(&self) {
        self.inner.lock().await.system_suspended = false;
    }

    /// Changes the autosuspend delay. `None` disables autosuspend. If the controller is powered,
    /// the autosuspend timer is re-armed with the new delay.
    #[cfg(test)]
    pub fn set_autosuspend_delay(&self, delay: Option<zx::MonotonicDuration>) {
        let mut inner = futures::executor::block_on(self.inner.lock());
        inner.autosuspend_delay = delay;
        self.schedule_autosuspend(&mut inner);
    }

    #[cfg(test)]
    pub fn set_interrupt_timeout(&self, timeout: zx::MonotonicDuration) {
        futures::executor::block_on(self.inner.lock()).interrupt_timeout = timeout;
    }

    #[cfg(test)]
    pub fn is_powered(&self) -> bool {
        self.runtime_pm.powered.load(Ordering::Acquire)
    }

    /// Locks the device and makes sure that the controller is powered, cancelling any pending
    /// autosuspend. The returned guard re-arms the autosuspend timer when dropped.
    async fn lock_powered(&self) -> Result<PoweredDevice<'_>, Status> {
        let mut inner = self.inner.lock().await;
        if inner.stopped {
            return Err(Status::BAD_STATE);
        }
        if inner.system_suspended {
            // A request that was read before the dispatcher was paused can get here after
            // system_suspend(). Don't take the power votes across system suspend.
            warn!("Rejecting transfer while system suspended");
            return Err(Status::BAD_STATE);
        }
        inner.cancel_autosuspend();
        if !inner.powered() {
            RuntimePmState::increment(&self.runtime_pm.power_up_waits);
            // On failure nothing is held, so there is nothing to autosuspend.
            inner.power_up().await?;
        }
        Ok(PoweredDevice { device: self, inner })
    }

    fn schedule_autosuspend(&self, inner: &mut DwSpiDeviceInner) {
        inner.cancel_autosuspend();
        if !inner.powered() {
            return;
        }
        let Some(delay) = inner.autosuspend_delay else {
            return;
        };
        let generation = inner.autosuspend_generation;
        let weak_inner: Weak<Mutex<DwSpiDeviceInner>> = Arc::downgrade(&self.inner);
        // Runs on the fuchsia-async executor rather than a power-managed dispatcher, see the
        // invariant on `inner`. System suspend cancels it under the lock, and the checks below
        // make a timer that already fired and is waiting for the lock a no-op.
        let task = self.scope.compute(async move {
            fasync::Timer::new(fasync::MonotonicInstant::after(delay)).await;
            let Some(inner) = weak_inner.upgrade() else {
                return;
            };
            let mut inner = inner.lock().await;
            // A transfer, system suspend or a newer timer may have raced with this one.
            if inner.autosuspend_generation != generation
                || !inner.powered()
                || inner.system_suspended
            {
                return;
            }
            debug!("Runtime suspending after {} ms idle", delay.into_millis());
            if let Err(e) = inner.power_down().await {
                warn!("Failed to runtime suspend: {e}");
            }
        });
        inner.autosuspend_timer = Some(task);
    }
}

impl DwSpiDeviceInner {
    fn new(
        mmio: MmioRegion<VmoMemory>,
        cs_gpio: Option<fidl_next::Client<fgpio::Gpio>>,
        interrupt: zx::Interrupt,
        resources: DwSpiResources,
        runtime_pm: Arc<RuntimePmState>,
        health_latch: HealthAlertLatch,
    ) -> Self {
        let pin_states_without_sleep = vec![false; resources.pin_states.len()];
        Self {
            mmio: DwSpiRegsBlock { mmio },
            cs_gpio,
            irq: Box::pin(fasync::OnInterrupt::new(interrupt)),
            irq_needs_ack: false,
            interrupt_timeout: INTERRUPT_TIMEOUT,
            resources,
            registered_vmos: HashMap::new(),
            loopback_registered_vmos: HashMap::new(),
            runtime_pm,
            health_latch,
            system_suspended: false,
            stopped: false,
            pin_states_without_sleep,
            autosuspend_generation: 0,
            autosuspend_delay: DEFAULT_AUTOSUSPEND_DELAY,
            autosuspend_timer: None,
            sckdv: None,
            rsd: None,
        }
    }

    fn cancel_autosuspend(&mut self) {
        self.autosuspend_generation = self.autosuspend_generation.wrapping_add(1);
        // The timer task never drops its own handle, so this never aborts a running power-down.
        self.autosuspend_timer = None;
    }

    async fn power_off(&mut self) -> Result<(), Status> {
        self.cancel_autosuspend();
        if self.powered() { self.power_down().await } else { Ok(()) }
    }

    fn record_power_up_failure(&self, reason: &'static str) {
        RuntimePmState::increment(&self.runtime_pm.power_up_failures);
        self.health_latch.raise(HealthCondition {
            status: Some(HealthStatus::Unhealthy),
            reason_code: Some(REASON_POWER_UP_FAILED.to_string()),
            message: Some("Failed to power up SPI controller".to_string()),
            diagnostics: Some(vec![diag_str(DIAG_REASON, reason)]),
            ..Default::default()
        });
    }

    fn record_power_release_failure(&self, reason: &'static str) {
        RuntimePmState::increment(&self.runtime_pm.power_down_failures);
        self.health_latch.raise(HealthCondition {
            status: Some(HealthStatus::Unhealthy),
            reason_code: Some(REASON_POWER_RELEASE_FAILED.to_string()),
            message: Some("Failed to release SPI controller power resource".to_string()),
            diagnostics: Some(vec![diag_str(DIAG_REASON, reason)]),
            ..Default::default()
        });
    }

    /// Whether the power domain, clock and interrupt controller votes are held. The provider keeps
    /// a single reference count per resource, so votes are only taken or released when this
    /// changes. The controller registers must not be accessed while this is false.
    fn powered(&self) -> bool {
        self.runtime_pm.powered.load(Ordering::Relaxed)
    }

    fn set_powered(&self, powered: bool) {
        self.runtime_pm.powered.store(powered, Ordering::Release);
    }

    /// Whether the interrupt controller vote is currently held.
    fn interrupt_controller_voted(&self) -> bool {
        self.runtime_pm.irq_controller_vote.load(Ordering::Relaxed)
    }

    fn set_interrupt_controller_voted(&self, voted: bool) {
        self.runtime_pm.irq_controller_vote.store(voted, Ordering::Relaxed);
    }

    /// Selects `state` on every pin controller, continuing past failures. Returns the first error.
    /// Boards are not required to provide a "sleep" state: a missing one is skipped quietly, and
    /// after the first `NOT_FOUND` it is not requested from that pin controller again (the pin
    /// states protocol has no way to query the available states up front).
    async fn select_pin_states(&mut self, pin_state: PinState) -> Result<(), Status> {
        let is_sleep = pin_state == PinState::Sleep;
        let state = pin_state.as_str();
        let mut result = Ok(());
        for (index, pin_states) in self.resources.pin_states.iter().enumerate() {
            if is_sleep && self.pin_states_without_sleep[index] {
                continue;
            }
            match pin_states.select_state(state).await {
                Ok(Ok(_)) => {}
                Ok(Err(Status::NOT_FOUND)) if is_sleep => {
                    self.pin_states_without_sleep[index] = true;
                }
                Ok(Err(s)) => {
                    error!("Failed to select pin state {state}: {s}");
                    result = result.and(Err(s));
                }
                // Pin states are optional, so ignore closed channels when a composite parent is
                // absent.
                Err(fidl_next::Error::Protocol(fidl_next::ProtocolError::PeerClosed)) => {
                    debug!("Pin states channel closed while selecting {state}");
                }
                Err(e) => {
                    let s = transport_status(&e);
                    error!("Failed to select pin state {state}: {e}");
                    result = result.and(Err(s));
                }
            }
        }
        result
    }

    async fn init(&mut self, timing: DwSpiTiming) -> Result<(), DriverError> {
        self.power_up().await?;

        let parent_clock_hz = self
            .resources
            .clock_bus
            .get_rate()
            .await
            .map_err(|_| Status::INTERNAL)
            .flatten()
            .inspect_err(|e| {
                error!("Failed to get bus clock rate: {e:?}");
            })
            .unwrap_or(fclock::ClockGetRateResponse { hz: 0 })
            .hz;

        if timing.max_bus_clock_hz > 0 {
            // The caller powers off on any init() failure.
            let (sckdv, rsd) = Self::get_sckdv_rsd(timing, parent_clock_hz)?;
            self.sckdv = Some(sckdv);
            self.rsd = Some(rsd);
        } else {
            warn!("Max bus clock rate reported to be zero, keeping the current baud rate divider");
            // power_down() clears BAUDR, so cache the divider and RX sample delay in effect now and
            // restore them on every power-up. power_up() leaves them alone while they are not
            // cached, so these are the values present after the reset toggle and power-up, which
            // are the bootloader's only if the reset does not clear them.
            let sckdv = self.mmio.baudr().read().sckdv();
            if sckdv != 0 {
                self.sckdv = Some(sckdv);
                self.rsd = Some(self.mmio.rx_sample_dly().read().rsd());
            }
        }

        // Program the baud rate and RX sample delay computed above.
        self.init_registers();

        Ok(())
    }

    /// Takes the power resources and selects the default pin state, returning the failed action
    /// and the stage reached on failure so that acquired votes can be rolled back.
    async fn acquire_resources(&mut self) -> Result<(), (Status, &'static str, PowerStage)> {
        check_fidl(self.resources.powerdomain.enable().await, "enable power domain")
            .map_err(|s| (s, "enable power domain", PowerStage::Off))?;

        if let Some(interrupt_controller) = &self.resources.interrupt_controller {
            check_fidl(
                interrupt_controller.enable().await,
                "enable interrupt controller power domain",
            )
            .map_err(|s| (s, "enable interrupt controller power domain", PowerStage::Domain))?;
            self.set_interrupt_controller_voted(true);
        }

        // Enable the SPI functional clock ("bus") before the APB registers clock ("registers").
        check_fidl(self.resources.clock_bus.enable().await, "enable bus clock")
            .map_err(|s| (s, "enable bus clock", PowerStage::InterruptController))?;

        check_fidl(self.resources.clock_regs.enable().await, "enable registers clock")
            .map_err(|s| (s, "enable registers clock", PowerStage::BusClock))?;

        check_fidl(self.resources.reset.toggle().await, "toggle reset")
            .map_err(|s| (s, "toggle reset", PowerStage::RegistersClock))?;

        self.select_pin_states(PinState::Default)
            .await
            .map_err(|s| (s, "select default pin state", PowerStage::RegistersClock))?;
        Ok(())
    }

    /// Releases the resources up to and including `stage`, in reverse power-up order, without
    /// retries. Every resource is released even if releasing another one fails, except that the
    /// power domain is not disabled if disabling a clock failed (so the domain never powers off
    /// under a still-prepared clock). Returns the first error and its action description.
    async fn release_resources(&mut self, stage: PowerStage) -> Result<(), (Status, &'static str)> {
        let mut result = Ok(());
        let mut clock_disable_failed = false;
        if stage >= PowerStage::RegistersClock
            && let Err(s) =
                check_fidl(self.resources.clock_regs.disable().await, "disable registers clock")
        {
            clock_disable_failed = true;
            result = result.and(Err((s, "disable registers clock")));
        }
        if stage >= PowerStage::BusClock
            && let Err(s) =
                check_fidl(self.resources.clock_bus.disable().await, "disable bus clock")
        {
            clock_disable_failed = true;
            result = result.and(Err((s, "disable bus clock")));
        }
        if stage >= PowerStage::InterruptController && self.interrupt_controller_voted() {
            self.set_interrupt_controller_voted(false);
            if let Some(interrupt_controller) = &self.resources.interrupt_controller
                && let Err(s) = check_fidl(
                    interrupt_controller.disable().await,
                    "disable interrupt controller power domain",
                )
            {
                result = result.and(Err((s, "disable interrupt controller power domain")));
            }
        }
        // Never disable the power domain while a clock vote is still held.
        if stage >= PowerStage::Domain
            && !clock_disable_failed
            && let Err(s) =
                check_fidl(self.resources.powerdomain.disable().await, "disable power domain")
        {
            result = result.and(Err((s, "disable power domain")));
        }
        result
    }

    /// Power-up order: power domain -> interrupt controller vote -> bus clock -> registers clock ->
    /// reset toggle -> default pin state -> register initialization. Rolls back on failure.
    async fn power_up(&mut self) -> Result<(), Status> {
        debug_assert!(!self.powered());
        if self.powered() {
            return Ok(());
        }
        let start = zx::MonotonicInstant::get();
        if let Err((status, failed_action, stage)) = self.acquire_resources().await {
            self.record_power_up_failure(failed_action);
            if let Err((rollback_err, rollback_action)) = self.release_resources(stage).await {
                warn!("Failed to roll back power-up: {rollback_err}");
                self.record_power_release_failure(rollback_action);
            }
            return Err(status);
        }

        // Register state is lost while the power domain is off, so always reinitialize.
        self.init_registers();
        let elapsed_ns =
            u64::try_from((zx::MonotonicInstant::get() - start).into_nanos()).unwrap_or(0);
        self.runtime_pm.record_power_up(elapsed_ns);
        self.health_latch.clear(REASON_POWER_UP_FAILED);
        self.set_powered(true);
        Ok(())
    }

    /// Power-down order: sleep pin state -> quiesce the controller -> registers clock ->
    /// bus clock -> interrupt controller vote -> power domain. Releases are never retried, the
    /// power domain vote is kept if a clock Disable fails, and the registers are not touched again
    /// until the next successful power-up.
    async fn power_down(&mut self) -> Result<(), Status> {
        debug_assert!(self.powered());
        if !self.powered() {
            return Ok(());
        }

        let pin_result = self
            .select_pin_states(PinState::Sleep)
            .await
            .map_err(|s| (s, "select sleep pin state"));

        // Mask all interrupts, disable the SPI peripheral and stop the serial clock so that nothing
        // is left asserted towards the interrupt controller while power is removed.
        self.mmio.imr_mut().write(registers::Imr::from_raw(0));
        self.mmio.ssi_enr_mut().write(registers::SsiEnr::from_raw(0));
        self.mmio.baudr_mut().write(registers::Baudr::from_raw(0));

        // From here on the registers must not be touched. Update counters and latch any release
        // fault before clearing `powered` so lock-free `health_check()` and Inspect never observe
        // `powered == false` with stale counters or before the fault is latched.
        let release_result = self.release_resources(PowerStage::RegistersClock).await;
        let result = pin_result.and(release_result);
        RuntimePmState::increment(&self.runtime_pm.power_down_count);
        if let Err((_, failed_action)) = result {
            self.record_power_release_failure(failed_action);
        }
        self.set_powered(false);
        result.map_err(|(status, _)| status)
    }

    fn get_sckdv_rsd(timing: DwSpiTiming, parent_clock_hz: u64) -> Result<(u32, u32), Status> {
        let max_bus_clock_hz = timing.max_bus_clock_hz;
        let rx_sample_delay_ns = timing.rx_sample_delay_ns;

        if parent_clock_hz == 0 {
            error!("Unknown parent clock rate, can't compute the baud rate divider");
            return Err(Status::BAD_STATE);
        }

        // Round the divider up to avoid overclocking.
        let Some(numerator) = parent_clock_hz.checked_add(max_bus_clock_hz - 1) else {
            error!(
                "Unsupported max bus clock {max_bus_clock_hz} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        };

        let divider = numerator / max_bus_clock_hz;
        if divider >= 0xffff {
            error!(
                "Unsupported max bus clock {max_bus_clock_hz} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        }

        // The divider must be even.
        let divider = if divider % 2 == 0 { divider } else { divider + 1 };

        // Convert the RX delay from nanoseconds to parent clock cycles.
        let Some(numerator) = rx_sample_delay_ns.checked_mul(parent_clock_hz) else {
            error!(
                "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        };

        let rx_sample_delay = Duration::from_nanos(numerator);
        let rx_sample_delay_clocks = rx_sample_delay.as_secs();
        // Verify that the clock count fits in the register, and that the conversion from
        // nanoseconds to clock cycles did not result in rounding.
        if rx_sample_delay_clocks > 0xff || rx_sample_delay.subsec_nanos() != 0 {
            error!(
                "Unsupported RX delay {rx_sample_delay_ns} for parent clock rate {parent_clock_hz}"
            );
            return Err(Status::INVALID_ARGS);
        }

        Ok((divider as u32, rx_sample_delay_clocks as u32))
    }

    fn init_registers(&mut self) {
        self.mmio.ssi_enr_mut().write(registers::SsiEnr::from_raw(0));

        self.mmio.ctrlr0_mut().write({
            let mut ctrlr0 = registers::CtrlR0::from_raw(0);
            ctrlr0.set_spi_frf(0); // Standard SPI
            ctrlr0.set_frf(0); // Motorola SPI
            ctrlr0.set_dfs(7); // 8-bit (values 3-15 correspond to 4-16 bits, so 7 means 8 bits)
            ctrlr0.set_tmod(0); // Transmit & Receive
            ctrlr0
        });

        if let (Some(sckdv), Some(rsd)) = (self.sckdv, self.rsd) {
            self.mmio.baudr_mut().write({
                let mut baudr = registers::Baudr::from_raw(0);
                baudr.set_sckdv(sckdv);
                baudr
            });
            self.mmio.rx_sample_dly_mut().write({
                let mut rx_sample_dly = registers::RxSampleDly::from_raw(0);
                rx_sample_dly.set_rsd(rsd);
                rx_sample_dly
            });
        }

        // Mask all interrupts initially in IMR
        self.mmio.imr_mut().write(registers::Imr::from_raw(0));

        // Clear any pending interrupts and deassert target select lines
        let _ = self.mmio.icr().read();
        self.mmio.ser_mut().write(registers::Ser::from_raw(0));

        // Configure the controller to interrupt us when the TX FIFO is half empty.
        self.mmio.txftlr_mut().write({
            let mut txftlr = registers::Txftlr::from_raw(0);
            txftlr.set_tft((FIFO_SIZE / 2).try_into().unwrap());
            txftlr
        });

        // Enable SSI
        self.mmio.ssi_enr_mut().write({
            let mut ssi_enr = registers::SsiEnr::from_raw(0);
            ssi_enr.set_ssi_en(true);
            ssi_enr
        });
    }

    async fn exchange_pio_loop(
        &mut self,
        mut txdata: &[u8],
        rx: bool,
        size: usize,
    ) -> Result<Vec<u8>, Status> {
        // Don't unmask RXFIM since we know there won't be any RX data to start.
        self.mmio.imr_mut().write({
            let mut imr = registers::Imr::from_raw(0);
            imr.set_rxoim(true);
            imr.set_rxuim(true);
            imr.set_txoim(true);
            imr.set_txeim(true);
            imr
        });

        if self.mmio.sr().read().rfne() {
            warn!("RX FIFO is not empty before starting transfer");
        }

        let mut tx_remaining = size;
        let mut rx_remaining = size;

        let mut rxdata = Vec::<u8>::with_capacity(if rx { size } else { 0 });

        loop {
            let isr = self.mmio.isr().read();

            debug!("Interrupt {:#02x}: TX: {tx_remaining} RX: {rx_remaining}", isr.to_raw());

            if isr.txois() || isr.rxuis() || isr.rxois() {
                warn!("Unexpected interrupt {:#02x}", isr.to_raw());
                return Err(Status::IO);
            }
            if !isr.txeis() && !isr.rxfis() {
                warn!("Spurious interrupt {:#02x}", isr.to_raw());
            }

            // The controller may still be draining the TX FIFO and filling the RX FIFO while we're
            // handling this interrupt. Reading TX words first prevents us from accidentally
            // overflowing the RX FIFO due to the RX count increasing after we've read it.
            let tx_words = self.mmio.txflr().read().txtfl() as usize;
            let rx_words = self.mmio.rxflr().read().rxtfl() as usize;

            assert!(tx_words <= FIFO_SIZE);
            // Fill the TX FIFO up to FIFO_SIZE - 1. If a byte is already in the TX shift register,
            // filling up to the maximum could cause an RX FIFO overflow.
            let tx_free = (FIFO_SIZE - tx_words).saturating_sub(1);

            debug!("  RX words {rx_words}, TX words {tx_words} (free {tx_free})");

            // Drain the RX FIFO first. If it's full, writing to the TX FIFO first will overflow it.
            let transfer_size = std::cmp::min(rx_remaining, rx_words);
            for _ in 0..transfer_size {
                let data = self.mmio.dr0().read().dr() as u8;
                if rx {
                    rxdata.push(data);
                }
            }

            rx_remaining -= transfer_size;

            self.mmio.rxftlr_mut().write({
                // If there are FIFO_SIZE or fewer bytes left to receive, we can configure the
                // controller to wait for the FIFO to be completely full before interrupting us.
                // Otherwise, have it interrupt us when the FIFO is half full.
                let fifo_level =
                    if rx_remaining <= FIFO_SIZE { rx_remaining.max(1) } else { FIFO_SIZE / 2 };
                let mut rxftlr = registers::Rxftlr::from_raw(0);
                rxftlr.set_rft((fifo_level - 1).try_into().unwrap());
                debug!("  New RXFTLR: {:#02x}", rxftlr.to_raw());
                rxftlr
            });

            // Next fill the TX FIFO.
            let transfer_size = std::cmp::min(tx_remaining, tx_free);
            for i in 0..transfer_size {
                let data = if txdata.len() > 0 { txdata[i] } else { 0xFF };
                self.mmio.dr0_mut().write(registers::Dr0::from_raw(data as u32));
            }

            tx_remaining -= transfer_size;
            if txdata.len() > 0 {
                txdata = &txdata[transfer_size..];
            }

            // Enable/disable FIFO threshold interrupts depending on how many bytes we have left to
            // transmit/receive.
            self.mmio.imr_mut().update(|imr| {
                imr.set_txeim(tx_remaining > 0);
                imr.set_rxfim(rx_remaining > 0);
                debug!("  New IMR: {:#02x}", imr.to_raw());
            });

            if tx_remaining == 0 && rx_remaining == 0 {
                break;
            }

            if let Err(status) = self.wait_for_interrupt().await {
                self.mmio.imr_mut().write(registers::Imr::from_raw(0));
                // Flush both FIFOs so stale data does not leak into the next transfer, and drop
                // any interrupt that arrives late.
                self.mmio.ssi_enr_mut().write(registers::SsiEnr::from_raw(0));
                self.mmio.ssi_enr_mut().write({
                    let mut ssi_enr = registers::SsiEnr::from_raw(0);
                    ssi_enr.set_ssi_en(true);
                    ssi_enr
                });
                self.drain_interrupts();
                return Err(status);
            }
        }

        self.mmio.imr_mut().write(registers::Imr::from_raw(0));

        Ok(rxdata)
    }

    fn ack_interrupt(&self) -> Result<(), Status> {
        let interrupt: &zx::Interrupt = (*self.irq).as_ref();
        interrupt.ack()
    }

    /// Waits for the next controller interrupt, acknowledging the previous one first, without
    /// blocking the executor. Fails with `TIMED_OUT` rather than waiting forever if no interrupt
    /// arrives, e.g. because the interrupt controller masked the line.
    async fn wait_for_interrupt(&mut self) -> Result<(), Status> {
        if self.irq_needs_ack {
            self.ack_interrupt()?;
            self.irq_needs_ack = false;
        }
        let deadline = fasync::MonotonicInstant::after(self.interrupt_timeout);
        match self.irq.next().on_timeout(deadline, || None).await {
            Some(Ok(_)) => {
                self.irq_needs_ack = true;
                Ok(())
            }
            Some(Err(status)) => {
                error!("Failed to wait for interrupt: {status}");
                Err(status)
            }
            None => {
                error!("Timed out waiting for interrupt");
                Err(Status::TIMED_OUT)
            }
        }
    }

    /// Acknowledges the pending interrupt and any that already arrived.
    fn drain_interrupts(&mut self) {
        if self.irq_needs_ack {
            // A failure is logged; there is nothing else to do about it here.
            let _: Result<(), Status> =
                self.ack_interrupt().inspect_err(|s| warn!("Failed to ack interrupt: {s}"));
            self.irq_needs_ack = false;
        }
        while let Some(Some(Ok(_))) = self.irq.next().now_or_never() {
            let _: Result<(), Status> =
                self.ack_interrupt().inspect_err(|s| warn!("Failed to ack interrupt: {s}"));
        }
    }

    // Takes `&mut self` so that the returned future is `Send`: the interrupt stream makes
    // `DwSpiDeviceInner` `Send` but not `Sync`.
    async fn set_cs_state(&mut self, chip_select: u32, assert: bool) -> Result<(), Status> {
        if chip_select == 0 {
            if let Some(cs_gpio) = &self.cs_gpio {
                let mode = if assert {
                    fgpio::natural::BufferMode::OutputLow
                } else {
                    fgpio::natural::BufferMode::OutputHigh
                };
                cs_gpio.set_buffer_mode(mode).wire().await.map_err(|e| {
                    error!("Failed to toggle CS: {e}");
                    Status::IO
                })?;
            }
        }
        Ok(())
    }

    async fn exchange_pio(
        &mut self,
        chip_select: u32,
        txdata: &[u8],
        rx: bool,
        size: usize,
    ) -> Result<Vec<u8>, Status> {
        if size == 0 {
            return Ok(vec![]);
        }

        assert!(txdata.len() > 0 || rx); // If there is no TX data then we must be receiving.
        assert!(txdata.len() == 0 || txdata.len() == size); // TX size must match RX size.

        let enable_loopback =
            chip_select == fidl_next_fuchsia_hardware_spiimpl::LOOPBACK_CHIP_SELECT;

        // Only one chip select is supported for now.
        if chip_select != 0 && !enable_loopback {
            return Err(Status::NOT_FOUND);
        }

        if self.mmio.ctrlr0().read().srl() != enable_loopback {
            self.mmio.ssi_enr_mut().write({
                let mut ssi_enr = registers::SsiEnr::from_raw(0);
                ssi_enr.set_ssi_en(false);
                ssi_enr
            });

            self.mmio.ctrlr0_mut().update(|ctrlr0| {
                ctrlr0.set_srl(enable_loopback);
            });

            self.mmio.ssi_enr_mut().write({
                let mut ssi_enr = registers::SsiEnr::from_raw(0);
                ssi_enr.set_ssi_en(true);
                ssi_enr
            });
        }

        self.set_cs_state(chip_select, true).await?;

        // TODO(https://fxbug.dev/529838127): Support DMA transfers for larger sizes.
        // This is a placeholder indicating where DMA support would be added.
        // For now, we only implement PIO.

        // A target must be selected before the transfer can begin.
        self.mmio.ser_mut().write({
            let mut ser = registers::Ser::from_raw(0);
            ser.set_ser(1);
            ser
        });

        let rxdata = self.exchange_pio_loop(txdata, rx, size).await;

        self.mmio.ser_mut().write(registers::Ser::from_raw(0));

        self.set_cs_state(chip_select, false).await?;

        rxdata
    }

    fn get_vmos(&self, chip_select: u32) -> Result<&HashMap<u32, RegisteredVmo>, Status> {
        match chip_select {
            0 => Ok(&self.registered_vmos),
            fidl_next_fuchsia_hardware_spiimpl::LOOPBACK_CHIP_SELECT => {
                Ok(&self.loopback_registered_vmos)
            }
            _ => Err(Status::NOT_FOUND),
        }
    }

    fn get_vmos_mut(
        &mut self,
        chip_select: u32,
    ) -> Result<&mut HashMap<u32, RegisteredVmo>, Status> {
        match chip_select {
            0 => Ok(&mut self.registered_vmos),
            fidl_next_fuchsia_hardware_spiimpl::LOOPBACK_CHIP_SELECT => {
                Ok(&mut self.loopback_registered_vmos)
            }
            _ => Err(Status::NOT_FOUND),
        }
    }

    pub fn register_vmo_impl(
        &mut self,
        chip_select: u32,
        vmo_id: u32,
        vmo: fmem::natural::Range,
        rights: SharedVmoRight,
    ) -> Result<(), Status> {
        let vmos = self.get_vmos_mut(chip_select)?;
        if vmos.contains_key(&vmo_id) {
            return Err(Status::ALREADY_EXISTS);
        }

        let vmo_size = vmo.vmo.get_size()?;
        if let Some(end) = vmo.offset.checked_add(vmo.size) {
            if end > vmo_size {
                error!("VMO range end {} is greater than VMO size {}", end, vmo_size);
                return Err(Status::INVALID_ARGS);
            }
        } else {
            error!("VMO offset {} and size {} overflow", vmo.offset, vmo.size);
            return Err(Status::INVALID_ARGS);
        }

        vmos.insert(vmo_id, RegisteredVmo { vmo, rights });
        Ok(())
    }

    pub fn unregister_vmo_impl(
        &mut self,
        chip_select: u32,
        vmo_id: u32,
    ) -> Result<zx::Vmo, Status> {
        let vmos = self.get_vmos_mut(chip_select)?;
        if let Some(registered) = vmos.remove(&vmo_id) {
            Ok(registered.vmo.vmo)
        } else {
            Err(Status::NOT_FOUND)
        }
    }

    pub fn release_registered_vmos_impl(&mut self, chip_select: u32) {
        if let Ok(vmos) = self.get_vmos_mut(chip_select) {
            vmos.clear();
        }
    }

    fn get_validated_vmo(
        &self,
        chip_select: u32,
        buffer: &fsharedmemory::natural::SharedVmoBuffer,
        required_right: SharedVmoRight,
    ) -> Result<(&zx::Vmo, u64), Status> {
        let vmos = self.get_vmos(chip_select)?;
        let registered = vmos.get(&buffer.vmo_id).ok_or(Status::NOT_FOUND)?;

        if !registered.rights.contains(required_right) {
            return Err(Status::ACCESS_DENIED);
        }

        let end = buffer.offset.checked_add(buffer.size).ok_or(Status::OUT_OF_RANGE)?;
        if end > registered.vmo.size {
            return Err(Status::OUT_OF_RANGE);
        }

        let final_offset =
            registered.vmo.offset.checked_add(buffer.offset).ok_or(Status::OUT_OF_RANGE)?;
        Ok((&registered.vmo.vmo, final_offset))
    }

    pub async fn transmit_vmo_impl(
        &mut self,
        chip_select: u32,
        buffer: &fsharedmemory::natural::SharedVmoBuffer,
    ) -> Result<(), Status> {
        // TODO(https://fxbug.dev/529838127): Use DMA instead of copying into/out of vectors.
        let mut tx_data = vec![0u8; buffer.size as usize];
        {
            let (vmo, offset) =
                self.get_validated_vmo(chip_select, buffer, SharedVmoRight::READ)?;
            vmo.read(&mut tx_data, offset)?;
        }

        self.exchange_pio(chip_select, &tx_data, false, tx_data.len()).await?;
        Ok(())
    }

    pub async fn receive_vmo_impl(
        &mut self,
        chip_select: u32,
        buffer: &fsharedmemory::natural::SharedVmoBuffer,
    ) -> Result<(), Status> {
        let rx_data = self.exchange_pio(chip_select, &[], true, buffer.size as usize).await?;

        // TODO(https://fxbug.dev/529838127): Use DMA instead of copying into/out of vectors.
        let (vmo, offset) = self.get_validated_vmo(chip_select, buffer, SharedVmoRight::WRITE)?;
        vmo.write(&rx_data, offset)?;
        Ok(())
    }

    pub async fn exchange_vmo_impl(
        &mut self,
        chip_select: u32,
        tx_buffer: &fsharedmemory::natural::SharedVmoBuffer,
        rx_buffer: &fsharedmemory::natural::SharedVmoBuffer,
    ) -> Result<(), Status> {
        if tx_buffer.size != rx_buffer.size {
            return Err(Status::INVALID_ARGS);
        }

        // TODO(https://fxbug.dev/529838127): Use DMA instead of copying into/out of vectors.
        let mut tx_data = vec![0u8; tx_buffer.size as usize];
        {
            let (tx_vmo, tx_offset) =
                self.get_validated_vmo(chip_select, tx_buffer, SharedVmoRight::READ)?;
            tx_vmo.read(&mut tx_data, tx_offset)?;
        }

        let rx_data = self.exchange_pio(chip_select, &tx_data, true, tx_data.len()).await?;

        let (rx_vmo, rx_offset) =
            self.get_validated_vmo(chip_select, rx_buffer, SharedVmoRight::WRITE)?;
        rx_vmo.write(&rx_data, rx_offset)?;
        Ok(())
    }
}

// Transfers power the controller up on demand (see `DwSpiDevice::lock_powered()`) and arm the
// autosuspend timer when they complete. Requests that do not touch the hardware only take the lock.
impl fidl_next_fuchsia_hardware_spiimpl::SpiImplServerHandler for DwSpiDevice {
    async fn get_chip_select_count(&mut self, responder: Responder<fspi_impl::GetChipSelectCount>) {
        let _ = responder.respond(1).await;
    }

    async fn transmit_vector(
        &mut self,
        request: Request<fspi_impl::TransmitVector>,
        responder: Responder<fspi_impl::TransmitVector>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => device
                .exchange_pio(payload.chip_select, &payload.data, false, payload.data.len())
                .await
                .map(|_| ()),
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }

    async fn receive_vector(
        &mut self,
        request: Request<fspi_impl::ReceiveVector>,
        responder: Responder<fspi_impl::ReceiveVector>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => device
                .exchange_pio(payload.chip_select, &[], true, payload.size as usize)
                .await
                .map(|data| SpiImplReceiveVectorResponse { data }),
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }

    async fn exchange_vector(
        &mut self,
        request: Request<fspi_impl::ExchangeVector>,
        responder: Responder<fspi_impl::ExchangeVector>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => device
                .exchange_pio(payload.chip_select, &payload.txdata, true, payload.txdata.len())
                .await
                .map(|rxdata| SpiImplExchangeVectorResponse { rxdata }),
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }

    // Bus locking is not supported, so chip select is never held asserted across requests and the
    // controller can always be runtime suspended between transfers.
    async fn lock_bus(
        &mut self,
        _request: Request<fspi_impl::LockBus>,
        responder: Responder<fspi_impl::LockBus>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED).await;
    }

    async fn unlock_bus(
        &mut self,
        _request: Request<fspi_impl::UnlockBus>,
        responder: Responder<fspi_impl::UnlockBus>,
    ) {
        let _ = responder.respond_err(Status::NOT_SUPPORTED).await;
    }

    async fn register_vmo(
        &mut self,
        request: Request<fspi_impl::RegisterVmo>,
        responder: Responder<fspi_impl::RegisterVmo>,
    ) {
        let payload = request.payload();
        let result = self.inner.lock().await.register_vmo_impl(
            payload.chip_select,
            payload.vmo_id,
            payload.vmo,
            payload.rights,
        );
        let _ = responder.respond_with(result).await;
    }

    async fn unregister_vmo(
        &mut self,
        request: Request<fspi_impl::UnregisterVmo>,
        responder: Responder<fspi_impl::UnregisterVmo>,
    ) {
        let payload = request.payload();
        let result = self
            .inner
            .lock()
            .await
            .unregister_vmo_impl(payload.chip_select, payload.vmo_id)
            .map(|vmo| SpiImplUnregisterVmoResponse { vmo });
        let _ = responder.respond_with(result).await;
    }

    async fn release_registered_vmos(
        &mut self,
        request: Request<fspi_impl::ReleaseRegisteredVmos>,
    ) {
        let payload = request.payload();
        self.inner.lock().await.release_registered_vmos_impl(payload.chip_select);
    }

    async fn transmit_vmo(
        &mut self,
        request: Request<fspi_impl::TransmitVmo>,
        responder: Responder<fspi_impl::TransmitVmo>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => device.transmit_vmo_impl(payload.chip_select, &payload.buffer).await,
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }

    async fn receive_vmo(
        &mut self,
        request: Request<fspi_impl::ReceiveVmo>,
        responder: Responder<fspi_impl::ReceiveVmo>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => device.receive_vmo_impl(payload.chip_select, &payload.buffer).await,
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }

    async fn exchange_vmo(
        &mut self,
        request: Request<fspi_impl::ExchangeVmo>,
        responder: Responder<fspi_impl::ExchangeVmo>,
    ) {
        let payload = request.payload();
        let result = match self.lock_powered().await {
            Ok(mut device) => {
                device
                    .exchange_vmo_impl(payload.chip_select, &payload.tx_buffer, &payload.rx_buffer)
                    .await
            }
            Err(status) => Err(status),
        };
        let _ = responder.respond_with(result).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_baud_rate_and_delay() {
        let (sckdv, rsd) = DwSpiDeviceInner::get_sckdv_rsd(
            DwSpiTiming { max_bus_clock_hz: 20_000_000, rx_sample_delay_ns: 25 },
            200_000_000,
        )
        .unwrap();

        assert_eq!(sckdv, 10);
        assert_eq!(rsd, 5);
    }

    #[test]
    fn test_set_baud_rate_too_slow() {
        let result = DwSpiDeviceInner::get_sckdv_rsd(
            DwSpiTiming { max_bus_clock_hz: 2_000, rx_sample_delay_ns: 0 },
            200_000_000,
        );
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }

    #[test]
    fn test_set_baud_divider_rounded_up() {
        let (sckdv, rsd) = DwSpiDeviceInner::get_sckdv_rsd(
            DwSpiTiming { max_bus_clock_hz: 3_600_000, rx_sample_delay_ns: 0 },
            200_000_000,
        )
        .unwrap();

        assert_eq!(sckdv, 56); // Divider rounded up to 56.
        assert_eq!(rsd, 0);
    }

    #[test]
    fn test_set_baud_rate_invalid_delay_remainder() {
        let result = DwSpiDeviceInner::get_sckdv_rsd(
            DwSpiTiming { max_bus_clock_hz: 20_000_000, rx_sample_delay_ns: 28 },
            200_000_000,
        );
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }

    #[test]
    fn test_set_baud_rate_invalid_delay_too_large() {
        let result = DwSpiDeviceInner::get_sckdv_rsd(
            DwSpiTiming { max_bus_clock_hz: 20_000_000, rx_sample_delay_ns: 5000 },
            200_000_000,
        );
        assert_eq!(result.unwrap_err(), Status::INVALID_ARGS);
    }
}
