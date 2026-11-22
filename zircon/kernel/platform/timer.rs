// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::ops;

use affine::{Ratio, Round};
use zx_types::{ZX_TIME_INFINITE, ZX_TIME_INFINITE_PAST};

#[inline]
pub const fn zx_nsec(n: i64) -> i64 {
    n
}

#[inline]
pub const fn zx_usec(n: i64) -> i64 {
    n.saturating_mul(1_000)
}

#[inline]
pub const fn zx_msec(n: i64) -> i64 {
    n.saturating_mul(1_000_000)
}

#[inline]
pub const fn zx_sec(n: i64) -> i64 {
    n.saturating_mul(1_000_000_000)
}

#[inline]
pub const fn zx_min(n: i64) -> i64 {
    n.saturating_mul(60_000_000_000)
}

#[inline]
pub const fn zx_hour(n: i64) -> i64 {
    n.saturating_mul(3_600_000_000_000)
}

/// Monotonic timeline instant in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantMono(pub i64);

impl core::ops::Add<i64> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: i64) -> Self {
        Self(self.0.saturating_add(rhs))
    }
}

impl core::ops::Add<DurationMono> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::Sub<DurationMono> for InstantMono {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl core::ops::Sub<InstantMono> for InstantMono {
    type Output = DurationMono;

    #[inline]
    fn sub(self, rhs: InstantMono) -> DurationMono {
        DurationMono(self.0.saturating_sub(rhs.0))
    }
}

impl InstantMono {
    pub const ZERO: Self = Self(0);
    pub const INFINITE: Self = Self(ZX_TIME_INFINITE);
    pub const INFINITE_PAST: Self = Self(ZX_TIME_INFINITE_PAST);

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn into_nanos(self) -> i64 {
        self.0
    }
}

/// Monotonic timeline instant in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantMonoTicks(pub i64);

/// Boot timeline instant in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantBoot(pub i64);

impl ops::Sub<InstantBoot> for InstantBoot {
    type Output = DurationBoot;

    #[inline]
    fn sub(self, rhs: InstantBoot) -> DurationBoot {
        DurationBoot(self.0.saturating_sub(rhs.0))
    }
}

/// Boot timeline instant in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantBootTicks(pub i64);

/// Monotonic timeline duration in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMono(pub i64);

impl core::ops::Add<DurationMono> for DurationMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl core::ops::Sub<DurationMono> for DurationMono {
    type Output = Self;

    #[inline]
    fn sub(self, rhs: DurationMono) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

impl core::ops::Mul<i64> for DurationMono {
    type Output = Self;

    #[inline]
    fn mul(self, rhs: i64) -> Self {
        Self(self.0.saturating_mul(rhs))
    }
}

impl DurationMono {
    pub const ZERO: Self = Self(0);
    pub const INFINITE: Self = Self(ZX_TIME_INFINITE);
    pub const INFINITE_PAST: Self = Self(ZX_TIME_INFINITE_PAST);

    /// Returns the number of nanoseconds contained by this `Duration`.
    pub const fn into_nanos(self) -> i64 {
        self.0
    }

    /// Returns the total number of whole microseconds contained by this `Duration`.
    pub const fn into_micros(self) -> i64 {
        self.0 / 1_000
    }

    /// Returns the total number of whole milliseconds contained by this `Duration`.
    pub const fn into_millis(self) -> i64 {
        self.into_micros() / 1_000
    }

    /// Returns the total number of whole seconds contained by this `Duration`.
    pub const fn into_seconds(self) -> i64 {
        self.into_millis() / 1_000
    }

    /// Returns the duration as a floating-point value in seconds.
    pub fn into_seconds_f64(self) -> f64 {
        self.into_nanos() as f64 / 1_000_000_000f64
    }

    /// Returns the total number of whole minutes contained by this `Duration`.
    pub const fn into_minutes(self) -> i64 {
        self.into_seconds() / 60
    }

    /// Returns the total number of whole hours contained by this `Duration`.
    pub const fn into_hours(self) -> i64 {
        self.into_minutes() / 60
    }

    pub const fn from_nanos(nanos: i64) -> Self {
        Self(nanos)
    }

    pub const fn from_micros(micros: i64) -> Self {
        Self(micros.saturating_mul(1_000))
    }

    pub const fn from_millis(millis: i64) -> Self {
        Self::from_micros(millis.saturating_mul(1_000))
    }

    pub const fn from_seconds(secs: i64) -> Self {
        Self::from_millis(secs.saturating_mul(1_000))
    }

    pub const fn from_minutes(min: i64) -> Self {
        Self::from_seconds(min.saturating_mul(60))
    }

    pub const fn from_hours(hours: i64) -> Self {
        Self::from_minutes(hours.saturating_mul(60))
    }
}

/// Monotonic timeline duration in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationMonoTicks(pub i64);

/// Boot timeline duration in nanoseconds.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationBoot(pub i64);

/// Boot timeline duration in ticks.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationBootTicks(pub i64);

// TODO(https://fxbug.dev/319935985): Not all locations are migrated to using specific timeline
// types, and some types (such as Deadline) are dangerously allowed to be instantiated on different
// timelines without capturing it in the type. For the moment we allow such behavior with these
// explicitly untyped types.

/// Instant in nanoseconds on an unknown timeline.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InstantUnknown(pub i64);

impl From<InstantMono> for InstantUnknown {
    #[inline]
    fn from(val: InstantMono) -> Self {
        Self(val.0)
    }
}

impl From<InstantBoot> for InstantUnknown {
    #[inline]
    fn from(val: InstantBoot) -> Self {
        Self(val.0)
    }
}

impl core::ops::Sub<InstantUnknown> for InstantUnknown {
    type Output = DurationUnknown;

    /// Saturating difference between two instants, matching `zx_time_sub_time`.
    #[inline]
    fn sub(self, rhs: InstantUnknown) -> DurationUnknown {
        DurationUnknown(self.0.saturating_sub(rhs.0))
    }
}

/// Duration in nanoseconds on an unknown timeline.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DurationUnknown(pub i64);

impl From<DurationMono> for DurationUnknown {
    #[inline]
    fn from(val: DurationMono) -> Self {
        Self(val.0)
    }
}

impl From<DurationBoot> for DurationUnknown {
    #[inline]
    fn from(val: DurationBoot) -> Self {
        Self(val.0)
    }
}

/// CurrentTicksObservation contains a coherent read of the current ticks on both the monotonic and
/// boot timeline.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct CurrentTicksObservation {
    pub mono_now: i64,
    pub boot_now: i64,
}

zr::static_assert_size_and_align!(CurrentTicksObservation, 16, 8);
zr::static_assert!(core::mem::offset_of!(CurrentTicksObservation, mono_now) == 0);
zr::static_assert!(core::mem::offset_of!(CurrentTicksObservation, boot_now) == 8);

/// Monotonic and boot time representing the same instant in both timelines.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct InstantMonoAndBootTime {
    pub mono_time: InstantMono,
    pub boot_time: InstantBoot,
}

zr::static_assert_size_and_align!(InstantMonoAndBootTime, 16, 8);
zr::static_assert!(core::mem::offset_of!(InstantMonoAndBootTime, mono_time) == 0);
zr::static_assert!(core::mem::offset_of!(InstantMonoAndBootTime, boot_time) == 8);

/// Monotonic time and boot ticks representing same instant in both timelines.
/// This is primarily useful for lining up samples of monotonic time with
/// corresponding trace events, which use boot ticks, so that computed and
/// visualized intervals are accurate.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct InstantMonoTimeAndBootTicks {
    pub mono_time: InstantMono,
    pub boot_ticks: InstantBootTicks,
}

zr::static_assert_size_and_align!(InstantMonoTimeAndBootTicks, 16, 8);
zr::static_assert!(core::mem::offset_of!(InstantMonoTimeAndBootTicks, mono_time) == 0);
zr::static_assert!(core::mem::offset_of!(InstantMonoTimeAndBootTicks, boot_ticks) == 8);

// TODO(https://fxbug.dev/540178114): Consolidate `BootInitCell` with `BootStatic`,
// `BootGlobal`, and `BootCell` into a shared `zr` type.
/// Static storage initialized to a default value at compile time, mutated at
/// most once during single-CPU boot in [`timer_set_ticks_to_time_ratio`] with
/// interrupts disabled before secondary CPUs are started, and read-only
/// thereafter.
///
/// Static default initialization is used instead of `LazyInit` because early
/// boot logging (`printf` / `dprintf!`) calls [`current_boot_time`] before
/// [`timer_set_ticks_to_time_ratio`] is invoked, requiring [`TICKS_TO_TIME`]
/// to start with a valid `1:1` ratio (`Ratio::const_default()`).
#[repr(transparent)]
struct BootInitCell<T>(UnsafeCell<T>);

impl<T> BootInitCell<T> {
    const fn new(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }

    #[inline]
    const fn as_ptr(&self) -> *mut T {
        self.0.get()
    }
}

// SAFETY: `BootInitCell<T>` is module-private and `as_ptr` only returns a raw pointer without
// dereferencing it, so safe code cannot perform unsynchronized reads or writes through
// `&BootInitCell<T>`. Every dereference in this module is either the single-CPU boot-time write
// (with interrupts disabled before secondary CPUs start) or a by-value read that cannot execute
// concurrently with that write. `T: Send + Sync` permits values written on the boot CPU to be
// moved/copied and shared across CPUs.
unsafe impl<T: Send + Sync> Sync for BootInitCell<T> {}

/// The ratio that defines the relationship between the system's tick counter
/// and the monotonic/boot nanosecond clocks.
static TICKS_TO_TIME: BootInitCell<Ratio> = BootInitCell::new(Ratio::const_default());

/// High-precision timer ticks per second.
static TICKS_PER_SECOND: BootInitCell<u64> = BootInitCell::new(0);

/// The `GetTicksSyncFlag` bits of `<platform/timer.h>`: which of the surrounding
/// memory accesses a raw ticks observation must be ordered against (`kNone`).
pub const GET_TICKS_SYNC_FLAG_NONE: u8 = 0;
/// `GetTicksSyncFlag::kAfterPreviousLoads`: order tick observation after previous loads.
pub const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS: u8 = 1 << 0;
/// `GetTicksSyncFlag::kAfterPreviousStores`: order tick observation after previous stores.
pub const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES: u8 = 1 << 1;
/// `GetTicksSyncFlag::kBeforeSubsequentLoads`: order tick observation before subsequent loads.
pub const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS: u8 = 1 << 2;
/// `GetTicksSyncFlag::kBeforeSubsequentStores`: order tick observation before subsequent stores.
pub const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES: u8 = 1 << 3;

unsafe extern "C" {
    fn cpp_timer_current_mono_ticks() -> InstantMonoTicks;
    fn cpp_timer_current_mono_ticks_synchronized_after_and_before_stores() -> InstantMonoTicks;
    fn cpp_timer_current_boot_ticks() -> InstantBootTicks;
    fn cpp_timer_get_mono_ticks_offset() -> i64;
    fn cpp_timer_get_boot_ticks_offset() -> i64;
    fn cpp_timer_current_mono_and_boot_ticks() -> CurrentTicksObservation;
    fn cpp_timer_set_initial_ticks(ticks: u64);
    fn cpp_timer_convert_mono_to_raw_ticks(
        mono_ticks: InstantMonoTicks,
        out_raw_ticks: *mut i64,
    ) -> bool;
    /// Pauses the monotonic clock during system suspend entry.
    fn cpp_timer_pause_monotonic();
    /// Resumes the monotonic clock during system suspend exit.
    fn cpp_timer_unpause_monotonic();
    /// Returns `true` if the monotonic clock is currently paused.
    fn cpp_timer_is_monotonic_paused() -> bool;
}

/// Pauses the monotonic clock during system suspend entry.
///
/// Safe to call concurrently with `timer_current_mono_ticks_synchronized`, which will retry its
/// read of the current ticks if `mono_ticks_modifier` is `ZX_TIME_INFINITE_PAST` (set before the
/// update) or if it notices that the value changed during its read.
///
/// # Safety
///
/// The monotonic clock must not currently be paused (`!timer_is_monotonic_paused()`), and there
/// must be no concurrent calls to [`timer_pause_monotonic`] or [`timer_unpause_monotonic`] across
/// CPUs.
#[inline]
pub unsafe fn timer_pause_monotonic() {
    // SAFETY: Caller guarantees the monotonic clock is not currently paused and no concurrent
    // `timer_pause_monotonic` or `timer_unpause_monotonic` calls occur.
    unsafe { cpp_timer_pause_monotonic() }
}

/// Resumes the monotonic clock during system suspend exit.
///
/// Safe to call concurrently with `timer_current_mono_ticks_synchronized`, which retries its read
/// if it notices that `mono_ticks_modifier` changed during its read.
///
/// # Safety
///
/// The monotonic clock must currently be paused (`timer_is_monotonic_paused()`), raw ticks must
/// not have regressed since the clock was paused, and there must be no concurrent calls to
/// [`timer_pause_monotonic`] or [`timer_unpause_monotonic`] across CPUs.
#[inline]
pub unsafe fn timer_unpause_monotonic() {
    // SAFETY: Caller guarantees the monotonic clock is currently paused, raw ticks have not
    // regressed since pause, and no concurrent `timer_pause_monotonic` or
    // `timer_unpause_monotonic` calls occur.
    unsafe { cpp_timer_unpause_monotonic() }
}

/// Returns `true` if the monotonic clock is currently paused.
///
/// Performs a `Relaxed` atomic read of `mono_ticks_modifier` intended to be used in diagnostic
/// assertions; does not synchronize-with anything (establishes no happens-before ordering).
#[inline]
pub fn timer_is_monotonic_paused() -> bool {
    // SAFETY: Performs a relaxed atomic load of `mono_ticks_modifier` with no safety
    // preconditions.
    unsafe { cpp_timer_is_monotonic_paused() }
}

pub use crate::kernel::timer::timer_tick;

/// Returns the current monotonic time in ticks.
#[inline]
pub fn timer_current_mono_ticks() -> InstantMonoTicks {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform timer
    // ticks.
    unsafe { cpp_timer_current_mono_ticks() }
}

/// Access the platform specific monotonic ticks counter, carrying the platform-required barriers
/// to ensure that the timer cannot be re-ordered conceptually with operations specified via the
/// `GET_TICKS_SYNC_FLAG_*` constants.
#[inline]
pub fn timer_current_mono_ticks_synchronized<const FLAGS: u8>() -> InstantMonoTicks {
    const {
        assert!(
            FLAGS
                == (GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
                    | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS)
                || FLAGS
                    == (GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
                        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES)
        );
    }
    if FLAGS
        == (GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS)
    {
        // SAFETY: Calling this FFI function has no preconditions and safely returns the platform
        // timer ticks synchronized with loads.
        unsafe { cpp_timer_current_mono_ticks() }
    } else {
        // SAFETY: Calling this FFI function has no preconditions and safely returns the platform
        // timer ticks synchronized with stores.
        unsafe { cpp_timer_current_mono_ticks_synchronized_after_and_before_stores() }
    }
}

/// High-precision timer ticks per second.
#[inline]
pub fn ticks_per_second() -> i64 {
    // SAFETY: `TICKS_PER_SECOND` is only written at most once in `timer_set_ticks_to_time_ratio`
    // while running on the boot CPU with interrupts disabled and before secondary CPUs start, so
    // no interrupt handler or other CPU can run this read concurrently with that write.
    unsafe { *TICKS_PER_SECOND.as_ptr() as i64 }
}

/// Returns the current boot time in ticks.
#[inline]
pub fn timer_current_boot_ticks() -> InstantBootTicks {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the platform timer
    // ticks.
    unsafe { cpp_timer_current_boot_ticks() }
}

/// The current monotonic time in ticks.
#[inline]
pub fn current_mono_ticks() -> InstantMonoTicks {
    timer_current_mono_ticks()
}

/// The current boot time in ticks.
#[inline]
pub fn current_boot_ticks() -> InstantBootTicks {
    timer_current_boot_ticks()
}

/// Current monotonic time in nanoseconds.
#[inline]
pub fn current_mono_time() -> InstantMono {
    let ticks = current_mono_ticks().0;
    InstantMono(timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(ticks))
}

/// Current boot time in nanoseconds.
#[inline]
pub fn current_boot_time() -> InstantBoot {
    let ticks = current_boot_ticks().0;
    InstantBoot(timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(ticks))
}

/// Current monotonic and boot time in nanoseconds, derived from a coherent
/// observation of the offsets and raw ticks.
#[inline]
pub fn current_mono_and_boot_time() -> InstantMonoAndBootTime {
    let observation = timer_current_mono_and_boot_ticks();
    let ticks_to_time = timer_get_ticks_to_time_ratio();
    InstantMonoAndBootTime {
        mono_time: InstantMono(ticks_to_time.scale::<{ Round::DOWN }>(observation.mono_now)),
        boot_time: InstantBoot(ticks_to_time.scale::<{ Round::DOWN }>(observation.boot_now)),
    }
}

/// Current monotonic time in nanoseconds and boot ticks, derived from a coherent
/// observation of the offsets and raw ticks.
#[inline]
pub fn current_mono_time_and_boot_ticks() -> InstantMonoTimeAndBootTicks {
    let observation = timer_current_mono_and_boot_ticks();
    InstantMonoTimeAndBootTicks {
        mono_time: InstantMono(
            timer_get_ticks_to_time_ratio().scale::<{ Round::DOWN }>(observation.mono_now),
        ),
        boot_ticks: InstantBootTicks(observation.boot_now),
    }
}

/// Setter/getter pair for the ratio which defines the relationship between the
/// system's tick counter, and the current_time/clock_monotonic clock.  This gets
/// set once by architecture specific platform code, after an appropriate ticks
/// source has been selected and characterized.
#[inline]
pub fn timer_get_ticks_to_time_ratio() -> Ratio {
    // SAFETY: `TICKS_TO_TIME` is only written at most once in `timer_set_ticks_to_time_ratio`
    // while running on the boot CPU with interrupts disabled and before secondary CPUs start, so
    // no interrupt handler or other CPU can run this read concurrently with that write.
    unsafe { *TICKS_TO_TIME.as_ptr() }
}

/// The offset added to raw platform ticks to put them on the monotonic ticks timeline.
#[inline]
pub fn timer_get_mono_ticks_offset() -> i64 {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the offset.
    unsafe { cpp_timer_get_mono_ticks_offset() }
}

/// Access the offset from the raw ticks timeline to the boot ticks timeline.
#[inline]
pub fn timer_get_boot_ticks_offset() -> i64 {
    // SAFETY: Calling this FFI function has no preconditions; it just loads an atomic.
    unsafe { cpp_timer_get_boot_ticks_offset() }
}

/// Performs a coherent read of both the mono and boot timelines and returns a tuple with the
/// current value of both timelines.
#[inline]
pub fn timer_current_mono_and_boot_ticks() -> CurrentTicksObservation {
    // SAFETY: Calling this FFI function has no preconditions and safely returns the current ticks
    // observation.
    unsafe { cpp_timer_current_mono_and_boot_ticks() }
}

/// Setter/getter pair for the ratio which defines the relationship between the
/// system's tick counter, and the current_time/clock_monotonic clock.  This gets
/// set once by architecture specific platform code, after an appropriate ticks
/// source has been selected and characterized.
///
/// # Safety
///
/// Must be called at most once, on the boot CPU with interrupts disabled,
/// before secondary CPUs are started. No other CPU or interrupt handler may
/// access the ticks-to-time ratio or `ticks_per_second` concurrently with this
/// call.
#[inline]
pub unsafe fn timer_set_ticks_to_time_ratio(ticks_to_time: Ratio) {
    // ASSERT that we are not calling this function twice.  Once set, this ratio
    // may not change.
    // SAFETY: The caller upholds the at-most-once, single-CPU contract with interrupts disabled
    // before secondary CPUs are started, and ensures no concurrent reader (including interrupt
    // handlers) accesses `TICKS_TO_TIME` or `TICKS_PER_SECOND` during this call.
    unsafe {
        debug_assert!(*TICKS_PER_SECOND.as_ptr() == 0);
        debug_assert!(ticks_to_time.numerator() != 0);
        debug_assert!(ticks_to_time.denominator() != 0);
        *TICKS_TO_TIME.as_ptr() = ticks_to_time;
        *TICKS_PER_SECOND.as_ptr() =
            (*TICKS_TO_TIME.as_ptr()).inverse().scale::<{ Round::DOWN }>(zx_sec(1)) as u64;
    }
}

/// C++ FFI entry point for [`timer_get_ticks_to_time_ratio`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_timer_get_ticks_to_time_ratio() -> *const Ratio {
    TICKS_TO_TIME.as_ptr().cast_const()
}

/// C++ FFI entry point for [`current_mono_time`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_current_mono_time() -> InstantMono {
    current_mono_time()
}

/// C++ FFI entry point for [`current_boot_time`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_current_boot_time() -> InstantBoot {
    current_boot_time()
}

// TODO(https://fxbug.dev/540178114): Switch `kernel/scheduler.rs` to call
// `current_mono_time_and_boot_ticks()` directly and remove this export, its C++ wrapper,
// `Scheduler::CurrentMonoTimeAndBootTicks`, and `cpp_current_mono_time_and_boot_ticks`.
/// C++ FFI entry point for [`current_mono_time_and_boot_ticks`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_current_mono_time_and_boot_ticks() -> InstantMonoTimeAndBootTicks {
    current_mono_time_and_boot_ticks()
}

/// C++ FFI entry point for [`ticks_per_second`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/platform/timer.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ticks_per_second() -> zx_types::zx_ticks_t {
    ticks_per_second()
}

/// Sets the raw ticks value at which the boot and monotonic timelines start.
///
/// # Safety
///
/// Must be called on the boot CPU with interrupts disabled, before secondary
/// CPUs are started.
#[inline]
pub unsafe fn timer_set_initial_ticks(ticks: u64) {
    // SAFETY: the caller upholds the single-CPU contract above.
    unsafe { cpp_timer_set_initial_ticks(ticks) }
}

/// Converts a ticks value on the monotonic timeline to a raw hardware ticks value.
///
/// Returns the raw ticks value if the monotonic clock is not paused, and `None` if it is.
#[inline]
pub fn timer_convert_mono_to_raw_ticks(mono_ticks: InstantMonoTicks) -> Option<i64> {
    let mut raw_ticks: i64 = 0;
    // SAFETY: `raw_ticks` is a valid, writable `i64`, and the function otherwise only reads the
    // atomic monotonic ticks modifier.
    let has_value = unsafe { cpp_timer_convert_mono_to_raw_ticks(mono_ticks, &raw mut raw_ticks) };
    has_value.then_some(raw_ticks)
}

/// Sets the calling CPU's hardware oneshot timer, which is responsible for calling `timer_tick()`
/// when it fires, to the raw ticks value `deadline`.
#[inline]
pub fn platform_set_oneshot_timer(deadline: i64) -> Result<(), zx_status::Status> {
    #[cfg(target_arch = "aarch64")]
    {
        crate::arm_generic_timer::platform_set_oneshot_timer(deadline)
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::timer::platform_set_oneshot_timer(deadline)
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::timer::platform_set_oneshot_timer(deadline)
    }
}

/// Clears the calling CPU's hardware oneshot timer.
#[inline]
pub fn platform_stop_timer() {
    #[cfg(target_arch = "aarch64")]
    {
        crate::arm_generic_timer::platform_stop_timer();
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::timer::platform_stop_timer();
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::timer::platform_stop_timer();
    }
}

/// Shuts down the calling CPU's platform timer.
#[inline]
pub fn platform_shutdown_timer() {
    #[cfg(target_arch = "aarch64")]
    {
        crate::arm_generic_timer::platform_shutdown_timer();
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::timer::platform_shutdown_timer();
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::timer::platform_shutdown_timer();
    }
}

/// Convert a sample taken early on to a proper `zx_ticks_t`, if possible.
/// This returns 0 if early samples are not convertible.
#[inline]
pub fn platform_convert_early_ticks(sample: libarch::EarlyTicks) -> zx_types::zx_ticks_t {
    #[cfg(target_arch = "aarch64")]
    {
        crate::arm_generic_timer::platform_convert_early_ticks(sample)
    }
    #[cfg(target_arch = "riscv64")]
    {
        crate::generic_riscv64::timer::platform_convert_early_ticks(sample)
    }
    #[cfg(target_arch = "x86_64")]
    {
        crate::platform_pc::timer::platform_convert_early_ticks(sample)
    }
}

/// Platform timer tests.
#[cfg(ktest)]
#[unittest::suite(name = "platform_timer")]
mod tests {
    use super::{
        DurationMono, InstantBoot, InstantMono, Round, TICKS_TO_TIME, current_boot_ticks,
        current_boot_time, current_mono_and_boot_time, current_mono_ticks, current_mono_time,
        current_mono_time_and_boot_ticks, rust_current_boot_time, rust_current_mono_time,
        rust_current_mono_time_and_boot_ticks, rust_ticks_per_second,
        rust_timer_get_ticks_to_time_ratio, ticks_per_second, timer_current_mono_and_boot_ticks,
        timer_get_ticks_to_time_ratio, zx_sec,
    };
    use unittest::{assert_eq, assert_ge, assert_gt, assert_le, assert_true};

    /// Test time ordering and equality.
    #[test]
    fn test_time_ordering_and_equality() {
        assert_true!(DurationMono(10) < DurationMono(20));
        assert_true!(DurationMono(15) == DurationMono(15));
        assert_true!(InstantMono(100) < InstantMono(200));
        assert_true!(InstantMono(100) + 50 == InstantMono(150));
        assert_true!(InstantMono(100) + DurationMono(50) == InstantMono(150));
    }

    /// Test saturating addition overflow on DurationMono.
    #[test]
    fn test_duration_mono_add_overflow() {
        assert_true!(DurationMono::INFINITE + DurationMono(1) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 10) + DurationMono(20) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 1) + DurationMono(1) == DurationMono::INFINITE);

        assert_true!(DurationMono::INFINITE_PAST + DurationMono(-1) == DurationMono::INFINITE_PAST);
        assert_true!(
            DurationMono(i64::MIN + 10) + DurationMono(-20) == DurationMono::INFINITE_PAST
        );
        assert_true!(DurationMono(i64::MIN + 1) + DurationMono(-1) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating subtraction overflow on DurationMono.
    #[test]
    fn test_duration_mono_sub_overflow() {
        assert_true!(DurationMono::INFINITE - DurationMono(-1) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 10) - DurationMono(-20) == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX - 1) - DurationMono(-1) == DurationMono::INFINITE);

        assert_true!(DurationMono::INFINITE_PAST - DurationMono(1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN + 10) - DurationMono(20) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN + 1) - DurationMono(1) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating multiplication overflow on DurationMono.
    #[test]
    fn test_duration_mono_mul_overflow() {
        // Positive * positive overflow
        assert_true!(DurationMono::INFINITE * 2 == DurationMono::INFINITE);
        assert_true!(DurationMono(i64::MAX / 2 + 1) * 2 == DurationMono::INFINITE);

        // Negative * negative overflow
        assert_true!(DurationMono::INFINITE_PAST * -1 == DurationMono::INFINITE);
        assert_true!(DurationMono::INFINITE_PAST * -2 == DurationMono::INFINITE);

        // Positive * negative underflow
        assert_true!(DurationMono::INFINITE * -2 == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MAX / 2 + 2) * -2 == DurationMono::INFINITE_PAST);

        // Negative * positive underflow
        assert_true!(DurationMono::INFINITE_PAST * 2 == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono(i64::MIN / 2 - 1) * 2 == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from microseconds.
    #[test]
    fn test_duration_mono_from_micros_overflow() {
        let max_valid = i64::MAX / 1_000;
        assert_true!(DurationMono::from_micros(max_valid) == DurationMono(max_valid * 1_000));
        assert_true!(DurationMono::from_micros(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_micros(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000;
        assert_true!(DurationMono::from_micros(min_valid) == DurationMono(min_valid * 1_000));
        assert_true!(DurationMono::from_micros(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_micros(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from milliseconds.
    #[test]
    fn test_duration_mono_from_millis_overflow() {
        let max_valid = i64::MAX / 1_000_000;
        assert_true!(DurationMono::from_millis(max_valid) == DurationMono(max_valid * 1_000_000));
        assert_true!(DurationMono::from_millis(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_millis(i64::MAX / 1_000 + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_millis(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000_000;
        assert_true!(DurationMono::from_millis(min_valid) == DurationMono(min_valid * 1_000_000));
        assert_true!(DurationMono::from_millis(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(
            DurationMono::from_millis(i64::MIN / 1_000 - 1) == DurationMono::INFINITE_PAST
        );
        assert_true!(DurationMono::from_millis(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from seconds.
    #[test]
    fn test_duration_mono_from_seconds_overflow() {
        let max_valid = i64::MAX / 1_000_000_000;
        assert_true!(
            DurationMono::from_seconds(max_valid) == DurationMono(max_valid * 1_000_000_000)
        );
        assert_true!(DurationMono::from_seconds(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_seconds(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / 1_000_000_000;
        assert_true!(
            DurationMono::from_seconds(min_valid) == DurationMono(min_valid * 1_000_000_000)
        );
        assert_true!(DurationMono::from_seconds(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_seconds(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from minutes.
    #[test]
    fn test_duration_mono_from_minutes_overflow() {
        const NANOS_PER_MIN: i64 = 60_000_000_000;
        let max_valid = i64::MAX / NANOS_PER_MIN;
        assert_true!(
            DurationMono::from_minutes(max_valid) == DurationMono(max_valid * NANOS_PER_MIN)
        );
        assert_true!(DurationMono::from_minutes(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_minutes(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / NANOS_PER_MIN;
        assert_true!(
            DurationMono::from_minutes(min_valid) == DurationMono(min_valid * NANOS_PER_MIN)
        );
        assert_true!(DurationMono::from_minutes(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_minutes(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating conversion from hours.
    #[test]
    fn test_duration_mono_from_hours_overflow() {
        const NANOS_PER_HOUR: i64 = 3_600_000_000_000;
        let max_valid = i64::MAX / NANOS_PER_HOUR;
        assert_true!(
            DurationMono::from_hours(max_valid) == DurationMono(max_valid * NANOS_PER_HOUR)
        );
        assert_true!(DurationMono::from_hours(max_valid + 1) == DurationMono::INFINITE);
        assert_true!(DurationMono::from_hours(i64::MAX) == DurationMono::INFINITE);

        let min_valid = i64::MIN / NANOS_PER_HOUR;
        assert_true!(
            DurationMono::from_hours(min_valid) == DurationMono(min_valid * NANOS_PER_HOUR)
        );
        assert_true!(DurationMono::from_hours(min_valid - 1) == DurationMono::INFINITE_PAST);
        assert_true!(DurationMono::from_hours(i64::MIN) == DurationMono::INFINITE_PAST);
    }

    /// Test saturating arithmetic on InstantMono with DurationMono and InstantMono.
    #[test]
    fn test_instant_mono_overflow() {
        // InstantMono + i64
        assert_true!(InstantMono::INFINITE + 1 == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST + (-1) == InstantMono::INFINITE_PAST);

        // InstantMono + DurationMono
        assert_true!(InstantMono::INFINITE + DurationMono(1) == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST + DurationMono(-1) == InstantMono::INFINITE_PAST);

        // InstantMono - DurationMono
        assert_true!(InstantMono::INFINITE - DurationMono(-1) == InstantMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST - DurationMono(1) == InstantMono::INFINITE_PAST);

        // InstantMono - InstantMono -> DurationMono
        assert_true!(InstantMono::INFINITE - InstantMono(-1) == DurationMono::INFINITE);
        assert_true!(InstantMono::INFINITE_PAST - InstantMono(1) == DurationMono::INFINITE_PAST);
    }

    /// Test ticks-to-time ratio, `ticks_per_second`, current time conversions, and FFI shims.
    #[test]
    fn test_ticks_to_time_and_current_time() {
        let ratio = timer_get_ticks_to_time_ratio();
        assert_gt!(ratio.numerator(), 0);
        assert_gt!(ratio.denominator(), 0);

        let ffi_ratio_ptr = rust_timer_get_ticks_to_time_ratio();
        assert_eq!(ffi_ratio_ptr, TICKS_TO_TIME.as_ptr().cast_const());
        // SAFETY: `rust_timer_get_ticks_to_time_ratio` returns a pointer to `TICKS_TO_TIME`, which
        // is immutable once initialized before secondary CPUs start and cannot be written
        // concurrently during unit tests.
        let ffi_ratio = unsafe { *ffi_ratio_ptr };
        assert_eq!(ffi_ratio.numerator(), ratio.numerator());
        assert_eq!(ffi_ratio.denominator(), ratio.denominator());

        let tps = ticks_per_second();
        assert_gt!(tps, 0);
        assert_eq!(tps, rust_ticks_per_second());
        assert_eq!(tps, ratio.inverse().scale::<{ Round::DOWN }>(zx_sec(1)));

        let obs_start = timer_current_mono_and_boot_ticks();
        let mono_ticks_1 = current_mono_ticks();
        let boot_ticks_1 = current_boot_ticks();
        let mono_time_1 = current_mono_time();
        let ffi_mono_time = rust_current_mono_time();
        let boot_time_1 = current_boot_time();
        let ffi_boot_time = rust_current_boot_time();
        let both_time = current_mono_and_boot_time();
        let mixed = current_mono_time_and_boot_ticks();
        let ffi_mixed = rust_current_mono_time_and_boot_ticks();
        let obs_end = timer_current_mono_and_boot_ticks();

        let expected_start_mono = InstantMono(ratio.scale::<{ Round::DOWN }>(obs_start.mono_now));
        let expected_end_mono = InstantMono(ratio.scale::<{ Round::DOWN }>(obs_end.mono_now));
        let expected_start_boot = InstantBoot(ratio.scale::<{ Round::DOWN }>(obs_start.boot_now));
        let expected_end_boot = InstantBoot(ratio.scale::<{ Round::DOWN }>(obs_end.boot_now));

        assert_ge!(mono_ticks_1.0, obs_start.mono_now);
        assert_le!(mono_ticks_1.0, obs_end.mono_now);
        assert_ge!(boot_ticks_1.0, obs_start.boot_now);
        assert_le!(boot_ticks_1.0, obs_end.boot_now);

        assert_ge!(mono_time_1.0, expected_start_mono.0);
        assert_le!(mono_time_1.0, ffi_mono_time.0);
        assert_le!(ffi_mono_time.0, both_time.mono_time.0);
        assert_le!(both_time.mono_time.0, mixed.mono_time.0);
        assert_le!(mixed.mono_time.0, ffi_mixed.mono_time.0);
        assert_le!(ffi_mixed.mono_time.0, expected_end_mono.0);

        assert_ge!(boot_time_1.0, expected_start_boot.0);
        assert_le!(boot_time_1.0, ffi_boot_time.0);
        assert_le!(ffi_boot_time.0, both_time.boot_time.0);
        assert_le!(both_time.boot_time.0, ratio.scale::<{ Round::DOWN }>(mixed.boot_ticks.0));
        assert_le!(mixed.boot_ticks.0, ffi_mixed.boot_ticks.0);
        assert_le!(ffi_mixed.boot_ticks.0, obs_end.boot_now);
        assert_le!(ratio.scale::<{ Round::DOWN }>(ffi_mixed.boot_ticks.0), expected_end_boot.0);
    }
}
