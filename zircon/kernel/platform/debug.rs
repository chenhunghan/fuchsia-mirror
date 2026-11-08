// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::InterruptDisableGuard;
use crate::cbuf::Cbuf;
#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::debuglog_rs::{dlog_bypass, dlog_panic_started};
use crate::dev_interrupt::{
    InterruptHandler, InterruptPolarity, InterruptTriggerMode, configure_interrupt, mask_interrupt,
    register_permanent_int_handler, unmask_interrupt,
};
use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::event::{AutounsignalEvent, Event};
use crate::kernel::thread::{self, Interruptible};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::ksync_kernel::declare_singleton_mutex;
use crate::platform_rs::timer::{
    DurationMono, DurationUnknown, InstantBoot, InstantUnknown, current_boot_time,
    current_mono_time, zx_msec, zx_usec,
};
#[cfg(console_enabled)]
use crate::platform_rs::timer::{zx_nsec, zx_sec};
use crate::root_resource_filter::root_resource_filter_add_deny_region;
use boot_options::BootOptions;
use core::ffi::{c_char, c_int, c_void};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use core::{ptr, slice};
use debug::dprintf;
use debug::dprintf::{INFO, dprintf_enabled};
#[cfg(any(console_enabled, ktest))]
use kprint::kprint;
use lazy_init::LazyInit;
use pin_init::{pin_data, pin_init};
use zbi::KernelDriverIrqFlags;
use zx_status::Status;
#[cfg(console_enabled)]
use zx_types::ZX_ERR_INTERNAL;
use zx_types::{
    ZX_OK, ZX_RSRC_KIND_IOPORT, ZX_RSRC_KIND_MMIO, zx_instant_boot_t, zx_instant_mono_t,
    zx_status_t,
};

const ZBI_KERNEL_DRIVER_IRQ_FLAGS_EDGE_TRIGGERED: u32 = KernelDriverIrqFlags::EDGE_TRIGGERED.bits();
const ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED: u32 =
    KernelDriverIrqFlags::LEVEL_TRIGGERED.bits();
const ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_LOW: u32 = KernelDriverIrqFlags::POLARITY_LOW.bits();
const ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH: u32 = KernelDriverIrqFlags::POLARITY_HIGH.bits();

/// Opaque C++ `uart::all::Driver` type.
#[repr(C)]
pub struct UartDriver {
    _opaque: [u8; 0],
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum UartDriverKind {
    #[default]
    Null = 0,
    Mmio = 1,
    Pio = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct UartDriverInfo {
    kind: UartDriverKind,
    has_irq: bool,
    irq: u32,
    has_simple_cfg: bool,
    cfg_flags: u32,
    io_base: usize,
    io_size: usize,
}

zr::static_assert_size_and_align!(UartDriverKind, 4, 4);
zr::static_assert_size_and_align!(UartDriverInfo, 40, 8);

unsafe extern "C" {
    fn cpp_uart_enabled() -> bool;
    fn cpp_uart_set_driver(serial: *const UartDriver);
    fn cpp_uart_unparse(buf: *mut c_char, len: usize) -> usize;
    fn cpp_uart_get_driver_info(out: *mut UartDriverInfo);
    fn cpp_uart_init_interrupt(uart_irq: u32);
    fn cpp_uart_interrupt();
    fn cpp_uart_enable_rx_interrupt();
    fn cpp_uart_tx_drained() -> bool;
    fn cpp_uart_prepare_for_suspend();
    fn cpp_uart_wakeup_from_suspend();
    fn cpp_uart_read(out_c: *mut u8) -> bool;
    fn cpp_uart_write(str: *const c_char, len: usize, blocking: bool);
    fn cpp_uart_pputc(c: c_char);
}

static IS_TX_IRQ_ENABLED: AtomicBool = AtomicBool::new(false);

/// Phase of [`UartGate`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UartGatePhase {
    Running = 0,
    Draining = 1,
    Suspended = 2,
}

/// `UartGate` arbitrates access to the UART hardware so that usermode may suspend
/// (and later resume) all kernel access to the hardware via `zx_debug_suspend`
/// and `zx_debug_resume`, e.g. so that it may power down the UART.
///
/// Every code path which touches the UART hardware must first successfully
/// "enter" the gate, and then "exit" it when it is finished.  The gate's state
/// is a single atomic word (a phase plus a count of active users), so entering
/// (and `exit_without_waking`) may be used from any context, including interrupt
/// handlers, while holding spinlocks, and during a panic.  `exit` may also wake
/// the suspending thread, so it may only be used from contexts in which it is
/// safe to signal an `Event`.
///
/// The gate has three phases:
///
///  - `Running`: Normal operation.  Anyone may enter.
///  - `Draining`: A suspend operation is in progress.  New operations are
///    refused, but operations which are required to allow in-flight operations
///    to complete (e.g. the UART IRQ handler) may still enter.  The suspending
///    thread waits (see `wait_for_no_active_users`) for the active count to drop to
///    zero, and then moves the gate to `Suspended`.  Alternatively, the
///    suspending thread may abandon the suspend operation (e.g. because it was
///    interrupted), moving the gate back to `Running`.
///  - `Suspended`: No one may enter.  Only the thread performing a suspend or
///    resume operation (serialized by `UartSuspendResumeLock`) may touch the
///    hardware.
struct UartGate {
    state: AtomicU32,
    /// Signaled by `exit` when the last active user exits while draining.  This
    /// is an `AutounsignalEvent` so that both signaling (`signal`) and consuming a
    /// signal (`wait`) are serialized by the event's lock.
    drained: LazyInit<AutounsignalEvent>,
}

impl UartGate {
    const PHASE_SHIFT: u32 = 30;
    const COUNT_MASK: u32 = (1u32 << Self::PHASE_SHIFT) - 1;

    /// The maximum amount of time `wait_for_no_active_users` waits between checks for
    /// active users.  Bounds the latency added when the last active user exits via
    /// `exit_without_waking`.
    const DRAIN_POLL_INTERVAL: DurationMono = DurationMono(zx_msec(10));

    const fn new() -> Self {
        Self {
            state: AtomicU32::new(Self::make_state(UartGatePhase::Running, 0)),
            drained: LazyInit::uninit(),
        }
    }

    const fn make_state(phase: UartGatePhase, count: u32) -> u32 {
        ((phase as u32) << Self::PHASE_SHIFT) | count
    }

    const fn phase_of(state: u32) -> UartGatePhase {
        match state >> Self::PHASE_SHIFT {
            0 => UartGatePhase::Running,
            1 => UartGatePhase::Draining,
            _ => UartGatePhase::Suspended,
        }
    }

    /// Enter the gate for a normal operation.  Fails unless the gate is running.
    fn try_enter(&self) -> bool {
        self.try_enter_while(|p| p == UartGatePhase::Running)
    }

    /// Enter the gate for an operation which may be needed to allow in-flight
    /// operations to complete.  Fails only if the gate is suspended.
    fn try_enter_unless_suspended(&self) -> bool {
        self.try_enter_while(|p| p != UartGatePhase::Suspended)
    }

    /// Exit the gate, waking the thread blocked in `wait_for_no_active_users` (if
    /// any) if the caller was the last active user while the gate was draining.
    ///
    /// Must only be called from contexts in which it is safe to signal an `Event`:
    /// thread context, or interrupt context (hard IRQ or timer callback), with no
    /// `ChainLockTransaction` active and without holding any spinlock which may be
    /// acquired while holding a chainlock.
    fn exit(&self) {
        if self.exit_internal() {
            self.drained.signal();
        }
    }

    /// Exit the gate without waking the thread blocked in `wait_for_no_active_users`.
    /// May be called from any context.
    ///
    /// If the caller is the last active user to exit while a thread is blocked in
    /// `wait_for_no_active_users`, that thread will not notice until its next poll
    /// (see `DRAIN_POLL_INTERVAL`).
    fn exit_without_waking(&self) {
        let _ = self.exit_internal();
    }

    /// Waits, interruptibly, until there are no active users.  Must only be called
    /// by the suspend/resume owner while the gate is draining.
    ///
    /// Operations which may enter while draining (see `try_enter_unless_suspended`)
    /// may do so at any time, so there may be active users again by the time this
    /// returns.
    ///
    /// Returns `Ok(())`, or `ZX_ERR_INTERNAL_INTR_KILLED` or
    /// `ZX_ERR_INTERNAL_INTR_RETRY` if the calling thread was killed or
    /// suspended while waiting.
    fn wait_for_no_active_users(&self) -> Result<(), Status> {
        debug_assert!(self.phase() == UartGatePhase::Draining);
        // Normally, the last active user to exit wakes us via `exit`.  However,
        // some users exit via `exit_without_waking`, so bound each wait and check
        // again.  A stale signal (e.g. from an abandoned suspend operation, or from
        // a user which entered and exited while draining) merely causes us to check
        // again.
        while self.has_active_users() {
            let status = self
                .drained
                .wait(&Deadline::after_mono(Self::DRAIN_POLL_INTERVAL, TimerSlack::none()));
            if status != Ok(()) && status != Err(Status::TIMED_OUT) {
                return status;
            }
        }
        Ok(())
    }

    fn phase(&self) -> UartGatePhase {
        Self::phase_of(self.state.load(Ordering::Acquire))
    }

    /// Returns true if there are any operations currently inside the gate.
    fn has_active_users(&self) -> bool {
        (self.state.load(Ordering::Acquire) & Self::COUNT_MASK) != 0
    }

    /// Move from `Running` to `Draining`.  Must only be called by the
    /// suspend/resume owner while the gate is running.
    ///
    /// This synchronizes with every successful entry which precedes it, so
    /// anything which happened before such an entry also happens before the
    /// caller's subsequent operations.
    fn begin_drain(&self) {
        let prev =
            self.state.fetch_add(Self::make_state(UartGatePhase::Draining, 0), Ordering::AcqRel);
        debug_assert!(Self::phase_of(prev) == UartGatePhase::Running);
    }

    /// Move from `Draining` back to `Running`, abandoning a suspend operation.  Must
    /// only be called by the suspend/resume owner while the gate is draining.
    /// Operations which are still inside the gate are unaffected.
    ///
    /// Sequentially consistent; see `try_enter_while`.
    fn cancel_drain(&self) {
        let prev =
            self.state.fetch_sub(Self::make_state(UartGatePhase::Draining, 0), Ordering::SeqCst);
        debug_assert!(Self::phase_of(prev) == UartGatePhase::Draining);
    }

    /// Attempt to move from `Draining` to `Suspended`.  Succeeds only if there are
    /// no active users.
    fn try_finish_drain(&self) -> bool {
        let expected = Self::make_state(UartGatePhase::Draining, 0);
        self.state
            .compare_exchange(
                expected,
                Self::make_state(UartGatePhase::Suspended, 0),
                Ordering::Acquire,
                Ordering::Relaxed,
            )
            .is_ok()
    }

    /// Move from `Suspended` to `Running`.  Must only be called by the
    /// suspend/resume owner while the gate is suspended.
    ///
    /// Sequentially consistent; see `try_enter_while`.
    fn resume(&self) {
        debug_assert!(
            self.state.load(Ordering::Relaxed) == Self::make_state(UartGatePhase::Suspended, 0)
        );
        self.state.store(Self::make_state(UartGatePhase::Running, 0), Ordering::SeqCst);
    }

    fn try_enter_while(&self, allowed: impl Fn(UartGatePhase) -> bool) -> bool {
        // Entry, and the operations which reopen the gate (`resume` and
        // `cancel_drain`), are sequentially consistent.  This allows callers to
        // reliably hand off work using the pattern
        //
        //   caller:  flag.store(true, seq_cst); if (try_enter()) { consume flag }
        //   owner:   resume(); if (try_enter()) { consume flag }
        //
        // (where consuming the flag is a seq_cst exchange), guaranteeing that at
        // least one of them consumes it.  With weaker orderings, the caller could
        // observe the gate closed while the owner observes the flag unset.
        //
        // A refused caller also observes everything which happened before the
        // phase change that caused it to be refused (e.g. `G_UART_RESUMED_EVENT` being
        // unsignaled).
        let mut state = self.state.load(Ordering::SeqCst);
        loop {
            if !allowed(Self::phase_of(state)) {
                return false;
            }
            debug_assert!((state & Self::COUNT_MASK) != Self::COUNT_MASK);
            let Err(actual) = self.state.compare_exchange_weak(
                state,
                state + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) else {
                break;
            };
            state = actual;
        }
        true
    }

    /// Returns true if the caller was the last active user while the gate was
    /// draining.
    fn exit_internal(&self) -> bool {
        let prev = self.state.fetch_sub(1, Ordering::Release);
        debug_assert!((prev & Self::COUNT_MASK) != 0);
        prev == Self::make_state(UartGatePhase::Draining, 1)
    }
}

/// Guards all kernel access to the UART hardware so it can be suspended/resumed.
static G_UART_GATE: UartGate = UartGate::new();

struct KernelIrqProvider;

impl KernelIrqProvider {
    fn set_interrupts_enabled(irq_num: u32, enabled: bool) {
        if enabled {
            // While kernel access to the UART is suspended, the IRQ must remain
            // masked; the IRQ handler refuses to touch the hardware, so a pending
            // (level-triggered) IRQ would otherwise fire repeatedly.  This matters
            // when `platform_serial_resume` calls the driver's `WakeupFromSuspend`
            // hook (which may attempt to unmask the IRQ) before it has reopened the
            // gate.  `platform_serial_resume` unmasks the IRQ itself once the gate has
            // been reopened.
            if G_UART_GATE.phase() == UartGatePhase::Suspended {
                return;
            }
            let _ = unmask_interrupt(irq_num);
        } else {
            let _ = mask_interrupt(irq_num);
        }
    }
}

// Serializes the gate phase transitions (and associated hardware operations)
// made by platform_serial_suspend and platform_serial_resume.  This lock is
// only ever held briefly: it is never held while waiting for in-flight
// operations to drain, so that callers blocked acquiring it are never blocked
// indefinitely (Mutex acquisition is not interruptible).
declare_singleton_mutex!(UartSuspendResumeLock);

/// Signaled whenever the UART is not suspended (or when writes are being
/// discarded, see below).  Threads performing blocking writes wait on this (see
/// `platform_serial_wait_for_resume`) when they are unable to enter the gate.
static G_UART_RESUMED_EVENT: LazyInit<Event> = LazyInit::uninit();

/// When set, blocking writes which cannot proceed because the UART is suspended
/// are discarded rather than blocking.  Set (permanently) when the debuglog is
/// shut down as part of halting the system, so that a suspended UART cannot
/// prevent the system from halting.  Once set, `platform_serial_suspend` refuses
/// to start a new suspend operation.
///
/// Only written while holding `UartSuspendResumeLock`, which serializes it with
/// the suspending thread's check (and unsignaling of `G_UART_RESUMED_EVENT`).  Once
/// set, `G_UART_RESUMED_EVENT` remains signaled.  May be read without the lock.
static G_UART_DISCARD_WHILE_SUSPENDED: AtomicBool = AtomicBool::new(false);

/// The boot time at which the UART was last suspended.  Reported in the crashlog
/// if we panic while the UART is suspended.
static G_UART_SUSPENDED_AT: AtomicI64 = AtomicI64::new(0);

/// Set when a reader drains the RX queue from full while the UART is suspended
/// (and therefore could not re-enable the RX interrupt).  Consumed by whoever
/// next manages to enter the gate (the reader itself, or the operation which
/// reopens the gate: resume, or an abandoned suspend).
///
/// All accesses are sequentially consistent, as are entering and reopening the
/// gate (see `UartGate::try_enter_while`), so that if a reader is refused entry, the
/// operation which later reopens the gate is guaranteed to observe the flag.
static G_UART_RX_ENABLE_PENDING: AtomicBool = AtomicBool::new(false);

/// The UART's IRQ, if the UART is being driven by interrupts (as opposed to
/// being polled).  Written once during `uart_driver_handoff_late`.
static G_UART_IRQ: LazyInit<Option<u32>> = LazyInit::uninit();

/// Re-enable the RX interrupt if a reader requested it while the UART was
/// suspended.  The caller must have entered the gate.
fn maybe_enable_rx_interrupt() {
    if G_UART_RX_ENABLE_PENDING.swap(false, Ordering::SeqCst) {
        // SAFETY: The caller has entered `G_UART_GATE`, so the UART is not suspended.
        unsafe { cpp_uart_enable_rx_interrupt() };
    }
}

/// Called by the suspend/resume owner just after reopening the gate (see
/// `G_UART_RX_ENABLE_PENDING`).  Re-enables the RX interrupt on behalf of any reader
/// which was refused entry while the gate was closed.
fn enable_rx_interrupt_if_pending_after_reopen() {
    if G_UART_GATE.try_enter() {
        maybe_enable_rx_interrupt();
        G_UART_GATE.exit();
    }
}

/// Returns true if all output previously handed to the UART has been
/// transmitted.  Does not wait.  Used by `platform_serial_suspend` while the gate
/// is draining.
fn uart_tx_drained() -> bool {
    let mut drained = true;
    if G_UART_GATE.try_enter_unless_suspended() {
        // SAFETY: We have entered `G_UART_GATE` (not suspended).
        drained = unsafe { cpp_uart_tx_drained() };
        // We're called by the suspending thread itself, so there's no one to wake.
        G_UART_GATE.exit_without_waking();
    }
    drained
}

/// Enter the gate for non-blocking I/O which may be performed during a panic
/// (output, and polling for panic shell input).
///
/// Normally, such I/O is refused once a suspend operation has begun.  However,
/// once the system has started to panic, it may also enter while a suspend
/// operation is draining: the hardware is still powered (usermode has not yet
/// been told that the suspend succeeded), and the suspending thread will never
/// complete anyway, so observing us as an active user does not matter.
fn try_enter_for_panic_io() -> bool {
    if dlog_panic_started() {
        G_UART_GATE.try_enter_unless_suspended()
    } else {
        G_UART_GATE.try_enter()
    }
}

/// Initialized by `uart_driver_handoff_late`, provides buffered output of the UART,
/// which helps readers catch up.
/// Also provides synchronization mechanisms for character availability.
static RX_QUEUE: LazyInit<Cbuf> = LazyInit::uninit();

/// Size of the RX queue. The bigger the buffer, the bigger the window for
/// the reader to catch up. Useful when the incoming data is bursty.
const RX_QUEUE_SIZE: usize = 1024;

#[pin_data]
struct PollTimer {
    #[pin]
    timer: Timer,
}

// SAFETY: `G_UART_POLL_TIMER` is only scheduled from `uart_driver_handoff_late` and its own
// callback (`uart_poll`), and `Timer` is internally synchronized in C++ via `TimerLock`.
unsafe impl Sync for PollTimer {}

/// When polling is enabled, this will fire the polling callback for draining UART's RX queue.
static G_UART_POLL_TIMER: LazyInit<PollTimer> = LazyInit::uninit();

const POLLING_PERIOD: DurationMono = DurationMono(zx_msec(10));
const POLLING_SLACK: TimerSlack = TimerSlack::new(DurationUnknown(zx_msec(10)), SlackMode::Center);

/// Callback used by `G_UART_POLL_TIMER` when deadline is met. See `Timer` interface for more
/// information.
///
/// # Safety
///
/// `uart_timer` must point to a valid, pinned `Timer` instance.
unsafe extern "C" fn uart_poll<const DRAIN_UART: bool>(
    uart_timer: *mut Timer,
    now: zx_instant_mono_t,
    _arg: *mut c_void,
) {
    // SAFETY: `uart_timer` points to the pinned `G_UART_POLL_TIMER` static timer.
    unsafe {
        Pin::new_unchecked(&*uart_timer).set_deadline(
            &Deadline::new(InstantUnknown(now.saturating_add(POLLING_PERIOD.0)), POLLING_SLACK),
            uart_poll::<true>,
            ptr::null_mut(),
        );
    }
    if DRAIN_UART {
        // If the UART is suspended, skip this poll, but leave the timer running so
        // that we resume polling once the UART has been resumed.
        if !G_UART_GATE.try_enter() {
            return;
        }
        // Drain until there is nothing else in the RX Queue of the device.
        let mut c = 0u8;
        // SAFETY: We have entered `G_UART_GATE` so the UART is not suspended.
        while unsafe { cpp_uart_read(&mut c) } {
            RX_QUEUE.write_char(c);
        }
        // Timer callbacks may signal events.
        G_UART_GATE.exit();
    }
}

struct IrqConfig {
    trigger: InterruptTriggerMode,
    polarity: InterruptPolarity,
}

fn get_irq_config_from_flags(uart_flags: u32) -> Option<IrqConfig> {
    if uart_flags == 0 {
        return None;
    }

    if (uart_flags
        & (ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED
            | ZBI_KERNEL_DRIVER_IRQ_FLAGS_EDGE_TRIGGERED))
        == 0
    {
        return None;
    }

    if (uart_flags
        & (ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH | ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_LOW))
        == 0
    {
        return None;
    }

    // In order to configure the IRQ all information, trigger and polarity must be provided.
    // Otherwise the step must be omitted and let defaults take over.
    let trigger = if (uart_flags & ZBI_KERNEL_DRIVER_IRQ_FLAGS_LEVEL_TRIGGERED) != 0 {
        InterruptTriggerMode::Level
    } else {
        InterruptTriggerMode::Edge
    };
    let polarity = if (uart_flags & ZBI_KERNEL_DRIVER_IRQ_FLAGS_POLARITY_HIGH) != 0 {
        InterruptPolarity::High
    } else {
        InterruptPolarity::Low
    };
    Some(IrqConfig { trigger, polarity })
}

/// Returns true if this system has a debug serial port that is enabled.
pub fn platform_serial_enabled() -> bool {
    // SAFETY: `cpp_uart_enabled` inspects the static `gUart` variant without side effects.
    unsafe { cpp_uart_enabled() }
}

/// Waits, interruptibly, until all output previously handed to the UART has been
/// transmitted.  The gate must be draining with no writers still inside, so that
/// no new output can be queued.
///
/// UARTs generally do not raise an interrupt when the transmitter becomes
/// completely empty (e.g. LSR.TEMT on the 8250), so we must poll.  Since no new
/// output can be queued, this normally takes no longer than it takes to transmit
/// a FIFO's worth of data.  We sleep between polls rather than spinning with the
/// driver's lock held so that we never hold a spinlock (or disable interrupts)
/// for long.
fn wait_for_tx_drained() -> Result<(), Status> {
    const MIN_POLL_INTERVAL: DurationMono = DurationMono(zx_usec(100));
    const MAX_POLL_INTERVAL: DurationMono = DurationMono(zx_msec(10));
    let mut poll_interval = MIN_POLL_INTERVAL;
    while !uart_tx_drained() {
        thread::sleep_interruptible(current_mono_time() + poll_interval)?;
        poll_interval = core::cmp::min(poll_interval * 2, MAX_POLL_INTERVAL);
    }
    Ok(())
}

/// Suspend all kernel access to the debug UART hardware, on behalf of
/// `zx_debug_suspend`.
///
/// Upon successful return, the kernel will not touch the UART hardware (directly
/// or indirectly) until `platform_serial_resume` has been called.  In particular:
///
///  - Blocking writes (`platform_dputs_thread`) write nothing and return
///    `ZX_ERR_SHOULD_WAIT`.  Callers may wait for the UART to be resumed using
///    `platform_serial_wait_for_resume`.
///  - Non-blocking writes (`platform_dputs_irq`, `platform_pputc`) are dropped.
///  - Polling reads (`platform_pgetc`) report no data.
///  - The UART interrupt is masked.
///
/// Must be called from thread context.  Blocks, interruptibly, until any
/// in-progress UART operations have completed and any output they queued in the
/// hardware has been transmitted.  Transmission is detected via the driver's
/// `TxDrained` method; drivers which do not implement it are assumed to have
/// drained immediately, so with those drivers output still in the hardware FIFO
/// may be lost.  This is normally brief, but may take indefinitely long if the
/// hardware is unable to make progress (e.g. TX is stalled by flow control).  If
/// the wait is interrupted, the suspend operation is abandoned and the UART is
/// left running.  No spinlocks are held for long, and no lock is held while
/// waiting, so concurrent callers are never blocked for long.
///
/// While the suspend operation is in progress, `platform_serial_is_suspended`
/// returns true, writers are refused (as though the UART were suspended), and
/// concurrent calls to `platform_serial_suspend` and `platform_serial_resume`
/// fail with `ZX_ERR_BAD_STATE`.
///
/// Returns:
///  - `Ok(())` on success.
///  - `Err(Status::NOT_SUPPORTED)` if there is no debug UART, or if the debuglog is
///    being bypassed.
///  - `Err(Status::BAD_STATE)` if the UART is already suspended, another suspend
///    operation is in progress, or the system is halting (see
///    `platform_serial_discard_writes_while_suspended`).
///  - `ZX_ERR_INTERNAL_INTR_KILLED` or `ZX_ERR_INTERNAL_INTR_RETRY` if the calling
///    thread was killed or suspended while waiting.  The UART is not suspended.
pub fn platform_serial_suspend() -> Result<(), Status> {
    if !platform_serial_enabled() || dlog_bypass() {
        return Err(Status::NOT_SUPPORTED);
    }

    {
        ksync::lock!(let _guard = UartSuspendResumeLock::Get().lock());
        if G_UART_GATE.phase() != UartGatePhase::Running {
            return Err(Status::BAD_STATE);
        }

        // If the system is halting, refuse to suspend, since writers must never
        // block once writes are being discarded (see `G_UART_DISCARD_WHILE_SUSPENDED`).
        // The flag is only set while holding UartSuspendResumeLock, so it cannot be
        // set until we have finished starting the suspend operation.
        if G_UART_DISCARD_WHILE_SUSPENDED.load(Ordering::Relaxed) {
            return Err(Status::BAD_STATE);
        }

        // Unsignal the event before we start refusing entry to the gate, so that
        // any blocking writer which is refused will block until we resume (or
        // cancel).
        let _ = G_UART_RESUMED_EVENT.unsignal();
        // Record the suspend time before the gate starts reporting that the UART is
        // suspended, so that a crashlog never reports a stale time.
        G_UART_SUSPENDED_AT.store(current_boot_time().0, Ordering::Relaxed);
        G_UART_GATE.begin_drain();
    }

    // Wait for any in-flight operations to finish, and then for any output they
    // queued in the hardware to be transmitted.  Note that we leave the UART IRQ
    // unmasked while we wait; writers which are blocked waiting for space in the
    // TX FIFO need the IRQ handler in order to make progress.
    //
    // Some operations exit the gate via `UartGate::exit_without_waking` because they
    // run in contexts in which signaling an Event is unsafe; the bounded wait in
    // `UartGate::wait_for_no_active_users` covers them.
    //
    // In-flight operations and queued output normally drain quickly, but if the
    // hardware is unable to make progress (e.g. TX is stalled by flow control)
    // they may never drain.  So, wait interruptibly and without holding
    // UartSuspendResumeLock.  If we are interrupted, abandon the suspend
    // operation and leave the UART running.  While we wait, concurrent
    // suspend/resume calls fail with ZX_ERR_BAD_STATE.
    loop {
        // Once there are no active users, no new output can be queued: writers are
        // refused entry while draining, and the operations which may still enter
        // (e.g. the IRQ handler) do not write.  So it is safe to wait for the TX
        // output to drain, and then to finish the drain.
        let mut status = G_UART_GATE.wait_for_no_active_users();
        if status.is_ok() {
            status = wait_for_tx_drained();
        }
        if let Err(err) = status {
            // We were killed or suspended.  Abandon the suspend operation and
            // release any writers which were refused entry while we were draining.
            ksync::lock!(let _guard = UartSuspendResumeLock::Get().lock());
            G_UART_GATE.cancel_drain();
            enable_rx_interrupt_if_pending_after_reopen();
            G_UART_RESUMED_EVENT.signal();
            return Err(err);
        }

        // Hold the lock while finishing the drain and preparing the hardware so
        // that a concurrent resume (which requires the gate to be suspended) cannot
        // touch the hardware until we are done.
        ksync::lock!(let _guard = UartSuspendResumeLock::Get().lock());
        let finished = {
            // Mask the IRQ as soon as the gate is suspended.  Do this with interrupts
            // disabled on this CPU so that a pending level-triggered IRQ (which the
            // handler will now refuse to service) cannot prevent us from masking it.
            let _irqd = InterruptDisableGuard::new();
            let finished = G_UART_GATE.try_finish_drain();
            if finished && let Some(uart_irq) = *G_UART_IRQ {
                let _ = mask_interrupt(uart_irq);
            }
            finished
        };
        if finished {
            // At this point we are the only one who may touch the hardware.  Give the
            // driver a chance to quiesce the hardware before it loses power and/or
            // clocks.  Driver suspend hooks are required to complete quickly (see
            // uart::DriverBase::PrepareForSuspend).
            // SAFETY: The gate is in `Suspended` phase and we hold `UartSuspendResumeLock`.
            unsafe { cpp_uart_prepare_for_suspend() };
            return Ok(());
        }
        // Something (e.g. the IRQ handler) entered the gate in the meantime.  It
        // will wake us when it exits.  Try again.
    }
}

/// Resume kernel access to the debug UART hardware after a previous successful
/// call to `platform_serial_suspend`.  The caller is responsible for ensuring the
/// hardware is powered and clocked before calling.
///
/// Must be called from thread context.  Does not block for long.
///
/// Returns:
///  - `Ok(())` on success.
///  - `Err(Status::NOT_SUPPORTED)` if there is no debug UART, or if the debuglog is
///    being bypassed.
///  - `Err(Status::BAD_STATE)` if the UART is not suspended (including while a suspend
///    operation is still in progress).
pub fn platform_serial_resume() -> Result<(), Status> {
    if !platform_serial_enabled() || dlog_bypass() {
        return Err(Status::NOT_SUPPORTED);
    }

    ksync::lock!(let _guard = UartSuspendResumeLock::Get().lock());
    if G_UART_GATE.phase() != UartGatePhase::Suspended {
        return Err(Status::BAD_STATE);
    }

    // Restore the hardware while the gate is still closed.  Note that the driver
    // may attempt to unmask the UART IRQ as part of this.  `KernelIrqProvider`
    // suppresses that while the gate is suspended so that the IRQ cannot fire
    // (on any CPU) while the IRQ handler would still refuse to service it.
    // SAFETY: The gate is in `Suspended` phase and we hold `UartSuspendResumeLock`.
    unsafe { cpp_uart_wakeup_from_suspend() };

    {
        // Reopen the gate and then unmask the IRQ.  Keep interrupts disabled on
        // this CPU so that the IRQ stays masked for as short a time as possible
        // after the gate has been reopened (writers may need it to make progress).
        let _irqd = InterruptDisableGuard::new();
        G_UART_GATE.resume();
        if let Some(uart_irq) = *G_UART_IRQ {
            let _ = unmask_interrupt(uart_irq);
        }
    }

    // If a reader drained the RX queue while we were suspended, re-enable the RX
    // interrupt on its behalf.
    enable_rx_interrupt_if_pending_after_reopen();

    // Finally, release any blocked writers.
    G_UART_RESUMED_EVENT.signal();

    Ok(())
}

/// Cause blocking writes which cannot proceed because the debug UART is
/// suspended to be discarded instead of blocking, and release any writers which
/// are currently waiting.  This is permanent, and is intended to be called as
/// the system halts (when the debuglog is shut down) so that a suspended UART
/// cannot prevent the system from halting.  Has no effect on writes made while
/// the UART is not suspended.  Once called, `platform_serial_suspend` fails with
/// `ZX_ERR_BAD_STATE`.
///
/// Must be called from thread context.  Does not block for long.  Safe to call
/// concurrently with `platform_serial_suspend` and `platform_serial_resume`.
pub fn platform_serial_discard_writes_while_suspended() {
    // UartSuspendResumeLock is only ever held briefly (never while waiting for a
    // suspend operation to drain), so acquiring it here cannot block the halt
    // path for long.
    ksync::lock!(let _guard = UartSuspendResumeLock::Get().lock());
    G_UART_DISCARD_WHILE_SUSPENDED.store(true, Ordering::Relaxed);
    // Wake any blocked writers so that they observe the flag.
    G_UART_RESUMED_EVENT.signal();
}

/// Block until the debug UART is not suspended, or until blocking writes are
/// being discarded (see `platform_serial_discard_writes_while_suspended`).
///
/// Note that by the time this returns, the UART may have been suspended again,
/// so callers should be prepared to retry.
///
/// If `interruptible` is `Interruptible::Yes`, the wait may be interrupted by the
/// calling thread being killed (returning `ZX_ERR_INTERNAL_INTR_KILLED`) or
/// suspended (returning `ZX_ERR_INTERNAL_INTR_RETRY`).  Otherwise, returns `Ok(())`.
///
/// Must be called from thread context.
pub fn platform_serial_wait_for_resume(interruptible: Interruptible) -> Result<(), Status> {
    if interruptible == Interruptible::No {
        return G_UART_RESUMED_EVENT.wait_infinite();
    }
    G_UART_RESUMED_EVENT.wait(&Deadline::infinite())
}

/// Returns true if kernel access to the debug UART has been suspended (or a
/// suspend operation is in progress) via `platform_serial_suspend`.
///
/// Safe to call from any context, including during a panic.
pub fn platform_serial_is_suspended() -> bool {
    G_UART_GATE.phase() != UartGatePhase::Running
}

/// Returns the boot time at which the debug UART was most recently suspended via
/// `platform_serial_suspend`.  The value is only meaningful when
/// `platform_serial_is_suspended` returns true.
///
/// Safe to call from any context, including during a panic.
pub fn platform_serial_suspended_at() -> InstantBoot {
    InstantBoot(G_UART_SUSPENDED_AT.load(Ordering::Relaxed))
}

/// Early boot handoff for the UART driver.
///
/// # Safety
///
/// Must be called once during single-threaded early boot with a valid pointer to `serial`.
pub unsafe fn uart_driver_handoff_early(serial: *const UartDriver) {
    // SAFETY: Called once during single-threaded early boot before any UART operations wait on
    // events or use the RX queue or poll timer.
    unsafe {
        let _ =
            Pin::static_ref(&G_UART_GATE.drained).init_pin(AutounsignalEvent::init_unsignaled());
        let _ = Pin::static_ref(&G_UART_RESUMED_EVENT).init_pin(Event::init_signaled());
        let _ = Pin::static_ref(&RX_QUEUE).init_pin(Cbuf::init());
        let _ = Pin::static_ref(&G_UART_POLL_TIMER)
            .init_pin(pin_init!(PollTimer { timer <- Timer::init(ZX_CLOCK_MONOTONIC) }));
        cpp_uart_set_driver(serial);
    }
    #[allow(clippy::collapsible_if)]
    if dprintf_enabled(INFO) {
        if platform_serial_enabled() {
            let mut buffer = [0u8; 128];
            // SAFETY: `buffer` is valid for writes of `buffer.len()` bytes.
            let len = unsafe { cpp_uart_unparse(buffer.as_mut_ptr().cast(), buffer.len()) };
            let unparsed = core::str::from_utf8(&buffer[..len]).unwrap_or("");
            dprintf!(INFO, "UART: Selected driver kernel.serial={}\n", unparsed);
        }
    }
}

/// Post-VM handoff for the UART driver.
pub fn uart_driver_handoff_post_vm(_serial: *const UartDriver) {}

/// Late handoff for the UART driver (allocates RX buffer and configures interrupts or polling).
///
/// # Safety
///
/// Must be called once, after `uart_driver_handoff_early` and after the heap is initialized, and
/// before anything reads `G_UART_IRQ`, the poll timer, or `RX_QUEUE`. The IRQ handler or poll
/// timer this function starts is the first such reader.
pub unsafe fn uart_driver_handoff_late(_serial: *const UartDriver) {
    // This buffer is needed even when serial is disabled, to prevent uninitialized
    // access to it.
    let layout = core::alloc::Layout::from_size_align(RX_QUEUE_SIZE, 1).unwrap();
    // SAFETY: `layout` has non-zero size (`RX_QUEUE_SIZE`).
    let buf = unsafe { kalloc::alloc(layout) }.expect("failed to allocate UART RX queue");
    // SAFETY: `uart_driver_handoff_late` runs once, so `RX_QUEUE` is initialized only once;
    // `RX_QUEUE_SIZE` is a non-zero power of two, and `buf` points to `RX_QUEUE_SIZE` writable
    // bytes from a fresh allocation that is never freed or otherwise accessed.
    unsafe { RX_QUEUE.initialize(RX_QUEUE_SIZE, buf.as_ptr()) };

    if !platform_serial_enabled() {
        // SAFETY: Called once, and nothing reads `G_UART_IRQ` until this function returns.
        unsafe { G_UART_IRQ.init(None) };
        return;
    }

    // Check for interrupt support or explicitly polling uart and reserve the IO regions, preventing
    // userspace from claiming them.
    let mut info = UartDriverInfo::default();
    // SAFETY: `info` is a valid mutable reference and `gUart` was initialized in early handoff.
    unsafe { cpp_uart_get_driver_info(&mut info) };
    let uart_irq = match info.kind {
        UartDriverKind::Mmio => {
            root_resource_filter_add_deny_region(info.io_base, info.io_size, ZX_RSRC_KIND_MMIO);
            info.has_irq.then_some(info.irq)
        }
        UartDriverKind::Pio => {
            root_resource_filter_add_deny_region(info.io_base, info.io_size, ZX_RSRC_KIND_IOPORT);
            info.has_irq.then_some(info.irq)
        }
        UartDriverKind::Null => {
            // No IRQ Handler for null driver.
            // SAFETY: Called once, and nothing reads `G_UART_IRQ` until this function returns.
            unsafe { G_UART_IRQ.init(None) };
            return;
        }
    };

    // Check for polling mode.
    let Some(uart_irq) = uart_irq.filter(|_| !BootOptions::get().debug_uart_poll) else {
        // Start the polling without performing any drain.
        // SAFETY: `G_UART_POLL_TIMER` was initialized during early boot.
        unsafe {
            G_UART_IRQ.init(None);
            uart_poll::<false>(
                ptr::from_ref(&G_UART_POLL_TIMER.timer).cast_mut(),
                current_mono_time().0,
                ptr::null_mut(),
            );
        }
        dprintf!(INFO, "UART: POLLING mode enabled.\n");
        return;
    };

    if info.has_simple_cfg {
        // Configure the interrupt if available.
        let irq_config = get_irq_config_from_flags(info.cfg_flags);
        if let Some(irq_config) = irq_config {
            let irq_config_result =
                configure_interrupt(uart_irq, irq_config.trigger, irq_config.polarity);
            debug_assert!(irq_config_result.is_ok());
        }
    }

    // Register IRQ Handler.
    extern "C" fn irq_handler_helper(_cookie: *mut c_void) {
        // Once the UART has been suspended, we must not touch the hardware.  The
        // suspend operation masks the IRQ shortly after suspending the gate, but
        // we may still observe a spurious (or already pending) IRQ in the
        // meantime.
        //
        // Note that the handler is still permitted to run while a suspend
        // operation is draining in-flight operations, as writers blocked waiting
        // on TX FIFO space depend on it.
        if !G_UART_GATE.try_enter_unless_suspended() {
            return;
        }
        // SAFETY: `gUart` is initialized and we have entered `G_UART_GATE`.
        unsafe { cpp_uart_interrupt() };
        // IRQ handlers may signal events (see `tx_irq_handler`).
        G_UART_GATE.exit();
    }
    // SAFETY: `irq_handler_helper` is valid for the lifetime of the system.
    let irq_register_result = unsafe {
        register_permanent_int_handler(
            uart_irq,
            InterruptHandler { cookie: ptr::null_mut(), r#fn: Some(irq_handler_helper) },
        )
    };
    debug_assert!(irq_register_result.is_ok());

    // Init UART Interrupts.
    // SAFETY: `gUart` is initialized and `uart_irq` has been registered.
    unsafe {
        cpp_uart_init_interrupt(uart_irq);
        G_UART_IRQ.init(Some(uart_irq));
    }

    dprintf!(INFO, "UART: IRQ driven RX: enabled\n");

    let is_tx_irq_enabled = !dlog_bypass();
    IS_TX_IRQ_ENABLED.store(is_tx_irq_enabled, Ordering::Relaxed);
    dprintf!(
        INFO,
        "UART: IRQ driven TX: {}\n",
        if is_tx_irq_enabled { "enabled" } else { "disabled" }
    );
}

/// Writes `s` to the debug serial port, blocking as needed until the output has
/// been handed off to the UART.  Must be called from a context which is allowed
/// to block.
///
/// Concurrent calls must be serialized by the caller.  The UART driver wakes
/// only one blocked writer per TX interrupt, so multiple concurrent blocking
/// writers can leave one of them blocked forever.  Most code should use
/// `dlog_serial_write`, which provides this serialization, instead.
///
/// If the debug serial port has been suspended (see `platform_serial_suspend`
/// and `zx_debug_suspend`), this function writes nothing and returns
/// `ZX_ERR_SHOULD_WAIT`.  The caller may then wait for the port to be resumed using
/// `platform_serial_wait_for_resume` and try again.  Callers should not hold
/// locks which other writers may need while waiting.  When the system is halting
/// (see `platform_serial_discard_writes_while_suspended`), output written while
/// suspended is silently discarded and `ZX_OK` is returned.
///
/// Returns `ZX_OK` if the output was written (or discarded).
pub fn platform_dputs_thread(s: &[u8]) -> zx_status_t {
    if !platform_serial_enabled() {
        return ZX_OK;
    }

    // If the UART has been suspended, tell the caller to wait for it to be
    // resumed (or, if the system is halting, discard the output).
    if !G_UART_GATE.try_enter() {
        return if G_UART_DISCARD_WHILE_SUSPENDED.load(Ordering::Relaxed) {
            ZX_OK
        } else {
            Status::SHOULD_WAIT.into_raw()
        };
    }

    // SAFETY: `s.as_ptr()` is valid for `s.len()` bytes, and we have entered `G_UART_GATE`.
    unsafe { cpp_uart_write(s.as_ptr().cast(), s.len(), true) };

    G_UART_GATE.exit();
    ZX_OK
}

/// Writes `s` to the debug serial port without blocking.  Safe to call from
/// contexts which may not block (e.g. with interrupts disabled).
///
/// If the debug serial port has been suspended, the output is silently dropped.
pub fn platform_dputs_irq(s: &[u8]) {
    if !platform_serial_enabled() {
        return;
    }

    // We cannot block here.  If the UART has been suspended, drop the output.
    if !try_enter_for_panic_io() {
        return;
    }

    // SAFETY: `s.as_ptr()` is valid for `s.len()` bytes, and we have entered `G_UART_GATE`.
    unsafe { cpp_uart_write(s.as_ptr().cast(), s.len(), false) };

    // We may be called with a ChainLockTransaction active, or while holding
    // spinlocks, so we must not signal.
    G_UART_GATE.exit_without_waking();
}

/// Returns 1 if a character was retrieved, 0 if no character was retrieved,
/// and negative on error.
///
/// If `wait` is true, blocks until a character is available.  While the debug
/// serial port is suspended, characters already received will still be
/// returned, but no new characters will arrive until the port is resumed, so a
/// waiting caller may block indefinitely.
pub fn platform_dgetc(c: &mut c_char, wait: bool) -> i32 {
    if !platform_serial_enabled() {
        return Status::NOT_SUPPORTED.into_raw();
    }

    let read = RX_QUEUE.read_char_with_context(wait);

    // 1 => Character read.
    if let Ok(read) = read {
        // This is safe because:
        //   * The RX IRQ handler is holding the UART lock while the queue is being inspected (Full)
        //     and the RX IRQ is being disabled.
        //   * The Read path, which is the only path which can transition the queue from full to not
        //     full, is not holding the UART lock while inspecting, but the operations is deferred
        //     and acquires the lock before enabling interrupts.
        //
        // As a consequence, the IRQ RX Interrupt cannot be enabled by this path, until the RX IRQ
        // Handler, has disabled it and released the lock. This means there is no possible
        // interleaving, where both paths observe full queue, and we enable the RX IRQ followed by
        // the IRQ RX Handler disabling them.
        if read.transitioned_from_full {
            // Record that the RX interrupt needs to be re-enabled.  If the UART is
            // suspended right now, we cannot touch the hardware, so the resume
            // operation will take care of it for us.  Otherwise, take care of it now.
            G_UART_RX_ENABLE_PENDING.store(true, Ordering::SeqCst);
            if G_UART_GATE.try_enter() {
                maybe_enable_rx_interrupt();
                G_UART_GATE.exit();
            }
        }
        *c = read.c as c_char;
        return 1;
    }

    // 0 => No character yet.
    let status = read.unwrap_err();
    if status == Status::SHOULD_WAIT {
        return 0;
    }

    // < 0 => Error.
    status.into_raw()
}

/// Polls for a character.  Returns 0 on success, negative value on error.
///
/// If the debug serial port is suspended, returns a negative value (no data).
pub fn platform_pgetc(c: &mut c_char) -> i32 {
    if !platform_serial_enabled() {
        return Status::NOT_SUPPORTED.into_raw();
    }

    // If the UART is suspended, report that there is no data available.
    if !try_enter_for_panic_io() {
        return -1;
    }

    let mut read = 0u8;
    // SAFETY: We have entered `G_UART_GATE`.
    let has_char = unsafe { cpp_uart_read(&mut read) };

    // Only used during a panic, so we must not signal.
    G_UART_GATE.exit_without_waking();

    if has_char {
        *c = read as c_char;
        return 0;
    }

    -1
}

/// Writes a character to the debug serial port.
///
/// If the debug serial port is suspended, the character is silently dropped.
pub fn platform_pputc(c: u8) {
    if !platform_serial_enabled() {
        return;
    }

    // We cannot block here.  If the UART has been suspended, drop the output.
    if !try_enter_for_panic_io() {
        return;
    }

    // SAFETY: We have entered `G_UART_GATE`.
    unsafe { cpp_uart_pputc(c as c_char) };

    // Only used during a panic, so we must not signal.
    G_UART_GATE.exit_without_waking();
}

/// Parses a duration of the form `<NUM><UNIT>`, where `UNIT` is one of `s`, `ms`, `us`, or
/// `ns` (e.g. `"500ms"`).  Returns `None` if `s` is not a valid duration.
#[cfg(console_enabled)]
fn parse_duration(s: &str) -> Option<DurationMono> {
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (digits, end) = s.split_at(split);
    if digits.is_empty() {
        return None;
    }
    let num: i64 = digits.parse().ok()?;
    if end == "s" {
        return Some(DurationMono(zx_sec(num)));
    }
    if end == "ms" {
        return Some(DurationMono(zx_msec(num)));
    }
    if end == "us" {
        return Some(DurationMono(zx_usec(num)));
    }
    if end == "ns" {
        return Some(DurationMono(zx_nsec(num)));
    }
    None
}

/// # Safety
///
/// When `argc > 0` and `argv` is non-null, `argv` must point to an array of at least `argc`
/// valid `CmdArgs` elements with valid NUL-terminated `arg_str` pointers.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_serial(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc < 1 || argv.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: `argc >= 1`, `argv` is non-null, and the caller guarantees `argv` is valid for
    // `argc` elements.
    let args = unsafe { slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprint!("usage:\n");
        kprint!(
            concat!(
                "{:s} suspend <NUM><s|ms|us|ns>  : suspend the debug UART, then resume it after ",
                "the given duration\n",
            ),
            args[0].as_str()
        );
        ZX_ERR_INTERNAL
    };

    if argc < 2 {
        return usage();
    }

    if args[1].as_str() == "suspend" {
        if argc != 3 {
            return usage();
        }
        let arg2 = args[2].as_str();
        let Some(duration) = parse_duration(arg2) else {
            kprint!("Invalid duration: {:s}\n", arg2);
            return usage();
        };

        kprint!("Suspending the debug UART for {} ns\n", duration.0);
        if let Err(status) = platform_serial_suspend() {
            kprint!("platform_serial_suspend failed: {}\n", status.into_raw());
            return status.into_raw();
        }
        let _ = thread::sleep_relative(duration);
        if let Err(status) = platform_serial_resume() {
            kprint!("platform_serial_resume failed: {}\n", status.into_raw());
            return status.into_raw();
        }
        kprint!("Resumed the debug UART\n");
        return ZX_OK;
    }

    kprint!("unknown command\n");
    usage()
}

#[cfg(console_enabled)]
static_command!(
    CMD_SERIAL,
    c"serial".as_ptr(),
    c"debug serial port commands".as_ptr(),
    cmd_serial,
    CMD_AVAIL_NORMAL
);

// FFI trampolines for C++ callers and `debug_ffi.cc` glue.

/// FFI callback for `UartSyncPolicy::Waiter::Wait` in `debug_ffi.cc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_uart_is_tx_irq_enabled() -> bool {
    IS_TX_IRQ_ENABLED.load(Ordering::Relaxed)
}

/// FFI callback for `KernelIrqProvider::SetInterruptsEnabled` in `debug_ffi.cc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_uart_irq_provider_set_interrupts_enabled(irq_num: u32, enabled: bool) {
    KernelIrqProvider::set_interrupts_enabled(irq_num, enabled);
}

/// FFI callback for `rx_irq_handler` in `debug_ffi.cc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_uart_rx_queue_full() -> bool {
    RX_QUEUE.full()
}

/// FFI callback for `rx_irq_handler` in `debug_ffi.cc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_uart_rx_queue_write_char(c: u8) {
    RX_QUEUE.write_char(c);
}

/// FFI trampoline for [`uart_driver_handoff_early`].
///
/// # Safety
///
/// Same as [`uart_driver_handoff_early`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_uart_driver_handoff_early(serial: *const UartDriver) {
    // SAFETY: Caller upholds the safety contract of `uart_driver_handoff_early`.
    unsafe { uart_driver_handoff_early(serial) };
}

/// FFI trampoline for [`uart_driver_handoff_post_vm`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_uart_driver_handoff_post_vm(serial: *const UartDriver) {
    uart_driver_handoff_post_vm(serial);
}

/// FFI trampoline for [`uart_driver_handoff_late`].
///
/// # Safety
///
/// Same as [`uart_driver_handoff_late`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_uart_driver_handoff_late(serial: *const UartDriver) {
    // SAFETY: Caller upholds the safety contract of `uart_driver_handoff_late`.
    unsafe { uart_driver_handoff_late(serial) };
}

/// FFI trampoline for [`platform_serial_enabled`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_enabled() -> bool {
    platform_serial_enabled()
}

/// FFI trampoline for [`platform_serial_suspend`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_suspend() -> zx_status_t {
    Status::result_into_raw(platform_serial_suspend())
}

/// FFI trampoline for [`platform_serial_resume`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_resume() -> zx_status_t {
    Status::result_into_raw(platform_serial_resume())
}

/// FFI trampoline for [`platform_serial_discard_writes_while_suspended`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_discard_writes_while_suspended() {
    platform_serial_discard_writes_while_suspended();
}

/// FFI trampoline for [`platform_serial_wait_for_resume`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_wait_for_resume(
    interruptible: Interruptible,
) -> zx_status_t {
    Status::result_into_raw(platform_serial_wait_for_resume(interruptible))
}

/// FFI trampoline for [`platform_serial_is_suspended`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_is_suspended() -> bool {
    platform_serial_is_suspended()
}

/// FFI trampoline for [`platform_serial_suspended_at`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_serial_suspended_at() -> zx_instant_boot_t {
    platform_serial_suspended_at().0
}

/// FFI trampoline for [`platform_dputs_thread`].
///
/// # Safety
///
/// When `len > 0` and `str_ptr` is non-null, `str_ptr` must be valid for reads of `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_dputs_thread(
    str_ptr: *const c_char,
    len: usize,
) -> zx_status_t {
    if str_ptr.is_null() {
        return ZX_OK;
    }
    // A zero-length write still goes through the gate, so it reports `ZX_ERR_SHOULD_WAIT` while
    // the UART is suspended.
    let s: &[u8] = if len == 0 {
        &[]
    } else {
        // SAFETY: `str_ptr` is non-null and the caller guarantees it is valid for `len` bytes.
        unsafe { slice::from_raw_parts(str_ptr.cast::<u8>(), len) }
    };
    platform_dputs_thread(s)
}

/// FFI trampoline for [`platform_dputs_irq`].
///
/// # Safety
///
/// When `len > 0` and `str_ptr` is non-null, `str_ptr` must be valid for reads of `len` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_dputs_irq(str_ptr: *const c_char, len: usize) {
    if len == 0 || str_ptr.is_null() {
        return;
    }
    // SAFETY: `str_ptr` is non-null and the caller guarantees it is valid for `len` bytes.
    platform_dputs_irq(unsafe { slice::from_raw_parts(str_ptr.cast::<u8>(), len) });
}

/// FFI trampoline for [`platform_dgetc`].
///
/// # Safety
///
/// `c` must be a valid, aligned pointer writable for 1 `c_char`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_dgetc(c: *mut c_char, wait: bool) -> c_int {
    let mut ch: c_char = 0;
    let ret = platform_dgetc(&mut ch, wait);
    if ret > 0 {
        // SAFETY: Caller guarantees `c` is a valid writable pointer.
        unsafe { *c = ch };
    }
    ret
}

/// FFI trampoline for [`platform_pgetc`].
///
/// # Safety
///
/// `c` must be a valid, aligned pointer writable for 1 `c_char`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_platform_pgetc(c: *mut c_char) -> c_int {
    let mut ch: c_char = 0;
    let ret = platform_pgetc(&mut ch);
    if ret == 0 {
        // SAFETY: Caller guarantees `c` is a valid writable pointer.
        unsafe { *c = ch };
    }
    ret
}

/// FFI trampoline for [`platform_pputc`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_pputc(c: c_char) {
    #[allow(clippy::unnecessary_cast)]
    platform_pputc(c as u8);
}

/// platform debug UART tests
#[cfg(ktest)]
#[unittest::suite(name = "platform_debug")]
mod tests {
    use crate::debuglog_rs::{dlog_serial_write, dlog_serial_write_interruptible, serial_write};
    use crate::kconcurrent::{IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned};
    use crate::kernel::scheduler_state::ThreadStateKind;
    use crate::kernel::thread::{DEFAULT_PRIORITY, ThreadPtr};
    use crate::platform_rs::timer::InstantMono;
    use core::cell::Cell;
    use core::sync::atomic::AtomicI32;
    use unittest::{assert_ok, expect_eq, expect_err, expect_false, expect_ok, expect_true};
    use zr::defer;
    use zx_types::ZX_ERR_INTERNAL_INTR_KILLED;

    /// Wait until `thread` has blocked.
    fn wait_until_blocked(thread: ThreadPtr) {
        let mut wait_duration = DurationMono(zx_usec(1));
        loop {
            {
                // SAFETY: `thread` is valid and has not been joined yet.
                let thread_ref = unsafe { thread.as_ref() };
                single_chain_lock_guard_owned!(
                    guard,
                    thread_ref,
                    IRQ_SAVE_OPTION,
                    clt_tag!("WaitUntilBlocked"),
                );
                if guard.proof().state() == ThreadStateKind::Blocked {
                    return;
                }
            }
            let _ = thread::sleep_relative(wait_duration);
            wait_duration = core::cmp::min(wait_duration * 2, DurationMono(zx_msec(10)));
        }
    }

    struct WriterState {
        done: AtomicBool,
        status: AtomicI32,
    }

    impl WriterState {
        const fn new() -> Self {
            Self {
                done: AtomicBool::new(false),
                status: AtomicI32::new(Status::INTERNAL.into_raw()),
            }
        }
    }

    const WRITER_MSG: &[u8] = b"blocking write issued while the debug UART was suspended\n";

    /// Note that we must use `dlog_serial_write` (or its interruptible variant, the
    /// same path used by `zx_debug_write`) rather than calling
    /// `platform_dputs_thread` directly.  The UART driver does not support multiple
    /// concurrent blocking writers (only one waiter is woken per TX interrupt), so
    /// all blocking writers must be serialized with the debuglog thread via the
    /// debuglog serial lock.
    extern "C" fn non_interruptible_writer(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `WriterState` that outlives this thread.
        let state = unsafe { &*arg.cast::<WriterState>() };
        dlog_serial_write(WRITER_MSG);
        state.status.store(ZX_OK, Ordering::Relaxed);
        state.done.store(true, Ordering::Relaxed);
        0
    }

    extern "C" fn interruptible_writer(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `WriterState` that outlives this thread.
        let state = unsafe { &*arg.cast::<WriterState>() };
        state.status.store(
            Status::result_into_raw(dlog_serial_write_interruptible(WRITER_MSG)),
            Ordering::Relaxed,
        );
        state.done.store(true, Ordering::Relaxed);
        0
    }

    /// Verifies debug UART suspend and resume behavior.
    // While the debug UART is suspended: (a) blocking writers block until it's resumed,
    // (b) non-blocking writers and polling reads do not hang, and (c) suspend/resume report
    // `BAD_STATE` when called in the wrong state.
    #[test]
    fn suspend_resume() {
        let status = platform_serial_suspend();
        if status == Err(Status::NOT_SUPPORTED) {
            kprint!("debug UART suspend not supported, skipping\n");
            return true;
        }
        assert_ok!(status);

        // No matter how the test exits, make sure the UART is resumed and that the
        // writer thread (if any) has terminated before `state` goes out of scope.
        let mut state = WriterState::new();
        let writer: Cell<Option<ThreadPtr>> = Cell::new(None);
        let _cleanup = defer(|| {
            if platform_serial_is_suspended() {
                let _ = platform_serial_resume();
            }
            if let Some(w) = writer.get() {
                // SAFETY: `w` has not been joined yet.
                let _ = unsafe { w.join(InstantMono::INFINITE) };
            }
        });

        expect_true!(platform_serial_is_suspended());
        expect_err!(platform_serial_suspend(), Status::BAD_STATE);

        // None of these may block or touch the hardware.
        serial_write(b"this non-blocking output should be dropped\n");
        platform_pputc(b'x');
        let mut c: c_char = 0;
        expect_eq!(-1, platform_pgetc(&mut c));

        // Start a thread to perform a blocking write.  It should not complete until
        // the UART has been resumed.
        // SAFETY: `non_interruptible_writer` and `&raw mut state` are valid, and `_cleanup` joins
        // the thread before `state` goes out of scope.
        let w = unsafe {
            thread::create_with_priority(
                c"uart suspend writer".as_ptr(),
                non_interruptible_writer,
                (&raw mut state).cast(),
                DEFAULT_PRIORITY,
            )
        };
        assert_ok!(w.map(|_| ()));
        let w = w.unwrap();
        writer.set(Some(w));
        // SAFETY: `w` was just created and has not been resumed or joined yet.
        unsafe { w.resume() };

        wait_until_blocked(w);
        expect_false!(state.done.load(Ordering::Relaxed));

        assert_ok!(platform_serial_resume());
        expect_false!(platform_serial_is_suspended());
        expect_err!(platform_serial_resume(), Status::BAD_STATE);

        // SAFETY: `w` has not been joined yet.
        assert_ok!(unsafe { w.join(InstantMono::INFINITE) }.map(|_| ()));
        writer.set(None);
        expect_true!(state.done.load(Ordering::Relaxed));
        expect_ok!(Status::ok(state.status.load(Ordering::Relaxed)));
    }

    /// Verifies killing an interruptible writer (`zx_debug_write`) blocked on UART suspend.
    #[test]
    fn suspend_interruptible_write_kill() {
        let status = platform_serial_suspend();
        if status == Err(Status::NOT_SUPPORTED) {
            kprint!("debug UART suspend not supported, skipping\n");
            return true;
        }
        assert_ok!(status);

        let mut state = WriterState::new();
        let writer: Cell<Option<ThreadPtr>> = Cell::new(None);
        let _cleanup = defer(|| {
            if let Some(w) = writer.get() {
                // SAFETY: `w` has not been joined yet.
                unsafe {
                    w.kill();
                    let _ = w.join(InstantMono::INFINITE);
                }
            }
            if platform_serial_is_suspended() {
                let _ = platform_serial_resume();
            }
        });

        // SAFETY: `interruptible_writer` and `&raw mut state` are valid, and `_cleanup` joins the
        // thread before `state` goes out of scope.
        let w = unsafe {
            thread::create_with_priority(
                c"uart suspend interruptible writer".as_ptr(),
                interruptible_writer,
                (&raw mut state).cast(),
                DEFAULT_PRIORITY,
            )
        };
        assert_ok!(w.map(|_| ()));
        let w = w.unwrap();
        writer.set(Some(w));
        // SAFETY: `w` was just created and has not been resumed or joined yet.
        unsafe { w.resume() };

        wait_until_blocked(w);
        expect_false!(state.done.load(Ordering::Relaxed));

        // Kill the writer while the UART is still suspended.  It should terminate
        // without writing anything.
        // SAFETY: `w` has not been joined yet.
        unsafe { w.kill() };
        // SAFETY: `w` has not been joined yet.
        assert_ok!(unsafe { w.join(InstantMono::INFINITE) }.map(|_| ()));
        writer.set(None);
        expect_true!(state.done.load(Ordering::Relaxed));
        expect_eq!(ZX_ERR_INTERNAL_INTR_KILLED, state.status.load(Ordering::Relaxed));
        expect_true!(platform_serial_is_suspended());

        assert_ok!(platform_serial_resume());
    }

    /// Verifies suspending an interruptible writer (`zx_debug_write`) blocked on UART suspend.
    // Interrupts the wait with `ZX_ERR_INTERNAL_INTR_RETRY` without writing anything.
    #[test]
    fn suspend_interruptible_write_thread_suspend() {
        let status = platform_serial_suspend();
        if status == Err(Status::NOT_SUPPORTED) {
            kprint!("debug UART suspend not supported, skipping\n");
            return true;
        }
        assert_ok!(status);

        let mut state = WriterState::new();
        let writer: Cell<Option<ThreadPtr>> = Cell::new(None);
        let _cleanup = defer(|| {
            if platform_serial_is_suspended() {
                let _ = platform_serial_resume();
            }
            if let Some(w) = writer.get() {
                // SAFETY: `w` has not been joined yet.
                let _ = unsafe { w.join(InstantMono::INFINITE) };
            }
        });

        // SAFETY: `interruptible_writer` and `&raw mut state` are valid, and `_cleanup` joins the
        // thread before `state` goes out of scope.
        let w = unsafe {
            thread::create_with_priority(
                c"uart suspend interruptible writer".as_ptr(),
                interruptible_writer,
                (&raw mut state).cast(),
                DEFAULT_PRIORITY,
            )
        };
        assert_ok!(w.map(|_| ()));
        let w = w.unwrap();
        writer.set(Some(w));
        // SAFETY: `w` was just created and has not been resumed or joined yet.
        unsafe { w.resume() };

        wait_until_blocked(w);
        expect_false!(state.done.load(Ordering::Relaxed));

        // Ask the writer to suspend while the UART is still suspended.  This should
        // interrupt the wait with ZX_ERR_INTERNAL_INTR_RETRY without writing
        // anything.
        // SAFETY: `w` has not been joined yet.
        assert_ok!(unsafe { w.suspend() });
        // SAFETY: `w` has not been joined yet.
        assert_ok!(unsafe { w.join(InstantMono::INFINITE) }.map(|_| ()));
        writer.set(None);
        expect_true!(state.done.load(Ordering::Relaxed));
        expect_eq!(Status::INTERRUPTED_RETRY.into_raw(), state.status.load(Ordering::Relaxed));
        expect_true!(platform_serial_is_suspended());

        assert_ok!(platform_serial_resume());
    }
}
