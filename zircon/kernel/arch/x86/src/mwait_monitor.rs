// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::alloc::Layout;
use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicU8, Ordering};

use zx_status::Status;

use super::MAX_CACHE_LINE;
use super::feature::{self, X86CpuidLeafNum};
use super::x86::x86_monitor;
use crate::kernel::types::cpu_num_t;

/// `MwaitMonitor` is used to perform `MWAIT`/`MONITOR` signaling (`class MwaitMonitor`).
///
/// This is a thread-safe type.
#[repr(transparent)]
#[derive(Default)]
pub struct MwaitMonitor {
    m: AtomicU8,
}

zr::static_assert!(size_of::<MwaitMonitor>() == 1);
zr::static_assert!(align_of::<MwaitMonitor>() == 1);

impl MwaitMonitor {
    /// Constructs a zero-initialized `MwaitMonitor`.
    pub const fn new() -> Self {
        Self { m: AtomicU8::new(0) }
    }

    /// Atomically replace this monitor's value `value` and return the old value.
    ///
    /// `Ordering::Relaxed` is sufficient because this coordinates only the single monitor
    /// state byte armed by `MONITOR` and re-checked against scheduler state/IPI fallback,
    /// rather than publishing
    /// non-atomic payload memory.
    #[inline]
    pub fn exchange(&self, value: u8) -> u8 {
        self.m.swap(value, Ordering::Relaxed)
    }

    /// Atomically write `value` to this monitor.
    #[inline]
    pub fn write(&self, value: u8) {
        self.m.store(value, Ordering::Relaxed)
    }

    /// Atomically read the monitor and return its value.
    #[inline]
    pub fn read(&self) -> u8 {
        self.m.load(Ordering::Relaxed)
    }

    /// Tells the hardware to begin watching this monitor. See `x86_enable_ints_and_mwait`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that CPL is 0 and the `MONITOR` instruction is supported on the
    /// current CPU.
    #[inline]
    pub unsafe fn prepare_for_wait(&self) {
        // SAFETY: Caller guarantees CPL 0 and `MONITOR` support, and `self.m.as_ptr()` points to a
        // valid readable byte.
        unsafe { x86_monitor(self.m.as_ptr().cast()) };
    }
}

/// An array of mwait/monitor objects with optimal alignment.
pub struct MwaitMonitorArray {
    monitors: *mut MwaitMonitor,
    alignment: usize,
    layout: Option<Layout>,
}

impl MwaitMonitorArray {
    /// Constructs an uninitialized `MwaitMonitorArray`.
    pub const fn new() -> Self {
        Self { monitors: core::ptr::null_mut(), alignment: 0, layout: None }
    }

    /// Initialize this instance with enough monitors for `cpu_count` CPUs.
    ///
    /// It is an error to successfully initialize a given instance more than once.
    pub fn init(&mut self, cpu_count: usize) -> Result<(), Status> {
        debug_assert!(self.monitors.is_null());
        if !self.monitors.is_null() {
            return Err(Status::BAD_STATE);
        }
        let largest_monitor_line_size = feature::x86_get_cpuid_leaf(X86CpuidLeafNum::Mon)
            .map(|leaf| (leaf.b & 0xffff) as usize)
            .unwrap_or(0);
        let alignment = core::cmp::max(largest_monitor_line_size, MAX_CACHE_LINE)
            .checked_next_power_of_two()
            .ok_or(Status::NO_MEMORY)?;
        let size = alignment.checked_mul(cpu_count).ok_or(Status::NO_MEMORY)?;
        // `kalloc::alloc_zeroed` requires a non-zero allocation size.
        if size == 0 {
            return Err(Status::NO_MEMORY);
        }
        let layout = Layout::from_size_align(size, alignment).map_err(|_| Status::NO_MEMORY)?;

        // SAFETY: `size > 0`, and `MwaitMonitor` is a `#[repr(transparent)]` `AtomicU8` for which
        // all-zero bytes is a valid initialized array.
        let monitors = unsafe { kalloc::alloc_zeroed(layout) }.ok_or(Status::NO_MEMORY)?;

        self.monitors = monitors.as_ptr().cast::<MwaitMonitor>();
        self.alignment = alignment;
        self.layout = Some(layout);
        Ok(())
    }

    /// Return a pointer to the monitor for `cpu_num`.
    ///
    /// Note, the monitors held by this array will be destroyed when this array is destroyed.
    ///
    /// # Safety
    ///
    /// The caller must ensure that [`init`](Self::init) has succeeded and `cpu_num < cpu_count`.
    #[inline]
    pub unsafe fn get_for_cpu(&self, cpu_num: cpu_num_t) -> *const MwaitMonitor {
        debug_assert!(!self.monitors.is_null());
        // SAFETY: Caller guarantees `cpu_num < cpu_count` after `init(cpu_count)`.
        unsafe { self.monitors.add((cpu_num as usize) * self.alignment) }
    }
}

impl Drop for MwaitMonitorArray {
    fn drop(&mut self) {
        if let Some(layout) = self.layout.take() {
            // SAFETY: `self.monitors` was allocated via `kalloc::alloc_zeroed` with `layout`.
            unsafe {
                kalloc::dealloc(self.monitors.cast::<u8>(), layout);
            }
            self.monitors = core::ptr::null_mut();
        }
    }
}
