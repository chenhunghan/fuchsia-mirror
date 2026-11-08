// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Para-virtualized (KVM) clock support for guests.
//!
//! # Concurrent accesses to guest memory
//!
//! The structures written here live in guest memory at guest-chosen addresses. The guest may
//! access them concurrently, and a guest that does not follow the KVM ABI may point several VCPUs'
//! structures at the same or overlapping memory, so writes from different host VCPU threads may
//! also race. The host only ever writes these structures with single plain (or, for an aligned
//! `version` field, relaxed atomic) stores and never reads them back, so such races can only
//! corrupt the values the guest observes. No Rust reference to guest memory is ever created,
//! except for a temporary `&AtomicU32` when `version` is aligned.

use core::arch::x86_64::_rdtsc;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, Ordering, fence};
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_vaddr_t;

use crate::arch_rs::x86::pv::{KVM_SYSTEM_TIME_STABLE, pv_clock_boot_time, pv_clock_system_time};
use crate::hypervisor::aspace::{GuestPhysicalAspace, GuestPtr};
use crate::platform_rs::timer::{current_boot_time, current_mono_time, ticks_per_second, zx_sec};

/// Stores the para-virtualized clock for a virtual CPU.
///
/// System time is time since boot time, and boot time is some fixed point in the
/// past.
///
/// This mirrors the layout of the C++ `PvClockState` in `<arch/hypervisor.h>`, which embeds it in
/// `Vcpu`.
#[repr(C)]
pub struct PvClockState {
    is_stable: bool,
    version: u32,
    /// Safety invariant: `system_time` is either `None` or points to a `pv_clock_system_time`
    /// within the mapping owned by `guest_ptr`.
    system_time: Option<NonNull<pv_clock_system_time>>,
    guest_ptr: GuestPtr,
}

static_assert!(offset_of!(PvClockState, is_stable) == 0);
static_assert!(offset_of!(PvClockState, version) == 4);
static_assert!(offset_of!(PvClockState, system_time) == 8);
static_assert!(offset_of!(PvClockState, guest_ptr) == 16);
static_assert!(size_of::<PvClockState>() == 56);
static_assert!(align_of::<PvClockState>() == 8);

/// This structure contains mapping between TSC and host wall time at some point
/// in time. KVM has a hypercall that asks the VMM to populate this structure and
/// it's actually used, which is rather puzzling considering that PV clock
/// provides an API to get wall time at the time of boot and offset from that time
/// which seem to be enough.
///
/// More detailed description of KVM API is available here:
///  https://www.kernel.org/doc/Documentation/virtual/kvm/hypercalls.txt
#[repr(C, packed)]
struct PvClockOffset {
    sec: u64,
    nsec: u64,
    tsc: u64,
    flags: u32,
    unused: [u32; 9],
}

static_assert!(size_of::<PvClockOffset>() == 64);
static_assert!(align_of::<PvClockOffset>() == 1);

fn calculate_scale_factor(tsc_freq: u64) -> (u32, i8) {
    // Guests convert TSC ticks to nanoseconds using this formula:
    //   ns = #TSCticks * mul * 2^(shift - 32).
    // mul * 2^(shift - 32) is a fractional number used as a scale factor in conversion.
    // It's very similar to how floating point numbers are usually represented in memory.
    const TARGET_FREQ: u64 = 1_000_000_000;

    debug_assert!(tsc_freq != 0);

    // We maintain the following invariant: 2^(exponent - 32) * x/y ~ TARGET_FREQ / tsc_freq,
    let mut exponent: i8 = 32;
    let mut x: u64 = TARGET_FREQ;
    let mut y: u64 = tsc_freq;

    // First make y small enough so that (y << 31) doesn't overflow in the next step. Adjust
    // exponent along the way to maintain invariant.
    while y >= (1u64 << 31) {
        y >>= 1;
        exponent -= 1;
    }

    // We scale x/y multiplying x by 2 until it gets big enough or we run out of bits.
    while x < (y << 31) && (x & (1u64 << 63)) == 0 {
        x <<= 1;
        exponent -= 1;
    }

    // Though it's very unlikely let's also consider a situation when x/y is still too small.
    while x < y {
        y >>= 1;
        exponent += 1;
    }

    // Finally make sure that x/y fits within 32 bits.
    while x >= (y << 32) {
        x >>= 1;
        exponent += 1;
    }

    let shift = exponent;
    let mul = (x / y) as u32;
    (mul, shift)
}

/// Updates guest boot time.
pub fn pv_clock_update_boot_time(
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    // Zircon does not maintain a UTC or local time to set a meaningful boot time
    // hence the value is fixed at zero.
    let guest_ptr = gpa.create_guest_ptr(
        guest_paddr,
        size_of::<pv_clock_boot_time>(),
        b"pv_clock-boot-time-guest-mapping",
    )?;
    let boot_time = guest_ptr.as_ptr::<pv_clock_boot_time>();
    if boot_time.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `boot_time` is non-null, so `guest_ptr` keeps `size_of::<pv_clock_boot_time>()`
    // writable bytes at it mapped and pinned until it is dropped. The bytes are written as `u8`,
    // which has no alignment requirement, because the guest chooses `guest_paddr`. See the
    // module documentation for concurrent accesses to guest memory.
    unsafe { ptr::write_bytes(boot_time.cast::<u8>(), 0, size_of::<pv_clock_boot_time>()) };
    Ok(())
}

/// Remembers guest physical address for KVM clock system time structure and enables updates
/// to guest system time.
pub fn pv_clock_reset_clock(
    pv_clock: &mut PvClockState,
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    let guest_ptr = gpa.create_guest_ptr(
        guest_paddr,
        size_of::<pv_clock_system_time>(),
        b"pv_clock-system-time-guest-mapping",
    )?;
    let Some(system_time) = NonNull::new(guest_ptr.as_ptr::<pv_clock_system_time>()) else {
        pv_clock.system_time = None;
        return Err(Status::INVALID_ARGS);
    };
    // SAFETY: `system_time` is non-null, so `guest_ptr` keeps
    // `size_of::<pv_clock_system_time>()` writable bytes at it mapped and pinned until it is
    // dropped. The bytes are written as `u8`, which has no alignment requirement, because the
    // guest chooses `guest_paddr`. See the module documentation for concurrent accesses to guest
    // memory.
    unsafe {
        ptr::write_bytes(system_time.as_ptr().cast::<u8>(), 0, size_of::<pv_clock_system_time>())
    };
    // Clear any previous `system_time` pointer before replacing `guest_ptr` (which drops the old
    // mapping), then publish the new `system_time`, so the `PvClockState::system_time` invariant
    // holds throughout.
    pv_clock.system_time = None;
    pv_clock.guest_ptr = guest_ptr;
    pv_clock.system_time = Some(system_time);
    Ok(())
}

/// Updates guest system time. If updates disabled does nothing.
pub fn pv_clock_update_system_time(pv_clock: &mut PvClockState) {
    let Some(system_time) = pv_clock.system_time else {
        return;
    };

    let (tsc_mul, tsc_shift) = calculate_scale_factor(ticks_per_second() as u64);

    // See the comment for the `pv_clock_boot_time` structure in arch/x86/src/pv.rs.
    let system_time = system_time.as_ptr();
    // SAFETY: By the `PvClockState::system_time` invariant, `system_time` points to a
    // `pv_clock_system_time` within the mapping kept alive by `pv_clock.guest_ptr`, so projecting
    // to its `version` field stays in bounds. No reference is created, so no alignment is
    // required.
    let guest_version = unsafe { &raw mut (*system_time).version };
    // Relaxed is sufficient: the sequentially consistent fences below order the version stores
    // with respect to the stores of the other fields, as in a seqlock writer. This only holds
    // when `version` is aligned; see `store_guest_version`.
    // SAFETY: As above, `guest_version` is valid for both reads and writes of a `u32` for the
    // duration of the call. The address is guest-controlled, so `store_guest_version` checks its
    // alignment rather than assuming it.
    unsafe { store_guest_version(guest_version, pv_clock.version.wrapping_add(1)) };
    fence(Ordering::SeqCst);
    let now = current_mono_time().0 as u64;
    // SAFETY: `_rdtsc` is always safe to execute in ring 0 on x86_64.
    let tsc = unsafe { _rdtsc() };
    let flags = if pv_clock.is_stable { KVM_SYSTEM_TIME_STABLE } else { 0 };
    // SAFETY: As above, the fields are within the mapping kept alive by `pv_clock.guest_ptr`.
    // They are written unaligned because the guest controls the address of the structure. See
    // the module documentation for concurrent accesses to guest memory.
    unsafe {
        (&raw mut (*system_time).tsc_mul).write_unaligned(tsc_mul);
        (&raw mut (*system_time).tsc_shift).write_unaligned(tsc_shift);
        (&raw mut (*system_time).system_time).write_unaligned(now);
        (&raw mut (*system_time).tsc_timestamp).write_unaligned(tsc);
        (&raw mut (*system_time).flags).write_unaligned(flags);
    }
    fence(Ordering::SeqCst);
    // SAFETY: As above, `guest_version` is valid for both reads and writes of a `u32` for the
    // duration of the call.
    unsafe { store_guest_version(guest_version, pv_clock.version.wrapping_add(2)) };
    pv_clock.version = pv_clock.version.wrapping_add(2);
}

/// Stores `value` to the guest-shared `version` field at `ptr`.
///
/// The store is atomic with relaxed ordering when `ptr` is suitably aligned. A guest that does not
/// follow the KVM ABI's alignment requirement may place the field anywhere, in which case it is
/// written unaligned, without atomicity guarantees towards the guest.
///
/// # Safety
///
/// `ptr` must be valid for both reads and writes of a `u32` for the duration of the call. It may
/// point into guest memory that is accessed concurrently; see the module documentation.
unsafe fn store_guest_version(ptr: *mut u32, value: u32) {
    if ptr.is_aligned() {
        // SAFETY: The caller guarantees `ptr` is valid for both reads and writes of a `u32` for
        // the duration of the call, and it was just checked to be aligned. See the module
        // documentation for concurrent accesses to guest memory.
        let version = unsafe { AtomicU32::from_ptr(ptr) };
        version.store(value, Ordering::Relaxed);
    } else {
        // A well-behaved guest follows the KVM ABI and aligns the structure.
        core::hint::cold_path();
        // SAFETY: The caller guarantees `ptr` is valid for writes; `write_unaligned` has no
        // alignment requirement.
        unsafe { ptr.write_unaligned(value) };
    }
}

/// Disables updates to guest system time.
pub fn pv_clock_stop_clock(pv_clock: &mut PvClockState) {
    pv_clock.system_time = None;
    pv_clock.guest_ptr.reset();
}

/// Populates mapping between TSC and wall time per guest request. `guest_paddr` contains
/// physical address of `PvClockOffset` structure where the result should be stored.
pub fn pv_clock_populate_offset(
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_vaddr_t,
) -> Result<(), Status> {
    let guest_ptr = gpa.create_guest_ptr(
        guest_paddr,
        size_of::<PvClockOffset>(),
        b"pv_clock-offset-guest-mapping",
    )?;
    let offset = guest_ptr.as_ptr::<PvClockOffset>();
    if offset.is_null() {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `offset` is non-null, so `guest_ptr` keeps `size_of::<PvClockOffset>()` writable
    // bytes at it mapped and pinned until it is dropped. `PvClockOffset` is packed, so `offset`
    // and its fields have no alignment requirement. See the module documentation for concurrent
    // accesses to guest memory.
    unsafe { ptr::write_bytes(offset, 0, 1) };
    // Zircon does not maintain a UTC or local time. We populate offset using the
    // only time available - time since the device was powered on.
    let time = current_boot_time().0;
    // SAFETY: `_rdtsc` is always safe to execute in ring 0 on x86_64.
    let tsc = unsafe { _rdtsc() };
    // SAFETY: As above, the fields are within the mapping kept alive by `guest_ptr`, and are
    // written unaligned because `PvClockOffset` is packed.
    unsafe {
        (&raw mut (*offset).sec).write_unaligned((time / zx_sec(1)) as u64);
        (&raw mut (*offset).nsec).write_unaligned((time % zx_sec(1)) as u64);
        (&raw mut (*offset).tsc).write_unaligned(tsc);
    }
    Ok(())
}

/// Unit tests for the para-virtualized clock helpers.
#[cfg(ktest)]
#[unittest::suite(name = "x86_hypervisor_pv")]
mod tests {
    use unittest::{expect_eq, expect_le, expect_ne};

    /// Converts `ticks` to nanoseconds the way a guest does: `ticks * mul * 2^(shift - 32)`.
    fn ticks_to_ns(ticks: u64, mul: u32, shift: i8) -> u128 {
        let product = ticks as u128 * mul as u128;
        if shift >= 32 { product << (shift - 32) } else { product >> (32 - shift) }
    }

    /// Verifies `calculate_scale_factor` for frequencies with exactly known scale factors.
    #[test]
    fn calculate_scale_factor_known_values() {
        // (tsc_freq, mul, shift)
        const CASES: [(u64, u32, i8); 6] = [
            (1, 4_000_000_000, 30),
            (19_200_000, 3_495_253_333, 6),
            (1_000_000_000, 2_147_483_648, 1),
            (2_000_000_000, 2_147_483_648, 0),
            (3_000_000_000, 2_863_311_530, -1),
            (1 << 63, 4_000_000_000, -33),
        ];
        for (tsc_freq, expected_mul, expected_shift) in CASES {
            let (mul, shift) = calculate_scale_factor(tsc_freq);
            expect_eq!(expected_mul, mul);
            expect_eq!(expected_shift, shift);
        }
    }

    /// Verifies that one second of ticks converts to one second of nanoseconds.
    #[test]
    fn calculate_scale_factor_converts_one_second() {
        // `mul` is also expected to be normalized to use all 32 bits.
        const FREQS: [u64; 12] = [
            1,
            1_000,
            32_768,
            1_000_000,
            19_200_000,
            1_000_000_000,
            2_100_000_000,
            2_899_999_000,
            4_000_000_000,
            5_000_000_000,
            1 << 32,
            1 << 40,
        ];
        for tsc_freq in FREQS {
            let (mul, shift) = calculate_scale_factor(tsc_freq);
            expect_ne!(0, mul & (1 << 31));
            let ns = ticks_to_ns(tsc_freq, mul, shift);
            expect_le!(999_999_999, ns);
            expect_le!(ns, 1_000_000_000);
        }
    }

    /// Verifies that `store_guest_version` writes exactly the `u32` at any alignment.
    #[test]
    fn store_guest_version_any_alignment() {
        // A guest may place the structure at any address.
        #[repr(C, align(8))]
        struct Buffer([u8; 16]);

        const VALUE: u32 = 0x1234_5678;
        for offset in 0..8 {
            let mut buffer = Buffer([0xff; 16]);
            let ptr = buffer.0.as_mut_ptr().wrapping_add(offset).cast::<u32>();
            // SAFETY: `ptr` is within `buffer` with at least `size_of::<u32>()` bytes after it,
            // and `buffer` is not otherwise accessed during the call, so `ptr` is valid for both
            // reads and writes of a `u32` for the duration of the call.
            unsafe { store_guest_version(ptr, VALUE) };
            let mut expected = [0xff; 16];
            expected[offset..offset + size_of::<u32>()].copy_from_slice(&VALUE.to_ne_bytes());
            expect_eq!(u128::from_ne_bytes(expected), u128::from_ne_bytes(buffer.0));
        }
    }
}
