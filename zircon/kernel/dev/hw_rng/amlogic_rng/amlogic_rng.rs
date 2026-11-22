// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::arm64::periph_paddr_to_vaddr;
use crate::hw_rng::{HwRngOps, hw_rng_register};
use crate::kernel::thread::sleep_relative;
use crate::kernel::types::PAddr;
use crate::platform_rs::timer::DurationMono;
use core::ptr::with_exposed_provenance;
use explicit_memory::mandatory_memset;
use lazy_init::LazyInit;
use regio::{Mmio, MmioPtr, Ro};
use zbi::DcfgAmlogicRngDriver;

zr::static_assert_size_and_align!(DcfgAmlogicRngDriver, 24, 8);

// Mask for the bit indicating RNG status.
const AML_RNG_READY: u32 = 1;

// Register for RNG data
static RNG_DATA: LazyInit<Mmio<u32, u32, Ro>> = LazyInit::uninit();
// Register whose 1st bit indicates RNG status: 1->ready, 0->not ready.
static RNG_STATUS: LazyInit<Mmio<u32, u32, Ro>> = LazyInit::uninit();
// Hardware RNG refresh time in microsecond.
static RNG_REFRESH_INTERVAL_USEC: LazyInit<u64> = LazyInit::uninit();

// Size of each RNG draw.
const RNG_DRAW_SIZE: usize = 4;
// Max number of retry
const MAX_RETRY: usize = 10000;

/// Returns true when the status register's ready bit is set.
fn rng_is_ready() -> bool {
    (RNG_STATUS.read() & AML_RNG_READY) == 1
}

struct AmlogicRng;

impl HwRngOps for AmlogicRng {
    fn get_entropy(&self, buf: &mut [u8]) -> usize {
        let mut len = buf.len();
        let dest: *mut u8 = buf.as_mut_ptr();
        let mut total_read: usize = 0;
        let mut retry: usize = 0;

        while len > 0 {
            // Retry until RNG is ready.
            while !rng_is_ready() {
                if retry > MAX_RETRY {
                    // SAFETY: `dest` points to writable memory of `buf.len()` (`len + total_read`)
                    // bytes.
                    unsafe { mandatory_memset(dest, 0, len + total_read) };
                    return 0;
                }

                let _ = sleep_relative(DurationMono::from_micros(1));
                retry += 1;
            }

            let mut read_buf: u32 = RNG_DATA.read();
            zr::static_assert!(core::mem::size_of::<u32>() == RNG_DRAW_SIZE);

            let read_size = len.min(RNG_DRAW_SIZE);
            // SAFETY: `dest.add(total_read)` points to writable memory within `buf` of at least
            // `read_size` bytes, and `read_buf` is a valid source of `read_size` bytes. The two
            // memories do not overlap.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    core::ptr::from_ref(&read_buf).cast::<u8>(),
                    dest.add(total_read),
                    read_size,
                );
            }
            // SAFETY: `read_buf` is a live local `u32`, valid for writes of `size_of::<u32>()`
            // bytes.
            unsafe {
                mandatory_memset(
                    core::ptr::from_mut(&mut read_buf).cast::<u8>(),
                    0,
                    core::mem::size_of::<u32>(),
                );
            }

            total_read += read_size;
            len -= read_size;

            // Hardware RNG expected to be ready after an interval.
            let _ = sleep_relative(DurationMono::from_micros(*RNG_REFRESH_INTERVAL_USEC as i64));
        }
        total_read
    }
}

static DRIVER: AmlogicRng = AmlogicRng;

/// Initializes the driver.
///
/// # Safety
///
/// Must be called at most once during boot initialization, ordered against `hw_rng_*` calls as
/// [`hw_rng_register`] requires. The physical addresses in `config` must correspond to valid Amlogic
/// hardware RNG registers in the peripheral memory map.
unsafe fn amlogic_rng_init(config: &DcfgAmlogicRngDriver) {
    assert!(config.rng_data_phys != 0);
    assert!(config.rng_status_phys != 0);

    let rng_data_vaddr = periph_paddr_to_vaddr(PAddr(config.rng_data_phys as usize))
        .expect("amlogic rng data registers not in peripheral map");
    let rng_status_vaddr = periph_paddr_to_vaddr(PAddr(config.rng_status_phys as usize))
        .expect("amlogic rng status registers not in peripheral map");

    // SAFETY: `periph_paddr_to_vaddr` returns a virtual address in the peripheral memory map
    // which remains mapped for the lifetime of the kernel, and the registers are 32-bit MMIO
    // registers whose reads never trap.
    let rng_data =
        Mmio::new(unsafe { MmioPtr::<u32, Ro>::new(with_exposed_provenance(rng_data_vaddr.0)) });
    // SAFETY: As above.
    let rng_status =
        Mmio::new(unsafe { MmioPtr::<u32, Ro>::new(with_exposed_provenance(rng_status_vaddr.0)) });
    let rng_refresh_interval_usec = config.rng_refresh_interval_usec;

    assert!(!rng_data.io().as_ptr().is_null());
    assert!(!rng_status.io().as_ptr().is_null());
    assert!(rng_refresh_interval_usec > 0);

    // SAFETY: The caller guarantees one-time initialization before the driver is registered.
    unsafe {
        RNG_DATA.init(rng_data);
        RNG_STATUS.init(rng_status);
        RNG_REFRESH_INTERVAL_USEC.init(rng_refresh_interval_usec);
    }

    // SAFETY: The caller upholds `hw_rng_register`'s one-time, synchronized registration contract.
    unsafe { hw_rng_register(&DRIVER) };
}

/// C-ABI entry point invoked during platform driver handoff.
///
/// # Safety
///
/// Same as [`amlogic_rng_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn AmlogicRngInit(config: &DcfgAmlogicRngDriver) {
    // SAFETY: The caller upholds `amlogic_rng_init`'s requirements.
    unsafe { amlogic_rng_init(config) };
}
