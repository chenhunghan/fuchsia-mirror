// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// A note on the distribution of code between us and the userspace driver:
// The default location for code is the userspace driver. Reasons for
// putting code here are: implementation requirement (need ring zero to write
// MSRs), stability, and performance. The device driver should do as much
// error checking as possible before calling us.
// Note that we do a lot of verification of the input configuration:
// We don't want to be compromised if the userspace driver gets compromised.

// A note on terminology: "events" vs "counters": A "counter" is an
// "event", but some events are not counters. Internally, we use the
// term "counter" when we know the event is a counter.

use core::ffi::c_void;
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use fbl::RefPtr;
pub use perfmon_bindings::{
    BufferHeader, CountRecord, EventId as PmuEventId, PcRecord, RecordHeader, RecordType,
    TickRecord, TimeRecord, ValueRecord, kEventIdNone, kRecordTypeCount, kRecordTypeInvalid,
    kRecordTypeLastBranch, kRecordTypePc, kRecordTypeTick, kRecordTypeTime, kRecordTypeValue,
};
use zx_types::zx_instant_mono_ticks_t;

use crate::vm::pinned_vm_object::PinnedVmObject;
use crate::vm::vm_mapping::VmMapping;
use crate::vm::vm_object::VmObject;

unsafe extern "C" {
    fn memalign(alignment: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// While the last-branch record is far larger, it is not emitted for each
/// event.
pub const MAX_EVENT_RECORD_SIZE: usize = size_of::<PcRecord>();

zr::static_assert!(size_of::<BufferHeader>() == 24);
zr::static_assert!(size_of::<RecordHeader>() == 4);
zr::static_assert!(align_of::<RecordHeader>() == 4);
zr::static_assert!(offset_of!(RecordHeader, type_) == 0);
zr::static_assert!(offset_of!(RecordHeader, reserved_flags) == 1);
zr::static_assert!(offset_of!(RecordHeader, event) == 2);
zr::static_assert!(size_of::<TimeRecord>() == 12);
zr::static_assert!(align_of::<TimeRecord>() == 4);
zr::static_assert!(offset_of!(TimeRecord, header) == 0);
zr::static_assert!(offset_of!(TimeRecord, time) == 4);
zr::static_assert!(size_of::<TickRecord>() == 4);
zr::static_assert!(align_of::<TickRecord>() == 4);
zr::static_assert!(offset_of!(TickRecord, header) == 0);
zr::static_assert!(size_of::<CountRecord>() == 12);
zr::static_assert!(align_of::<CountRecord>() == 4);
zr::static_assert!(offset_of!(CountRecord, header) == 0);
zr::static_assert!(offset_of!(CountRecord, count) == 4);
zr::static_assert!(size_of::<ValueRecord>() == 12);
zr::static_assert!(align_of::<ValueRecord>() == 4);
zr::static_assert!(offset_of!(ValueRecord, header) == 0);
zr::static_assert!(offset_of!(ValueRecord, value) == 4);
zr::static_assert!(size_of::<PcRecord>() == 20);
zr::static_assert!(align_of::<PcRecord>() == 4);
zr::static_assert!(offset_of!(PcRecord, header) == 0);
zr::static_assert!(offset_of!(PcRecord, aspace) == 4);
zr::static_assert!(offset_of!(PcRecord, pc) == 12);

/// Per-CPU performance monitor trace buffer state.
#[repr(C, align(64))]
pub struct PerfmonCpuData {
    /// The trace buffer, passed in from userspace.
    pub buffer_vmo: Option<RefPtr<VmObject>>,
    pub buffer_size: usize,

    /// The trace buffer when mapped into kernel space.
    /// This is only done while the trace is running.
    pub buffer_mapping: Option<RefPtr<VmMapping>>,
    pub pinned_buffer: PinnedVmObject,
    pub buffer_start: *mut BufferHeader,
    pub buffer_end: *mut c_void,

    /// The next record to fill.
    pub buffer_next: *mut RecordHeader,

    _opaque: zr::Opaque<()>,
}

zr::static_assert!(align_of::<PerfmonCpuData>() == crate::arch_rs::MAX_CACHE_LINE);

impl PerfmonCpuData {
    /// Creates an empty `PerfmonCpuData`.
    pub const fn new() -> Self {
        Self {
            buffer_vmo: None,
            buffer_size: 0,
            buffer_mapping: None,
            pinned_buffer: PinnedVmObject::new(),
            buffer_start: ptr::null_mut(),
            buffer_end: ptr::null_mut(),
            buffer_next: ptr::null_mut(),
            _opaque: zr::Opaque::new(()),
        }
    }
}

impl Default for PerfmonCpuData {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PerfmonCpuData {
    fn drop(&mut self) {
        // Match C++ reverse-declaration member destruction order:
        // 1. `pinned_buffer`, 2. `buffer_mapping`, 3. `buffer_vmo`.
        self.pinned_buffer.reset();
        self.buffer_mapping = None;
        self.buffer_vmo = None;
    }
}

/// Base state for performance monitor sessions across all CPUs.
#[repr(C)]
pub struct PerfmonStateBase {
    /// Number of entries in `cpu_data`.
    pub num_cpus: u32,

    /// An array with one entry for each cpu.
    /// TODO(dje): Ideally this would be something like
    /// `ktl::unique_ptr<PerfmonCpuData[]> cpu_data;`
    /// but that will need to wait for a "new" that handles aligned allocs.
    pub cpu_data: *mut PerfmonCpuData,

    _opaque: zr::Opaque<()>,
}

impl PerfmonStateBase {
    /// Creates a new `PerfmonStateBase` for `n_cpus` CPUs.
    pub const fn new(n_cpus: u32) -> Self {
        Self { num_cpus: n_cpus, cpu_data: ptr::null_mut(), _opaque: zr::Opaque::new(()) }
    }

    /// Helper to allocate space for per-cpu state.
    /// This data must be properly aligned so it is allocated specially.
    pub fn allocate_per_cpu_data(&mut self) -> bool {
        debug_assert!(self.cpu_data.is_null());

        let space_needed = size_of::<PerfmonCpuData>() * self.num_cpus as usize;
        // SAFETY: `align_of::<PerfmonCpuData>()` is a valid power-of-two alignment.
        let cdata = unsafe {
            memalign(align_of::<PerfmonCpuData>(), space_needed).cast::<PerfmonCpuData>()
        };
        if cdata.is_null() {
            return false;
        }

        for cpu in 0..self.num_cpus as usize {
            // SAFETY: `cdata` is a valid, newly allocated, aligned buffer of `self.num_cpus`
            // `PerfmonCpuData` elements, so `cdata.add(cpu)` is valid for writes.
            unsafe {
                cdata.add(cpu).write(PerfmonCpuData::new());
            }
        }

        self.cpu_data = cdata;
        true
    }
}

impl Drop for PerfmonStateBase {
    fn drop(&mut self) {
        debug_assert!(PERFMON_ACTIVE.load(Ordering::SeqCst) == 0);

        if !self.cpu_data.is_null() {
            for cpu in 0..self.num_cpus as usize {
                // SAFETY: `self.cpu_data` points to an initialized array of `self.num_cpus`
                // `PerfmonCpuData` elements allocated in `allocate_per_cpu_data`.
                unsafe {
                    let data = self.cpu_data.add(cpu);
                    ptr::drop_in_place(data);
                }
            }
            // This frees the memory allocated by `memalign()`.
            // SAFETY: `self.cpu_data` was allocated by `memalign()` in `allocate_per_cpu_data`.
            unsafe {
                free(self.cpu_data.cast::<c_void>());
            }
        }
    }
}

/// True if the chip supports perfmon at the version we require.
pub static PERFMON_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// This is accessed atomically as it is also accessed by the PMI handler.
pub static PERFMON_ACTIVE: AtomicI32 = AtomicI32::new(0);

// This section contains helper routines to write perfmon records.

/// Writes a performance monitor record header at `hdr`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `RecordHeader` and point to writable `RecordHeader` memory.
pub unsafe fn arch_perfmon_write_header(
    hdr: *mut RecordHeader,
    record_type: RecordType,
    event: PmuEventId,
) {
    // SAFETY: Caller guarantees `hdr` is properly aligned and valid for writes.
    unsafe {
        (*hdr).type_ = record_type;
        (*hdr).reserved_flags = 0;
        (*hdr).event = event;
    }
}

/// Writes a `TimeRecord` at `hdr` and returns a pointer to the next `RecordHeader`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `TimeRecord` and point to writable buffer memory with at
/// least `size_of::<TimeRecord>()` bytes available.
pub unsafe fn arch_perfmon_write_time_record(
    hdr: *mut RecordHeader,
    event: PmuEventId,
    time: zx_instant_mono_ticks_t,
) -> *mut RecordHeader {
    let rec = hdr.cast::<TimeRecord>();
    // SAFETY: Caller guarantees `hdr` is aligned for `TimeRecord` and has space for a `TimeRecord`.
    unsafe {
        arch_perfmon_write_header(&raw mut (*rec).header, kRecordTypeTime, event);
        (*rec).time = time;
        rec.add(1).cast::<RecordHeader>()
    }
}

/// Writes a `TickRecord` at `hdr` and returns a pointer to the next `RecordHeader`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `TickRecord` and point to writable buffer memory with at
/// least `size_of::<TickRecord>()` bytes available.
pub unsafe fn arch_perfmon_write_tick_record(
    hdr: *mut RecordHeader,
    event: PmuEventId,
) -> *mut RecordHeader {
    let rec = hdr.cast::<TickRecord>();
    // SAFETY: Caller guarantees `hdr` is aligned for `TickRecord` and has space for a `TickRecord`.
    unsafe {
        arch_perfmon_write_header(&raw mut (*rec).header, kRecordTypeTick, event);
        rec.add(1).cast::<RecordHeader>()
    }
}

/// Writes a `CountRecord` at `hdr` and returns a pointer to the next `RecordHeader`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `CountRecord` and point to writable buffer memory with at
/// least `size_of::<CountRecord>()` bytes available.
pub unsafe fn arch_perfmon_write_count_record(
    hdr: *mut RecordHeader,
    event: PmuEventId,
    count: u64,
) -> *mut RecordHeader {
    let rec = hdr.cast::<CountRecord>();
    // SAFETY: Caller guarantees `hdr` is aligned for `CountRecord` and has space for a
    // `CountRecord`.
    unsafe {
        arch_perfmon_write_header(&raw mut (*rec).header, kRecordTypeCount, event);
        (*rec).count = count;
        rec.add(1).cast::<RecordHeader>()
    }
}

/// Writes a `ValueRecord` at `hdr` and returns a pointer to the next `RecordHeader`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `ValueRecord` and point to writable buffer memory with at
/// least `size_of::<ValueRecord>()` bytes available.
pub unsafe fn arch_perfmon_write_value_record(
    hdr: *mut RecordHeader,
    event: PmuEventId,
    value: u64,
) -> *mut RecordHeader {
    let rec = hdr.cast::<ValueRecord>();
    // SAFETY: Caller guarantees `hdr` is aligned for `ValueRecord` and has space for a
    // `ValueRecord`.
    unsafe {
        arch_perfmon_write_header(&raw mut (*rec).header, kRecordTypeValue, event);
        (*rec).value = value;
        rec.add(1).cast::<RecordHeader>()
    }
}

/// Writes a `PcRecord` at `hdr` and returns a pointer to the next `RecordHeader`.
///
/// # Safety
///
/// `hdr` must be properly aligned for `PcRecord` and point to writable buffer memory with at
/// least `size_of::<PcRecord>()` bytes available.
pub unsafe fn arch_perfmon_write_pc_record(
    hdr: *mut RecordHeader,
    event: PmuEventId,
    aspace: u64,
    pc: u64,
) -> *mut RecordHeader {
    let rec = hdr.cast::<PcRecord>();
    // SAFETY: Caller guarantees `hdr` is aligned for `PcRecord` and has space for a `PcRecord`.
    unsafe {
        arch_perfmon_write_header(&raw mut (*rec).header, kRecordTypePc, event);
        (*rec).aspace = aspace;
        (*rec).pc = pc;
        rec.add(1).cast::<RecordHeader>()
    }
}

#[cfg(ktest)]
/// Unit tests for `perfmon`.
#[unittest::suite(name = "perfmon")]
mod tests {
    use unittest::{assert_eq, assert_false, assert_true};

    /// Tests allocation and cleanup of `PerfmonStateBase` per-CPU data.
    #[test]
    fn test_perfmon_state_base_allocate_and_drop() {
        let mut state = PerfmonStateBase::new(4);
        assert_eq!(state.num_cpus, 4);
        assert_true!(state.cpu_data.is_null());

        assert_true!(state.allocate_per_cpu_data());
        assert_false!(state.cpu_data.is_null());
        assert_eq!((state.cpu_data as usize) % crate::arch_rs::MAX_CACHE_LINE, 0);

        for cpu in 0..4usize {
            // SAFETY: `state.cpu_data` has 4 initialized `PerfmonCpuData` elements.
            let data = unsafe { &*state.cpu_data.add(cpu) };
            assert_true!(data.buffer_vmo.is_none());
            assert_eq!(data.buffer_size, 0);
            assert_true!(data.buffer_mapping.is_none());
            assert_true!(data.buffer_start.is_null());
            assert_true!(data.buffer_end.is_null());
            assert_true!(data.buffer_next.is_null());
        }
    }

    /// Tests record writing helpers.
    #[test]
    fn test_write_records() {
        let mut buf = [0u32; 32];
        let base = buf.as_mut_ptr().cast::<RecordHeader>();

        // SAFETY: `buf` is 128 bytes (4-byte aligned), which is more than enough for one of each
        // record type (12 + 4 + 12 + 12 + 20 = 60 bytes).
        unsafe {
            let next = arch_perfmon_write_time_record(base, 0x101, 0x1122_3344_5566_7788);
            assert_eq!(next as usize - base as usize, size_of::<TimeRecord>());
            let time_rec = ptr::read_unaligned(base.cast::<TimeRecord>());
            assert_eq!(time_rec.header.type_, kRecordTypeTime);
            assert_eq!(time_rec.header.reserved_flags, 0);
            assert_eq!(time_rec.header.event, 0x101);
            assert_eq!(time_rec.time, 0x1122_3344_5566_7788);

            let prev = next;
            let next = arch_perfmon_write_tick_record(prev, 0x102);
            assert_eq!(next as usize - prev as usize, size_of::<TickRecord>());
            let tick_rec = ptr::read_unaligned(prev.cast::<TickRecord>());
            assert_eq!(tick_rec.header.type_, kRecordTypeTick);
            assert_eq!(tick_rec.header.reserved_flags, 0);
            assert_eq!(tick_rec.header.event, 0x102);

            let prev = next;
            let next = arch_perfmon_write_count_record(prev, 0x103, 0xdead_beef_cafe_babe);
            assert_eq!(next as usize - prev as usize, size_of::<CountRecord>());
            let count_rec = ptr::read_unaligned(prev.cast::<CountRecord>());
            assert_eq!(count_rec.header.type_, kRecordTypeCount);
            assert_eq!(count_rec.header.reserved_flags, 0);
            assert_eq!(count_rec.header.event, 0x103);
            assert_eq!(count_rec.count, 0xdead_beef_cafe_babe);

            let prev = next;
            let next = arch_perfmon_write_value_record(prev, 0x104, 0x0102_0304_0506_0708);
            assert_eq!(next as usize - prev as usize, size_of::<ValueRecord>());
            let value_rec = ptr::read_unaligned(prev.cast::<ValueRecord>());
            assert_eq!(value_rec.header.type_, kRecordTypeValue);
            assert_eq!(value_rec.header.reserved_flags, 0);
            assert_eq!(value_rec.header.event, 0x104);
            assert_eq!(value_rec.value, 0x0102_0304_0506_0708);

            let prev = next;
            let next = arch_perfmon_write_pc_record(prev, 0x105, 0x42, 0xffff_0000_1234_5678);
            assert_eq!(next as usize - prev as usize, size_of::<PcRecord>());
            let pc_rec = ptr::read_unaligned(prev.cast::<PcRecord>());
            assert_eq!(pc_rec.header.type_, kRecordTypePc);
            assert_eq!(pc_rec.header.reserved_flags, 0);
            assert_eq!(pc_rec.header.event, 0x105);
            assert_eq!(pc_rec.aspace, 0x42);
            assert_eq!(pc_rec.pc, 0xffff_0000_1234_5678);
        }
    }
}
