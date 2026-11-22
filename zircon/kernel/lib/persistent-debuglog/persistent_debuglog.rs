// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
#[cfg(console_enabled)]
use core::ffi::c_int;
use core::ffi::{c_char, c_void};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::ksync_kernel::IrqSavePolicy;
use crate::vm::heap::HEAP_MAX_ALLOC_SIZE;
#[cfg(console_enabled)]
use kprint::kprint;
use kprint::kprintln;
#[cfg(console_enabled)]
use zx_status::Status;

unsafe extern "C" {
    fn arch_clean_cache_range(start: usize, len: usize);
}

/// Persistent RAM allocation granularity configured for the kernel build.
pub const PERSISTENT_RAM_ALLOCATION_GRANULARITY: usize =
    zr::parse_usize(env!("PERSISTENT_RAM_ALLOCATION_GRANULARITY"))
        .expect("PERSISTENT_RAM_ALLOCATION_GRANULARITY invalid");
// The allocation granularity of persistent RAM must be a power of two greater than 0.
zr::static_assert!(PERSISTENT_RAM_ALLOCATION_GRANULARITY.is_power_of_two());

/// The amount of persistent RAM that persistent debug logging would like to have.
pub const TARGET_PERSISTENT_DEBUGLOG_SIZE: usize =
    zr::parse_usize(env!("TARGET_PERSISTENT_DEBUGLOG_SIZE"))
        .expect("TARGET_PERSISTENT_DEBUGLOG_SIZE invalid");
// The persistent debuglog target size must be a multiple of the persistent RAM allocation
// granularity, and fit within u32::MAX.
zr::static_assert!(
    TARGET_PERSISTENT_DEBUGLOG_SIZE.is_multiple_of(PERSISTENT_RAM_ALLOCATION_GRANULARITY)
);
zr::static_assert!(TARGET_PERSISTENT_DEBUGLOG_SIZE <= u32::MAX as usize);

/// Flushes CPU data cache lines covering `bytes` to main memory so that
/// persistent debuglog writes survive non-power-cycle warm resets.
#[inline]
fn clean_cache_range(bytes: &[u8]) {
    // SAFETY: `arch_clean_cache_range` flushes CPU data cache lines covering
    // `[addr, addr + len)` to main memory and does not dereference or mutate
    // Rust references.
    unsafe {
        arch_clean_cache_range(bytes.as_ptr().addr(), bytes.len());
    }
}

/// On-RAM header at the start of the persistent debuglog region.
#[repr(C, packed)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct LogHeader {
    magic: u32,
    rd_ptr: u32,
}

zr::static_assert!(
    core::mem::size_of::<LogHeader>() == 8 && core::mem::align_of::<LogHeader>() == 1
);
// Persistent dlog header's alignment must be a power of 2 in size
zr::static_assert!(core::mem::align_of::<LogHeader>().is_power_of_two());

impl LogHeader {
    /// FourCC `"Plog"` in little-endian byte order (`0x676f_6c50`).
    const MAGIC: u32 = u32::from_le_bytes(*b"Plog");

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `LogHeader` is `#[repr(C, packed)]` with two `u32` fields and no padding,
        // so all `size_of::<Self>()` bytes are initialized and valid to view as `&[u8]`.
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    #[inline]
    fn is_magic_valid(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Returns true if this header has a valid magic value and an in-bounds `rd_ptr` for a
    /// `payload_size`-byte buffer.
    #[inline]
    fn is_recoverable(&self, payload_size: u32) -> bool {
        let rd_ptr = self.rd_ptr;
        self.is_magic_valid() && rd_ptr < payload_size
    }

    #[inline]
    fn validate_magic(&mut self) {
        self.magic = Self::MAGIC;
        clean_cache_range(self.as_bytes());
    }

    #[inline]
    fn invalidate_magic(&mut self) {
        self.magic = 0;
        clean_cache_range(self.as_bytes());
    }
}

/// Info about where the persisted log is in RAM, if we have a persisted log.
/// Laid out to match the 16-byte C++ `plog` representation when wrapped in
/// `Option<ActivePersistentLog>`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ActivePersistentLog {
    hdr: NonNull<LogHeader>,
    payload_size: u32,
}

zr::static_assert!(
    core::mem::size_of::<ActivePersistentLog>() == 16
        && core::mem::align_of::<ActivePersistentLog>() == 8
);
zr::static_assert!(
    core::mem::size_of::<Option<ActivePersistentLog>>() == 16
        && core::mem::align_of::<Option<ActivePersistentLog>>() == 8
);

// SAFETY: `ActivePersistentLog` encapsulates a persistent RAM mapping owned and
// synchronized by `PersistentDebugLog`'s spinlock.
unsafe impl Send for ActivePersistentLog {}

impl ActivePersistentLog {
    /// Creates a new `ActivePersistentLog` over a persistent RAM allocation.
    ///
    /// # Safety
    /// `hdr` must point to a valid, readable and writable `LogHeader` followed immediately by
    /// `payload_size` readable and writable bytes within the same persistent RAM allocation,
    /// and all accesses through this `ActivePersistentLog` must be synchronized by
    /// `persistent_log_lock`.
    const unsafe fn new(hdr: NonNull<LogHeader>, payload_size: u32) -> Self {
        Self { hdr, payload_size }
    }

    /// Returns a shared reference to the `LogHeader`.
    #[inline]
    fn header(&self) -> &LogHeader {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is valid and non-aliased
        // mutably while `self` is borrowed.
        unsafe { self.hdr.as_ref() }
    }

    /// Returns an exclusive reference to the `LogHeader`.
    #[inline]
    fn header_mut(&mut self) -> &mut LogHeader {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is valid and exclusively
        // accessed while `self` is mutably borrowed.
        unsafe { self.hdr.as_mut() }
    }

    /// Returns a shared slice of the payload bytes immediately following `self.hdr`.
    #[inline]
    fn payload(&self) -> &[u8] {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is followed immediately by
        // `self.payload_size` valid bytes within the same allocation.
        unsafe {
            let payload_ptr = self.hdr.as_ptr().add(1).cast::<u8>().cast_const();
            core::slice::from_raw_parts(payload_ptr, self.payload_size as usize)
        }
    }

    /// Returns an exclusive slice of the payload bytes immediately following `self.hdr`.
    #[inline]
    fn payload_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` is followed immediately by
        // `self.payload_size` valid writable bytes within the same allocation.
        unsafe {
            let payload_ptr = self.hdr.as_ptr().add(1).cast::<u8>();
            core::slice::from_raw_parts_mut(payload_ptr, self.payload_size as usize)
        }
    }

    /// Returns simultaneous exclusive references to the `LogHeader` and the disjoint payload
    /// byte slice immediately following it.
    #[inline]
    fn header_and_payload_mut(&mut self) -> (&mut LogHeader, &mut [u8]) {
        // SAFETY: `ActivePersistentLog::new` guarantees `self.hdr` points to a valid
        // `LogHeader` followed immediately by `self.payload_size` disjoint payload bytes in the
        // same allocation, and `&mut self` ensures exclusive access to both regions.
        unsafe {
            let hdr_ptr = self.hdr.as_ptr();
            let payload_ptr = hdr_ptr.add(1).cast::<u8>();
            (
                &mut *hdr_ptr,
                core::slice::from_raw_parts_mut(payload_ptr, self.payload_size as usize),
            )
        }
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    #[inline]
    fn invalidate(&mut self) {
        self.header_mut().invalidate_magic();
    }
}

/// We don't bother to lock this structure.  The log, if present, is recovered
/// during early boot while we are still running on a single core.  After that,
/// the recovered data is only ever accessed in a read only fashion, so there
/// is no real need to provide any explicit synchronization.
#[repr(C)]
#[derive(Debug)]
struct RecoveredPersistentLog {
    data: Option<NonNull<u8>>,
    capacity: u32,
    size: u32,
}

zr::static_assert!(
    core::mem::size_of::<RecoveredPersistentLog>() == 16
        && core::mem::align_of::<RecoveredPersistentLog>() == 8
);

// SAFETY: `RecoveredPersistentLog` points to recovery storage (a static buffer or a leaked heap
// allocation) populated during single-core early boot (or under test control) and read-only
// thereafter.
unsafe impl Send for RecoveredPersistentLog {}
// SAFETY: After single-core early boot initialization, `RecoveredPersistentLog` is accessed
// in a read-only fashion.
unsafe impl Sync for RecoveredPersistentLog {}

impl RecoveredPersistentLog {
    const fn new(data: Option<NonNull<u8>>, capacity: u32) -> Self {
        Self { data, capacity, size: 0 }
    }

    /// Used only by testing to reset a log to its "pre SetLocation'ed" state.
    #[cfg(ktest)]
    #[inline]
    fn reset(&mut self) {
        self.size = 0;
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    /// Returns `None` when `size == 0`.
    #[inline]
    fn as_bytes(&self) -> Option<&[u8]> {
        if self.size == 0 {
            return None;
        }
        let ptr = self.data?;
        // SAFETY: When `self.data` is `Some(ptr)`, it points to a backing store of at least
        // `self.capacity` bytes, and `self.size <= self.capacity` bytes have been initialized.
        Some(unsafe { core::slice::from_raw_parts(ptr.as_ptr().cast_const(), self.size as usize) })
    }

    /// Recovers valid log contents from a circular persistent payload buffer starting at `rd_ptr`.
    ///
    /// # Safety
    /// If `self.data` is `Some(ptr)` and `self.capacity > 0`, `ptr` must point to a writable
    /// (possibly uninitialized) allocation of at least `self.capacity` bytes.
    unsafe fn recover_from(&mut self, payload: &[u8], rd_ptr: u32) {
        self.size = 0;
        let Some(destination_ptr) = self.data else {
            return;
        };
        if self.capacity == 0 || payload.is_empty() {
            return;
        }

        // Save as much as we can in our recovery buffer, discarding the oldest data first if we
        // cannot fit it all.
        // C++ DEBUG_ASSERTs capacity <= u32::MAX here; capacity is a u32 so this holds by
        // construction.
        let payload_size = payload.len();
        let capacity = self.capacity as usize;
        let (bytes_to_process, mut read_offset) = if payload_size <= capacity {
            (payload_size, (rd_ptr as usize) % payload_size)
        } else {
            (capacity, ((rd_ptr as usize) + (payload_size - capacity)) % payload_size)
        };
        let mut write_offset: usize = 0;

        // SAFETY: Caller guarantees `destination_ptr` points to at least `self.capacity`
        // writable (possibly uninitialized) bytes.
        let destination = unsafe {
            core::slice::from_raw_parts_mut(
                destination_ptr.as_ptr().cast::<MaybeUninit<u8>>(),
                capacity,
            )
        };

        for _ in 0..bytes_to_process {
            let byte = payload[read_offset];
            read_offset += 1;
            if read_offset >= payload_size {
                read_offset = 0;
            }

            // Skip all zeros.
            if byte == 0 {
                continue;
            }

            destination[write_offset].write(
                if byte.is_ascii_graphic() || byte == b' ' || byte == b'\n' { byte } else { b'?' },
            );
            write_offset += 1;
        }
        self.size = write_offset as u32;
    }
}

/// Core persistent debuglog state machine managing the spinlock-protected active persistent RAM
/// ring buffer and the recovered previous-boot crashlog buffer.
#[ksync::guarded]
struct PersistentDebugLog {
    #[mutex]
    persistent_log_lock: ksync::KMutex<crate::ksync_kernel::TrackedSpinlock>,

    /// Info about where the persisted log is in RAM, if we have a persisted log.
    #[guarded_by(persistent_log_lock)]
    plog: Option<ActivePersistentLog>,

    /// We don't bother to lock this structure.  The log, if present, is recovered
    /// during early boot while we are still running on a single core.  After that,
    /// the recovered data is only ever accessed in a read only fashion, so there
    /// is no real need to provide any explicit synchronization.
    recovered_persistent_log: UnsafeCell<RecoveredPersistentLog>,
}

// SAFETY: `PersistentDebugLog` synchronizes all mutable access to `plog` and
// `recovered_persistent_log` using `persistent_log_lock` (`IrqSavePolicy`), and
// `recovered_persistent_log` is only read after single-core early boot initialization.
unsafe impl Send for PersistentDebugLog {}
// SAFETY: Shared references to `PersistentDebugLog` synchronize mutations via
// `persistent_log_lock` (`IrqSavePolicy`) and only read `recovered_persistent_log` after
// early boot recovery.
unsafe impl Sync for PersistentDebugLog {}

impl core::fmt::Debug for PersistentDebugLog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PersistentDebugLog")
            .field("persistent_log_lock", &self.persistent_log_lock)
            .field("plog", &self.plog)
            .finish_non_exhaustive()
    }
}

impl PersistentDebugLog {
    /// Returns a pin-initializer for `PersistentDebugLog` backed by `recovered_data` of length
    /// `recovered_capacity`.
    fn init(
        recovered_data: Option<NonNull<u8>>,
        recovered_capacity: u32,
    ) -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            persistent_log_lock <- ksync::KMutex::init(),
            plog: ksync::KCell::new(None),
            recovered_persistent_log: UnsafeCell::new(RecoveredPersistentLog::new(
                recovered_data,
                recovered_capacity,
            )),
        })
    }

    /// Called very early in boot.  Attempts to recover any previously persisted
    /// log by first performing consistency checks on the header, and then
    /// recovering as much as it can into the recovery buffer.
    ///
    /// # Safety
    /// If `virt` is non-null and `len > size_of::<LogHeader>()`, `virt` must point to a valid,
    /// readable and writable persistent RAM mapping of `len` bytes that remains valid while this
    /// `PersistentDebugLog` is active (or until `force_reset` is called in tests).
    unsafe fn set_location(&self, virt: *mut u8, len: usize) {
        // The location of the persistent dlog must be compatible with the header's
        // alignment, and the amount of space for the log payload must be positive,
        // otherwise we cannot effectively use the memory.
        let Some(virt_ptr) = NonNull::new(virt) else {
            return;
        };
        let header_size = core::mem::size_of::<LogHeader>();
        let header_align = core::mem::align_of::<LogHeader>();
        if (virt_ptr.addr().get() & (header_align - 1)) != 0 || len <= header_size {
            return;
        }

        // If we already have a persistent dlog location, then this function has been
        // (improperly) called twice.  It is tempting to assert here, but we are
        // currently very early in boot, which would make debugging the assert
        // extremely difficult.  Instead, just ignore this request.
        //
        // TODO(johngro): come back here and try to put a warning/OOPS in to the dlog
        // buffer?
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<IrqSavePolicy>()
        );
        if guard.plog().is_some() {
            return;
        }

        // Check our log header to see if we have a recovered persistent log.  If we
        // do, recover the log into our recovery buffer so that it can be picked up
        // later on and sent up to usermode in the crashlog.
        //
        // Recovery resets the active log immediately after copying it out, so the recovery
        // buffer must already be allocated. Early ZBI handoff uses a static buffer, while
        // deferred recovery at `LK_INIT_LEVEL_HEAP` uses a heap buffer.
        let header_ptr = virt_ptr.cast::<LogHeader>();
        let payload_size = core::cmp::min(u32::MAX as usize, len - header_size) as u32;
        // SAFETY: Caller guarantees `virt` points to a valid persistent RAM buffer of `len`
        // bytes (`>= size_of::<LogHeader>() + payload_size`), and all accesses to `active_log`
        // are synchronized by `persistent_log_lock`.
        let mut active_log = unsafe { ActivePersistentLog::new(header_ptr, payload_size) };

        let header = active_log.header();
        let initial_rd_ptr = header.rd_ptr;
        if header.is_recoverable(payload_size) {
            // This looks as good as it is going to.  Our magic number is valid, and our
            // read pointer lies within the available payload size.  Save as much as we
            // can in our recovery buffer, discarding the oldest data first if we cannot
            // fit it all.
            let payload = active_log.payload();
            // SAFETY: We hold `persistent_log_lock` exclusively during early boot (or after
            // `force_reset` in single-threaded unit tests), and `payload` is disjoint from
            // `self.recovered_persistent_log`.
            unsafe {
                let recovered_log = &mut *self.recovered_persistent_log.get();
                recovered_log.recover_from(payload, initial_rd_ptr);
            }
        }

        // Reset the log, then install it, and we are done.
        let header = active_log.header_mut();
        header.validate_magic();
        header.rd_ptr = 0;
        clean_cache_range(header.as_bytes());
        let payload = active_log.payload_mut();
        payload.fill(0);
        clean_cache_range(payload);
        *guard.as_mut().plog_mut() = Some(active_log);
    }

    /// Writes `str` to the persistent dlog, if enabled.  Otherwise, this is a
    /// no-op.
    fn write(&self, str: &[u8]) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<IrqSavePolicy>()
        );

        // If we have no persistent log, just get out.
        let Some(plog) = guard.as_mut().plog_mut() else {
            return;
        };

        // Copy the data into our log
        let payload_size = plog.payload_size as usize;
        let (hdr, payload) = plog.header_and_payload_mut();
        let todo: usize;
        let offset: usize;
        if str.len() > payload_size {
            todo = payload_size;
            offset = str.len() - payload_size;
        } else {
            todo = str.len();
            offset = 0;
        }

        let rd_ptr = hdr.rd_ptr as usize;
        let space = payload_size - rd_ptr;
        if space > todo {
            payload[rd_ptr..rd_ptr + todo].copy_from_slice(&str[offset..offset + todo]);
            clean_cache_range(&payload[rd_ptr..rd_ptr + todo]);

            hdr.rd_ptr += todo as u32;
            clean_cache_range(hdr.as_bytes());
        } else {
            payload[rd_ptr..rd_ptr + space].copy_from_slice(&str[offset..offset + space]);
            payload[..todo - space].copy_from_slice(&str[offset + space..offset + todo]);
            clean_cache_range(&payload[rd_ptr..rd_ptr + space]);
            clean_cache_range(&payload[..todo - space]);

            hdr.rd_ptr = (todo - space) as u32;
            clean_cache_range(hdr.as_bytes());
        }
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    #[inline]
    fn recovered_log(&self) -> Option<&[u8]> {
        // SAFETY: We don't bother to lock this structure.  The log, if present, is recovered
        // during early boot while we are still running on a single core.  After that,
        // the recovered data is only ever accessed in a read only fashion, so there
        // is no real need to provide any explicit synchronization.
        let recovered_log = unsafe { &*self.recovered_persistent_log.get() };
        recovered_log.as_bytes()
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    fn invalidate(&self) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<IrqSavePolicy>()
        );
        if let Some(active_log) = guard.as_mut().plog_mut() {
            active_log.invalidate();
        }
    }

    /// Replaces the buffer that `set_location` recovers the previous boot's log into.
    ///
    /// # Safety
    /// Must be called before `set_location`, while still running on a single core. If `data` is
    /// `Some(ptr)`, `ptr` must point to a writable (possibly uninitialized) allocation of at least
    /// `capacity` bytes that remains valid for the life of `self`.
    unsafe fn set_recovery_buffer(&self, data: Option<NonNull<u8>>, capacity: u32) {
        // SAFETY: The caller guarantees single-core early boot, so nothing else accesses the
        // recovered log concurrently.
        let recovered_log = unsafe { &mut *self.recovered_persistent_log.get() };
        *recovered_log = RecoveredPersistentLog::new(data, capacity);
    }

    /// Used only by testing to reset a log to its "pre SetLocation'ed" state.
    #[cfg(ktest)]
    fn force_reset(&self) {
        ksync::lock!(
            let mut guard = self.lock_persistent_log_lock_policy::<IrqSavePolicy>()
        );
        *guard.as_mut().plog_mut() = None;
        // SAFETY: `force_reset` is used only by single-threaded testing while holding
        // `persistent_log_lock` with `IrqSavePolicy`.
        let recovered_log = unsafe { &mut *self.recovered_persistent_log.get() };
        recovered_log.reset();
    }
}

/// Global persistent debuglog state and static recovery buffer of `STORAGE_SIZE` bytes.
///
/// If the persistent RAM payload fits in `STORAGE_SIZE`, `set_location` recovers the previous
/// boot's log into the static buffer and activates logging immediately.
///
/// If the payload exceeds `STORAGE_SIZE` (for example, when `STORAGE_SIZE == 0` and
/// `kernel.persistent-dlog.target-size` enables logging at runtime), `set_location` cannot recover
/// the log during ZBI handoff because the kernel heap is not yet initialized. Instead,
/// `set_location` saves the persistent RAM range without modifying it, and `init_heap` allocates a
/// heap recovery buffer and activates logging at `LK_INIT_LEVEL_HEAP`. Until `init_heap` runs,
/// `write` is a lock-free no-op: early boot logs are not persisted, and a reset before heap
/// initialization preserves the prior boot's log in persistent RAM.
struct PersistentDebuglogGlobals<const STORAGE_SIZE: usize> {
    instance: lazy_init::LazyInit<PersistentDebugLog, lazy_init::NoCheck>,
    storage: UnsafeCell<[u8; STORAGE_SIZE]>,
    deferred_location: UnsafeCell<Option<(NonNull<u8>, usize)>>,
    /// Fast-path gate so `write` avoids taking the spinlock when no persistent log is active.
    active: AtomicBool,
}

// SAFETY: `PersistentDebuglogGlobals` synchronizes active log mutations via
// `PersistentDebugLog`'s spinlock (`IrqSavePolicy`) and only mutates `storage` and
// `deferred_location` during single-core early boot initialization before read-only access.
unsafe impl<const STORAGE_SIZE: usize> Send for PersistentDebuglogGlobals<STORAGE_SIZE> {}
// SAFETY: Shared references to `PersistentDebuglogGlobals` synchronize active log
// mutations via `PersistentDebugLog`'s spinlock (`IrqSavePolicy`) and only read
// `storage` and `deferred_location` after single-core early boot recovery.
unsafe impl<const STORAGE_SIZE: usize> Sync for PersistentDebuglogGlobals<STORAGE_SIZE> {}

impl<const STORAGE_SIZE: usize> PersistentDebuglogGlobals<STORAGE_SIZE> {
    /// Constructs a new `PersistentDebuglogGlobals` with an internal recovery buffer of
    /// `STORAGE_SIZE` bytes.
    const fn new() -> Self {
        const {
            assert!(STORAGE_SIZE <= u32::MAX as usize);
        }
        Self {
            instance: lazy_init::LazyInit::uninit(),
            storage: UnsafeCell::new([0; STORAGE_SIZE]),
            deferred_location: UnsafeCell::new(None),
            active: AtomicBool::new(false),
        }
    }

    #[inline]
    fn instance(&self) -> &PersistentDebugLog {
        // SAFETY: `init_early` must be called before any other method on `self`, so `instance`
        // is initialized.
        unsafe { self.instance.get_unchecked() }
    }

    /// Fast-path gate for `write` and `invalidate` so they avoid taking the spinlock before a
    /// location is active.
    #[inline]
    fn active_instance(&self) -> Option<&PersistentDebugLog> {
        if !self.active.load(Ordering::Acquire) {
            return None;
        }
        Some(self.instance())
    }

    /// Initializes `instance`, using `storage` as its recovery buffer.
    ///
    /// # Safety
    /// - Must be called exactly once.
    /// - Must be called on a single CPU, before any other method on `self`.
    unsafe fn init_early(&'static self) {
        let (storage_ptr, storage_size) = if STORAGE_SIZE == 0 {
            (None, 0)
        } else {
            (NonNull::new(self.storage.get().cast::<u8>()), STORAGE_SIZE as u32)
        };
        // SAFETY: `init_pin` requires initialization to be serialized with all other access to
        // `instance`. The caller guarantees that this runs once, on a single CPU, before any
        // other method on `self`. `self.storage` is valid for `STORAGE_SIZE` bytes for the
        // `'static` lifetime of `self`.
        let _ = unsafe {
            Pin::static_ref(&self.instance)
                .init_pin(PersistentDebugLog::init(storage_ptr, storage_size))
        };
    }

    /// Sets the virtual address of where to store the persistent, assuming that we
    /// have one.  This needs to happen early in boot, usually during ZBI header
    /// processing, before we start up the secondary CPUs.
    ///
    /// # Safety
    /// - Must be called on a single CPU, before `init_heap`, because it writes
    ///   `deferred_location` without synchronization.
    /// - If `vaddr` is non-null and `len > size_of::<LogHeader>()`, `vaddr` must point to a
    ///   persistent RAM mapping that is valid for reads and writes of `len` bytes for the rest
    ///   of the kernel's lifetime.
    unsafe fn set_location(&self, vaddr: *mut u8, len: usize) {
        let instance = self.instance();
        let header_size = core::mem::size_of::<LogHeader>();
        if STORAGE_SIZE != 0 && len.saturating_sub(header_size) <= STORAGE_SIZE {
            // SAFETY: `PersistentDebugLog::set_location` requires `vaddr` to be valid for `len`
            // bytes while the log is active, which our caller guarantees for the rest of the
            // kernel's lifetime.
            unsafe {
                instance.set_location(vaddr, len);
            }
            self.active.store(true, Ordering::Release);
            return;
        }

        // The static recovery buffer (if any) is too small for this log, so don't touch the
        // previous boot's log until `init_heap` can allocate one.
        let Some(vaddr) = NonNull::new(vaddr) else {
            return;
        };
        if len <= header_size {
            return;
        }
        // SAFETY: Our caller guarantees that we are on a single CPU before `init_heap`, so
        // nothing else accesses `deferred_location` concurrently.
        let deferred_location = unsafe { &mut *self.deferred_location.get() };
        if deferred_location.is_none() && !self.active.load(Ordering::Relaxed) {
            *deferred_location = Some((vaddr, len));
        }
    }

    /// Wrapper around [`PersistentDebuglogGlobals::set_location`] taking a static mutable byte
    /// slice.
    ///
    /// # Safety
    /// Must be called on a single CPU, before `init_heap`.
    unsafe fn set_location_slice(&self, buffer: &'static mut [u8]) {
        // SAFETY: `set_location` requires a single CPU before `init_heap`, which our caller
        // guarantees. It also requires `vaddr` to be valid for reads and writes of `len` bytes
        // for the rest of the kernel's lifetime, which holds because `buffer` is a
        // `&'static mut [u8]` of `buffer.len()` bytes.
        unsafe {
            self.set_location(buffer.as_mut_ptr(), buffer.len());
        }
    }

    /// Activates a location deferred by `set_location`, recovering the previous boot's log (if
    /// any) into a heap buffer capped at `HEAP_MAX_ALLOC_SIZE` first.
    ///
    /// # Safety
    /// - Must be called after the heap is available.
    /// - Must be called on a single CPU, because it accesses `deferred_location` and the
    ///   recovery buffer without synchronization.
    unsafe fn init_heap(&self) {
        // SAFETY: Our caller upholds the same preconditions.
        unsafe { self.init_heap_with_limit(HEAP_MAX_ALLOC_SIZE) };
    }

    /// Helper for [`Self::init_heap`] parameterized by `max_recovery_bytes`.
    ///
    /// # Safety
    /// - Must be called after the heap is available.
    /// - Must be called on a single CPU, because it accesses `deferred_location` and the
    ///   recovery buffer without synchronization.
    unsafe fn init_heap_with_limit(&self, max_recovery_bytes: usize) {
        let instance = self.instance();
        // SAFETY: Our caller guarantees that we are on a single CPU, so nothing else accesses
        // `deferred_location` concurrently.
        let Some((vaddr, len)) = (unsafe { (*self.deferred_location.get()).take() }) else {
            return;
        };

        // Only allocate a recovery buffer if there is something to recover, so that cold boots
        // and clean reboots cost no heap.
        let header_size = core::mem::size_of::<LogHeader>();
        let payload_size = core::cmp::min(u32::MAX as usize, len - header_size) as u32;
        // SAFETY: `set_location` only stores `(vaddr, len)` in `deferred_location` when its
        // caller guarantees that `vaddr` is valid for `len > size_of::<LogHeader>()` bytes for
        // the rest of the kernel's lifetime, and `LogHeader` has alignment 1.
        let header = unsafe { vaddr.cast::<LogHeader>().read() };
        if header.is_recoverable(payload_size) {
            let max_bytes = core::cmp::min(max_recovery_bytes, HEAP_MAX_ALLOC_SIZE);
            let recovery_capacity = core::cmp::min(payload_size as usize, max_bytes);
            if recovery_capacity == 0 {
                // SAFETY: `set_recovery_buffer` requires a single CPU before `set_location`, which
                // our caller guarantees and the call order below ensures.
                unsafe {
                    instance.set_recovery_buffer(None, 0);
                }
            } else {
                let Ok(buffer) = kalloc::Box::<[u8]>::try_new_uninit_slice(recovery_capacity)
                else {
                    // Activating the log resets the persistent RAM header and would erase the
                    // previous boot's log. Leave persistent RAM untouched so a later boot can
                    // recover it.
                    kprintln!(
                        "pdlog: no memory to recover {} byte persistent debug log, not enabling it",
                        payload_size
                    );
                    return;
                };
                let buffer = kalloc::Box::into_raw(buffer);
                // SAFETY: `set_recovery_buffer` requires a single CPU before `set_location`, which
                // our caller guarantees and the call order below ensures. `buffer` is a leaked
                // heap allocation of `recovery_capacity` writable bytes that is never freed, so it
                // outlives `instance`.
                unsafe {
                    instance.set_recovery_buffer(
                        NonNull::new(buffer.cast::<u8>()),
                        recovery_capacity as u32,
                    );
                }
            }
        }

        // SAFETY: `PersistentDebugLog::set_location` requires `vaddr` to be valid for `len`
        // bytes while the log is active. `deferred_location` only holds ranges for which
        // `set_location`'s caller guaranteed that for the rest of the kernel's lifetime.
        unsafe {
            instance.set_location(vaddr.as_ptr(), len);
        }
        self.active.store(true, Ordering::Release);
    }

    /// Writes a string to the persistent dlog, if enabled.  Otherwise, this is a
    /// no-op.
    fn write(&self, bytes: &[u8]) {
        if let Some(instance) = self.active_instance() {
            instance.write(bytes);
        }
    }

    /// Invalidates the state of the persistent dlog.  This gets called every time we
    /// gracefully reboot, so that we don't end up recovering a dlog after reboot and
    /// end up producing an unnecessary crashlog.
    fn invalidate(&self) {
        if let Some(instance) = self.active_instance() {
            instance.invalidate();
        }
    }

    /// Fetch a string view which references the recovered crashlog (if any).
    #[inline]
    fn recovered_log(&self) -> Option<&[u8]> {
        self.instance().recovered_log()
    }

    /// Executes the `"pdlog"` kernel console command.
    ///
    /// # Safety
    /// If `argument_count > 0`, `argument_vector` must be non-null and point to at least
    /// `argument_count` initialized [`crate::console::CmdArgs`] elements.
    #[cfg(console_enabled)]
    unsafe fn cmd(
        &self,
        argument_count: c_int,
        argument_vector: *const crate::console::CmdArgs,
        _flags: u32,
    ) -> c_int {
        let argument_count = usize::try_from(argument_count).unwrap_or(0);
        // SAFETY: Caller guarantees `argument_vector` points to `argument_count` initialized
        // `CmdArgs` elements whenever `argument_count > 0`; a zero or negative count yields `&[]`.
        let arguments = unsafe { zr::slice_from_raw_parts(argument_vector, argument_count) };

        let command_name = arguments.first().map(|arg| arg.as_str()).unwrap_or("pdlog");
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} dump : dump the recovered persistent debug log", command_name);
            Status::INTERNAL.into_raw()
        };

        if arguments.len() < 2 {
            kprintln!("not enough arguments");
            return usage();
        }

        if arguments[1].as_str() == "dump" {
            if let Some(recovered) = self.recovered_log()
                && !recovered.is_empty()
            {
                kprintln!("Recovered {} bytes from the persistent debug log.", recovered.len());
                kprintln!("---- BEGIN ----");
                kprint!("{:s}", recovered);
                kprintln!("---- END ----");
            } else {
                kprintln!("There was no persistent debug log recovered!");
            }
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }
}

static GLOBAL_PERSISTENT_DEBUGLOG: PersistentDebuglogGlobals<TARGET_PERSISTENT_DEBUGLOG_SIZE> =
    PersistentDebuglogGlobals::new();

/// Called once from lib/debuglog during _very_ early init.
///
/// # Safety
/// - Must be called exactly once.
/// - Must be called on a single CPU, before any other `persistent_dlog_*` function.
pub unsafe fn persistent_dlog_init_early() {
    // SAFETY: `init_early` requires a single call on a single CPU before any other method,
    // which our caller guarantees. All other methods are only reached through the other
    // `persistent_dlog_*` functions.
    unsafe { GLOBAL_PERSISTENT_DEBUGLOG.init_early() };
}

/// Sets where the persistent debuglog lives, assuming that we have one.  This
/// needs to happen early in boot, usually during ZBI header processing, before
/// we start up the secondary CPUs.  The persistent debuglog keeps using
/// `buffer` for the life of the kernel.
///
/// # Safety
/// Must be called on a single CPU, before `LK_INIT_LEVEL_HEAP`.
pub unsafe fn persistent_dlog_set_location(buffer: &'static mut [u8]) {
    // SAFETY: `set_location_slice` requires a single CPU before `init_heap`. Our caller
    // guarantees a single CPU before `LK_INIT_LEVEL_HEAP`, and `init_heap` only runs from the
    // `LK_INIT_LEVEL_HEAP` hook.
    unsafe { GLOBAL_PERSISTENT_DEBUGLOG.set_location_slice(buffer) };
}

/// Returns true if the persistent debuglog is currently active.
#[inline]
pub fn persistent_dlog_enabled() -> bool {
    GLOBAL_PERSISTENT_DEBUGLOG.active_instance().is_some()
}

/// Writes `bytes` to the persistent debuglog, if enabled.  Otherwise, this is a
/// no-op.
pub fn persistent_dlog_write(bytes: &[u8]) {
    GLOBAL_PERSISTENT_DEBUGLOG.write(bytes);
}

/// Invalidates the state of the persistent dlog.  This gets called every time we
/// gracefully reboot, so that we don't end up recovering a dlog after reboot and
/// end up producing an unnecessary crashlog.
pub fn persistent_dlog_invalidate() {
    GLOBAL_PERSISTENT_DEBUGLOG.invalidate();
}

/// Fetch a string view which references the recovered crashlog (if any).
pub fn persistent_dlog_get_recovered_log() -> Option<&'static [u8]> {
    GLOBAL_PERSISTENT_DEBUGLOG.recovered_log()
}

/// FFI entry point for C++ `persistent_dlog_init_early()`.
///
/// # Safety
/// Same as [`persistent_dlog_init_early`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_init_early() {
    // SAFETY: Our caller upholds the same preconditions.
    unsafe { persistent_dlog_init_early() };
}

/// FFI entry point for C++ `persistent_dlog_set_location(void* vaddr, size_t len)`.
///
/// # Safety
/// - Must be called on a single CPU, before `LK_INIT_LEVEL_HEAP`.
/// - If `vaddr` is non-null and `len > size_of::<LogHeader>()`, `vaddr` must point to a
///   persistent RAM mapping that is valid for reads and writes of `len` bytes for the rest of
///   the kernel's lifetime.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_set_location(vaddr: *mut c_void, len: usize) {
    // SAFETY: `set_location` requires a single CPU before `init_heap`. Our caller guarantees a
    // single CPU before `LK_INIT_LEVEL_HEAP`, and `init_heap` only runs from the
    // `LK_INIT_LEVEL_HEAP` hook. Our caller also guarantees the `vaddr` and `len` requirement.
    unsafe {
        GLOBAL_PERSISTENT_DEBUGLOG.set_location(vaddr.cast::<u8>(), len);
    }
}

/// FFI entry point for C++ `persistent_dlog_invalidate()`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_persistent_dlog_invalidate() {
    persistent_dlog_invalidate();
}

/// Recovers and activates any persistent debuglog whose size exceeded the static recovery buffer
/// during ZBI handoff. Runs at `LK_INIT_LEVEL_HEAP`, before secondary CPUs start.
fn persistent_dlog_init_heap(_level: init::LkInitLevel) {
    // SAFETY: `init_heap` requires the heap and a single CPU. `LK_INIT_LEVEL_HEAP` hooks run
    // once the heap is available, on the boot CPU before secondary CPUs start.
    unsafe {
        GLOBAL_PERSISTENT_DEBUGLOG.init_heap();
    }
}

init::lk_init_hook!(persistent_dlog_heap, persistent_dlog_init_heap, init::LK_INIT_LEVEL_HEAP);

/// FFI entry point for C++ `persistent_dlog_get_recovered_log()`.
///
/// # Safety
/// If `out_len` is non-null, it must point to a writable `usize`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_persistent_dlog_get_recovered_log(
    out_len: *mut usize,
) -> *const c_char {
    match persistent_dlog_get_recovered_log() {
        Some(bytes) => {
            if let Some(out_len_ptr) = NonNull::new(out_len) {
                // SAFETY: Caller guarantees `out_len` is writable when non-null.
                unsafe {
                    out_len_ptr.as_ptr().write(bytes.len());
                }
            }
            bytes.as_ptr().cast::<c_char>()
        }
        None => {
            if let Some(out_len_ptr) = NonNull::new(out_len) {
                // SAFETY: Caller guarantees `out_len` is writable when non-null.
                unsafe {
                    out_len_ptr.as_ptr().write(0);
                }
            }
            core::ptr::null()
        }
    }
}

/// Console command entry point for `"pdlog"`.
///
/// # Safety
/// If `argument_count > 0`, `argument_vector` must be non-null and point to at least
/// `argument_count` initialized [`crate::console::CmdArgs`] elements.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_pdlog(
    argument_count: c_int,
    argument_vector: *const crate::console::CmdArgs,
    flags: u32,
) -> c_int {
    // SAFETY: Caller guarantees `argument_vector` points to `argument_count` initialized
    // `CmdArgs` elements whenever `argument_count > 0`.
    unsafe { GLOBAL_PERSISTENT_DEBUGLOG.cmd(argument_count, argument_vector, flags) }
}

// Always register `pdlog` because `kernel.persistent-dlog.target-size` can enable the persistent
// debuglog at runtime even when `TARGET_PERSISTENT_DEBUGLOG_SIZE == 0`.
#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_PDLOG,
    c"pdlog".as_ptr(),
    c"pdlog".as_ptr(),
    cmd_pdlog,
    crate::console::CMD_AVAIL_ALWAYS
);

/// Persistent Debuglog Tests
#[cfg(ktest)]
#[unittest::suite(name = "pdlog_rust")]
mod tests {
    use super::{LogHeader, PersistentDebugLog, PersistentDebuglogGlobals};
    use core::ptr::NonNull;
    use unittest::{assert_eq, assert_false, assert_ge, assert_true, unwrap_some};

    struct Buffer {
        storage: kalloc::Box<[u8]>,
        capacity: u32,
    }

    impl Buffer {
        fn setup(new_capacity: u32) -> Option<Self> {
            if new_capacity == 0 {
                return None;
            }
            let storage = kalloc::Box::<[u8]>::try_new_zeroed_slice(new_capacity as usize).ok()?;
            Some(Self { storage, capacity: new_capacity })
        }
    }

    struct TestEnvironment {
        log: kalloc::Box<PersistentDebugLog>,
        log_buffer: Buffer,
        _recovered_log_buffer: Buffer,
        hdr: NonNull<LogHeader>,
    }

    impl TestEnvironment {
        fn setup(log_size: u32, recovered_size: u32) -> Option<Self> {
            let mut log_buffer = Buffer::setup(log_size)?;
            let mut recovered_log_buffer = Buffer::setup(recovered_size)?;
            let mut uninit_log = kalloc::Box::<PersistentDebugLog>::try_new_uninit().ok()?;
            let recovered_ptr = NonNull::new(recovered_log_buffer.storage.as_mut_ptr());
            let init = PersistentDebugLog::init(recovered_ptr, recovered_log_buffer.capacity);
            // SAFETY: `uninit_log.as_mut_ptr()` points to a valid, writable heap allocation for
            // `PersistentDebugLog` that remains pinned inside `kalloc::Box`.
            unsafe {
                let _ = pin_init::PinInit::__pinned_init(init, uninit_log.as_mut_ptr());
            }
            // SAFETY: `uninit_log` was initialized in-place by `__pinned_init` above.
            let log = unsafe { uninit_log.assume_init() };
            let hdr = NonNull::new(log_buffer.storage.as_mut_ptr().cast::<LogHeader>())?;
            Some(Self { log, log_buffer, _recovered_log_buffer: recovered_log_buffer, hdr })
        }

        fn set_location(&mut self) {
            // SAFETY: `self.log_buffer.storage` is a valid heap buffer of
            // `self.log_buffer.capacity` bytes that outlives `self.log`.
            unsafe {
                self.log.set_location(
                    self.log_buffer.storage.as_mut_ptr(),
                    self.log_buffer.capacity as usize,
                );
            }
        }

        fn hdr(&self) -> &LogHeader {
            // SAFETY: `self.hdr` points to the start of `self.log_buffer.storage` (which has at
            // least `size_of::<LogHeader>()` bytes, and `LogHeader` has alignment 1).
            unsafe { self.hdr.as_ref() }
        }

        fn hdr_mut(&mut self) -> &mut LogHeader {
            // SAFETY: `self.hdr` points to the start of `self.log_buffer.storage` with exclusive
            // access via `&mut self`.
            unsafe { self.hdr.as_mut() }
        }
    }

    fn check_recovered_log_is_empty(log: &PersistentDebugLog) -> bool {
        (unittest::subtest!(|log: &PersistentDebugLog| {
            assert_true!(log.recovered_log().is_none());
        }))(log)
    }

    fn check_recovered_log_matches(log: &PersistentDebugLog, test_vectors: &[&[u8]]) -> bool {
        (unittest::subtest!(|log: &PersistentDebugLog, test_vectors: &[&[u8]]| {
            // Check that our string view representation of the recovered log matches the
            // test vectors.
            let recovered = unwrap_some!(log.recovered_log());
            let mut offset = 0usize;
            for test_vector in test_vectors {
                let log_amt = recovered.len() - offset;
                assert_ge!(log_amt, test_vector.len());
                assert_true!(&recovered[offset..offset + test_vector.len()] == *test_vector);
                offset += test_vector.len();
            }
            assert_eq!(recovered.len(), offset);
        }))(log, test_vectors)
    }

    /// Tests basic persistent debuglog initialization, writes, and crashlog recovery.
    #[test]
    fn pdlog_basic_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));

        // Our allocated buffer starts filled with zeros.  The log header should
        // consider this to be an invalid value.
        assert_false!(env.hdr().is_magic_valid());
        assert_eq!(0u32, env.hdr().magic);

        // Set the location of the persistent log ram.  This will attempt to recover
        // the (currently empty) log, and initialize the active log's header in the
        // process.
        env.set_location();
        assert_true!(env.hdr().is_magic_valid());
        assert_eq!(0u32, env.hdr().rd_ptr);

        // The recovered log should be empty.
        assert_true!(check_recovered_log_is_empty(&env.log));

        // Perform some writes to the log
        const TEST_STRINGS: [&[u8]; 4] =
            [b"Test pattern 1\n", b"This has no newline", b"ABCDEF0123456789\n", b"Foo Bar Baz\n"];

        let mut expected_rd_ptr = 0u32;
        for sv in TEST_STRINGS {
            env.log.write(sv);
            expected_rd_ptr += sv.len() as u32;
        }

        // The recovered log should be empty.
        assert_true!(check_recovered_log_is_empty(&env.log));

        // But the read pointer should have advanced to match the length of the
        // strings we have written so far.
        assert_eq!(expected_rd_ptr, env.hdr().rd_ptr);

        // Simulate a reboot by resetting our debug log, then setting the location of
        // our persistent buffer once again.  This will cause the implementation to
        // attempt to recover our log, at which point in time we can verify that
        // everything recovered correctly.
        env.log.force_reset();
        env.set_location();
        assert_true!(check_recovered_log_matches(&env.log, &TEST_STRINGS));
    }

    /// Tests persistent debuglog recovery when writes wrap around the ring buffer.
    #[test]
    fn pdlog_logwrap_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();

        // Perform enough writes to the log that it wraps at least once.
        const TEST_STR: &[u8] = b"0123456789AB\n";
        const REPEAT_COUNT: usize = 23;
        for _ in 0..REPEAT_COUNT {
            env.log.write(TEST_STR);
        }

        // "reboot" and recover the log.
        env.log.force_reset();
        env.set_location();

        // We have 120 bytes worth of payload, and our test string is 13 bytes long
        // and repeated 23 times. So, the total number of bytes we write into the
        // buffer is 299 meaning that we will wrap the buffer two complete times.  In
        // addition, our recovered log will start 120 % 13 == 3 bytes from the end of
        // an instance of our test string.
        // Update test vectors to match change in header size!
        zr::static_assert!(core::mem::size_of::<LogHeader>() == 8);
        zr::static_assert!(TEST_STR.len() == 13);
        let test_strings: [&[u8]; 10] = [
            &TEST_STR[TEST_STR.len() - 3..],
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
            TEST_STR,
        ];
        assert_true!(check_recovered_log_matches(&env.log, &test_strings));
    }

    // We expect that all embedded nulls get removed from strings when the log is
    // recovered.
    /// Tests that embedded null bytes are stripped when recovering the persistent debuglog.
    #[test]
    fn pdlog_zeros_removed_test() {
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();

        // Perform some writes to the log which have 0s embedded in them.
        const WITH_NULLS: [&[u8]; 2] = [b"This \0has\0nulls\n", b"\0\0even\0more\0nulls\0\0\0\n"];
        for sv in WITH_NULLS {
            env.log.write(sv);
        }

        // "reboot" and recover the log.
        env.log.force_reset();
        env.set_location();

        // Verify that the nulls are removed during recovery.
        const WITHOUT_NULLS: [&[u8]; 2] = [b"This hasnulls\n", b"evenmorenulls\n"];
        assert_true!(check_recovered_log_matches(&env.log, &WITHOUT_NULLS));
    }

    /// Tests that persistent debuglog recovery rejects a corrupted header magic value.
    #[test]
    fn pdlog_rejects_bad_magic_test() {
        // Set up a log, put some data into it, then "reboot"
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();
        env.log.write(b"I'm in your base, corrupting your magic numbers!\n");
        env.log.force_reset();

        // Before we attempt to recover the log, deliberately corrupt the magic
        // number.
        env.hdr_mut().magic = env.hdr().magic.wrapping_add(1);

        // Now attempt to recover the log, and verify that we get nothing.
        env.set_location();
        assert_true!(check_recovered_log_is_empty(&env.log));
    }

    /// Tests that persistent debuglog recovery rejects an out-of-bounds header read pointer.
    #[test]
    fn pdlog_rejects_bad_rd_ptr_test() {
        // Set up a log, put some data into it, then "reboot"
        let mut env = unwrap_some!(TestEnvironment::setup(128, 128));
        env.set_location();
        env.log.write(b"I'm in your base, corrupting your read pointer!\n");
        env.log.force_reset();

        // Before we attempt to recover the log, set the read pointer of the log to
        // something impossible.
        env.hdr_mut().rd_ptr = 0x12356;

        // Now attempt to recover the log, and verify that we get nothing.
        env.set_location();
        assert_true!(check_recovered_log_is_empty(&env.log));
    }

    /// Heap-allocated `PersistentDebuglogGlobals<N>` test fixture freed on drop. Use `N == 0` to
    /// simulate a kernel built with `TARGET_PERSISTENT_DEBUGLOG_SIZE == 0`.
    struct RuntimeGlobals<const N: usize>(NonNull<PersistentDebuglogGlobals<N>>);

    impl<const N: usize> RuntimeGlobals<N> {
        fn setup() -> Option<Self> {
            let globals = kalloc::Box::try_new(PersistentDebuglogGlobals::<N>::new()).ok()?;
            // Keep the pointer returned by `into_raw` (rather than one derived from a shared
            // reference) so that `drop` can hand it back to `Box::from_raw` with its original
            // provenance.
            let globals = Self(NonNull::new(kalloc::Box::into_raw(globals))?);
            // SAFETY: `globals` was just allocated and is not shared with any other thread, so
            // this is its only `init_early` call and runs before any other method on it.
            unsafe { globals.get().init_early() };
            Some(globals)
        }

        /// Returns the globals. The reference must not be used after `self` is dropped.
        fn get(&self) -> &'static PersistentDebuglogGlobals<N> {
            // SAFETY: `self.0` points to a live allocation that is only freed in `drop`, and
            // the tests below do not use a returned reference after dropping `self`.
            unsafe { self.0.as_ref() }
        }
    }

    impl<const N: usize> Drop for RuntimeGlobals<N> {
        fn drop(&mut self) {
            // SAFETY: `self.0` is the pointer returned by `kalloc::Box::into_raw` in `setup`,
            // and no reference from `get` is used after this. The leaked recovery buffer (if
            // any) is intentionally not freed.
            drop(unsafe { kalloc::Box::from_raw(self.0.as_ptr()) });
        }
    }

    /// Tests that runtime-enabled pdlog leaves NVRAM intact until heap init recovers it.
    #[test]
    fn pdlog_deferred_heap_recovery_test() {
        let mut log_buffer = unwrap_some!(Buffer::setup(128));
        let log_ptr = log_buffer.storage.as_mut_ptr();
        let log_len = log_buffer.capacity as usize;
        let hdr = log_ptr.cast::<LogHeader>();
        // SAFETY: `hdr` points to the start of `log_buffer`, which outlives all uses below, and
        // `LogHeader` has alignment 1.
        let read_hdr = || unsafe { hdr.read() };

        // First "boot": nothing is written to the location before the heap hook.
        {
            let runtime = unwrap_some!(RuntimeGlobals::<0>::setup());
            let globals = runtime.get();
            // SAFETY: `log_buffer` is valid for `log_len` bytes and outlives `globals`.
            unsafe { globals.set_location(log_ptr, log_len) };
            globals.write(b"dropped before the heap\n");
            assert_false!(read_hdr().is_magic_valid());
            assert_true!(globals.recovered_log().is_none());

            // SAFETY: Single-threaded test.
            unsafe { globals.init_heap() };
            assert_true!(read_hdr().is_magic_valid());
            assert_eq!(0u32, read_hdr().rd_ptr);
            assert_true!(globals.recovered_log().is_none());

            globals.write(b"Test pattern 1\n");
            globals.write(b"Foo Bar Baz\n");
        }

        // Second "boot": the previous log survives `set_location` and is recovered on the heap.
        let rd_ptr_before = read_hdr().rd_ptr;
        {
            let runtime = unwrap_some!(RuntimeGlobals::<0>::setup());
            let globals = runtime.get();
            // SAFETY: log_buffer is valid for log_len bytes and outlives globals.
            unsafe { globals.set_location(log_ptr, log_len) };
            assert_true!(read_hdr().is_magic_valid());
            assert_eq!(rd_ptr_before, read_hdr().rd_ptr);

            // SAFETY: Single-threaded test.
            unsafe { globals.init_heap() };
            let recovered = unwrap_some!(globals.recovered_log());
            assert_true!(recovered == b"Test pattern 1\nFoo Bar Baz\n");
            assert_eq!(0u32, read_hdr().rd_ptr);

            // A graceful reboot invalidates the log.
            globals.invalidate();
            assert_false!(read_hdr().is_magic_valid());
        }

        // Third "boot": nothing to recover after the graceful reboot.
        {
            let runtime = unwrap_some!(RuntimeGlobals::<0>::setup());
            let globals = runtime.get();
            // SAFETY: log_buffer is valid for log_len bytes and outlives globals.
            unsafe { globals.set_location(log_ptr, log_len) };
            // SAFETY: Single-threaded test.
            unsafe { globals.init_heap() };
            assert_true!(globals.recovered_log().is_none());
        }
    }

    /// Tests that a runtime log larger than the static recovery buffer is deferred to the heap.
    #[test]
    fn pdlog_oversized_runtime_log_deferred_test() {
        let mut log_buffer = unwrap_some!(Buffer::setup(128));
        let log_ptr = log_buffer.storage.as_mut_ptr();
        let log_len = log_buffer.capacity as usize;
        let hdr = log_ptr.cast::<LogHeader>();
        // SAFETY: `hdr` points to the start of `log_buffer`, which outlives all uses below, and
        // `LogHeader` has alignment 1.
        let read_hdr = || unsafe { hdr.read() };
        // SAFETY: As above; nothing else accesses `log_buffer` while this runs.
        unsafe {
            hdr.write(LogHeader { magic: LogHeader::MAGIC, rd_ptr: 15 });
            core::ptr::copy_nonoverlapping(b"Test pattern 1\n".as_ptr(), log_ptr.add(8), 15);
        }

        // A 16 byte static buffer cannot hold the 120 byte payload.
        let runtime = unwrap_some!(RuntimeGlobals::<16>::setup());
        let globals = runtime.get();
        // SAFETY: `log_buffer` is valid for `log_len` bytes and outlives `globals`.
        unsafe { globals.set_location(log_ptr, log_len) };
        assert_eq!(15u32, read_hdr().rd_ptr);
        assert_true!(globals.recovered_log().is_none());

        // SAFETY: Single-threaded test.
        unsafe { globals.init_heap() };
        let recovered = unwrap_some!(globals.recovered_log());
        assert_true!(recovered == b"Test pattern 1\n");
        assert_eq!(0u32, read_hdr().rd_ptr);
    }

    // Verify clamped heap recovery strips embedded nulls and keeps the newest trailing bytes up to
    // the allocation limit, including across a ring wrap.
    /// Tests clamped heap recovery in `init_heap_with_limit`.
    #[test]
    fn pdlog_clamped_heap_recovery_window_test() {
        let mut log_buffer = unwrap_some!(Buffer::setup(64));
        let log_ptr = log_buffer.storage.as_mut_ptr();
        let log_len = log_buffer.capacity as usize;

        // First boot: write a message with embedded nulls and wrap across the 56-byte payload.
        {
            let runtime = unwrap_some!(RuntimeGlobals::<0>::setup());
            let globals = runtime.get();
            // SAFETY: `log_buffer` is valid for `log_len` bytes and outlives `globals`.
            unsafe { globals.set_location(log_ptr, log_len) };
            // SAFETY: Single-threaded test.
            unsafe { globals.init_heap() };
            globals.write(b"0123456789abcdef0123456789abcdef0123456789\n");
            globals.write(b"Tail\0kept!\n");
        }

        // Second boot: clamp the recovery buffer to 15 bytes (`"789\nTail\0kept!\n"`), verifying
        // that `init_heap_with_limit` recovers the 14 non-zero trailing bytes across the ring wrap.
        {
            let runtime = unwrap_some!(RuntimeGlobals::<0>::setup());
            let globals = runtime.get();
            // SAFETY: `log_buffer` is valid for `log_len` bytes and outlives `globals`.
            unsafe { globals.set_location(log_ptr, log_len) };
            // SAFETY: Single-threaded test.
            unsafe { globals.init_heap_with_limit(15) };
            let recovered = unwrap_some!(globals.recovered_log());
            assert_true!(recovered == b"789\nTailkept!\n");
        }
    }
}
