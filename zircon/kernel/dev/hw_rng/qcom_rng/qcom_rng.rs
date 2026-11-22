// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::arm64::periph_paddr_to_vaddr;
use crate::hw_rng::{HwRngOps, hw_rng_register};
use crate::kernel::thread::sleep_relative;
use crate::kernel::types::PAddr;
use crate::platform_rs::timer::DurationMono;
use bitrs::layout;
use core::pin::Pin;
use core::ptr::{self, copy_nonoverlapping, with_exposed_provenance};
use explicit_memory::{ZeroOnDrop, mandatory_memset};
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
use pin_init::pin_init;
use regio::{MmioBank, MmioPtr, Offset, Ro};
use zbi::{DcfgQcomRng, QcomRngFlags};

zr::static_assert_size_and_align!(DcfgQcomRng, 16, 8);

#[ksync::guarded]
struct QcomRngLock {
    #[mutex]
    lock: ksync::KMutex<crate::ksync_kernel::TrackedMutex>,
}

static QCOM_RNG_LOCK: LazyInit<QcomRngLock, NoCheck> = LazyInit::uninit();

fn qcom_rng_lock_init(_level: init::LkInitLevel) {
    // SAFETY: lk init hooks run once during single-threaded early boot before `QCOM_RNG_LOCK` can
    // be acquired by `hw_rng_get_entropy` or unit tests.
    let Ok(()) = unsafe {
        Pin::static_ref(&QCOM_RNG_LOCK).init_pin(pin_init!(QcomRngLock {
            lock <- ksync::KMutex::init(),
        }))
    };
}
init::lk_init_hook!(qcom_rng_lock, qcom_rng_lock_init, init::LK_INIT_LEVEL_EARLIEST);

layout!({
    struct StatusRegister(u32);
    {
        let __ @ 31..1;
        let data_ready @ 0;
    }
});

const STATUS_REGISTER: Offset<StatusRegister, Ro> = Offset::new(0x4);

layout!({
    struct DataRegister(u32);
    {
        let data @ 31..0;
    }
});

const DATA_REGISTER: Offset<DataRegister, Ro> = Offset::new(0x0);

// Size of the register bank spanned by `DATA_REGISTER` and `STATUS_REGISTER`.
const MMIO_BANK_SIZE: usize = 0x8;

const RETRY_DELAY: DurationMono = DurationMono::from_micros(440);
const MAX_RETRIES: i32 = 5;

static mut MMIO_BASE: usize = 0;

/// # Safety
///
/// `mmio_base` must be a valid, 4-byte aligned virtual address of an `MMIO_BANK_SIZE`-byte
/// register bank that remains mapped and readable without trapping for the duration of the call.
unsafe fn get_entropy(mmio_base: usize, buf: &mut [u8]) -> usize {
    // SAFETY: `QCOM_RNG_LOCK` is initialized at `LK_INIT_LEVEL_EARLIEST`, before `get_entropy` can
    // be called.
    ksync::lock!(let _guard = unsafe { QCOM_RNG_LOCK.get_unchecked() }.lock_lock());
    let mut buffer = &mut *buf;
    let mmio = MmioBank::new(
        // SAFETY: The caller guarantees `mmio_base` is a valid, 4-byte aligned virtual address
        // of an `MMIO_BANK_SIZE`-byte register bank for the duration of the call.
        unsafe { MmioPtr::<u32, Ro>::new(with_exposed_provenance(mmio_base)) },
        MMIO_BANK_SIZE,
    );
    // RNG data that exists in the current stack frame must be scrubbed before returning, preventing
    // any sort of "leaks". `explicit_memory` ensures none of these cleanup operations are elided by
    // the compiler.
    let mut data = ZeroOnDrop::new(0u32);
    let mut retries = 0;
    while !buffer.is_empty() {
        // SAFETY: `STATUS_REGISTER` lies within the bank guaranteed valid by the caller.
        let status_register = unsafe { mmio.at(STATUS_REGISTER) }.read();
        // When the HW RNG unit runs out of data, it must produce more. This bit is set when the
        // unit has produced data, and it can be read from the `DataRegister`.
        if !status_register.data_ready() {
            if retries == MAX_RETRIES {
                // We failed to generate enough random data, scrub everything and bail out.
                let len = buf.len();
                // SAFETY: `buf` is a valid mutable slice of `len` bytes.
                unsafe {
                    mandatory_memset(buf.as_mut_ptr(), 0, len);
                }
                return 0;
            }
            retries += 1;
            let _ = sleep_relative(RETRY_DELAY);
            continue;
        }

        // SAFETY: As above, with `DATA_REGISTER` lying within the bank.
        *data = unsafe { mmio.at(DATA_REGISTER) }.read().data();
        let copy_size = buffer.len().min(size_of_val(&*data));
        // SAFETY: `data` and `buffer` are valid for `copy_size` bytes and do not overlap.
        unsafe {
            copy_nonoverlapping(ptr::from_ref(&*data).cast(), buffer.as_mut_ptr(), copy_size);
        }
        buffer = &mut buffer[copy_size..];
    }

    buf.len()
}

struct QcomRng;

impl HwRngOps for QcomRng {
    fn get_entropy(&self, buf: &mut [u8]) -> usize {
        // SAFETY: Initialized once by `qcom_rng_init` before `DRIVER` is registered, from a
        // peripheral mapping valid for the lifetime of the kernel.
        unsafe { get_entropy(MMIO_BASE, buf) }
    }
}

static DRIVER: QcomRng = QcomRng;

/// Initializes the driver.
///
/// # Safety
///
/// Must be called at most once during boot initialization, ordered against `hw_rng_*` calls as
/// [`hw_rng_register`] requires. `config.mmio_phys` must be the physical address of the Qualcomm
/// hardware RNG register bank in the peripheral memory map.
unsafe fn qcom_rng_init(config: &DcfgQcomRng) {
    // Check that the HWRNG unit has been handed off ready for use, otherwise complain and move on.
    if !config.flags.contains(QcomRngFlags::ENABLED) {
        kprint!("QCOM HWRNG: Handed off disabled.\n");
        return;
    }
    let mmio_base = periph_paddr_to_vaddr(PAddr(config.mmio_phys as usize))
        .expect("qcom rng registers not in peripheral map");
    // SAFETY: The caller guarantees one-time initialization before `DRIVER` is registered.
    unsafe { MMIO_BASE = mmio_base.0 };
    // SAFETY: The caller upholds `hw_rng_register`'s one-time, synchronized registration contract.
    unsafe { hw_rng_register(&DRIVER) };
    kprint!(
        "QCOM HWRNG: Registered {{mmio={:u},flags={:u}}}.\n",
        config.mmio_phys,
        config.flags.bits()
    );
}

/// C-ABI entry point invoked during platform driver handoff.
///
/// # Safety
///
/// Same as [`qcom_rng_init`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn QcomRngInit(config: &DcfgQcomRng) {
    // SAFETY: The caller upholds `qcom_rng_init`'s requirements.
    unsafe { qcom_rng_init(config) };
}

/// Qualcomm HW RNG kernel unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "qcom_rng")]
mod tests {
    use super::get_entropy;
    use unittest::{assert_eq, assert_true};

    /// Tests drawing random bytes from an initialized, ready hardware RNG register bank.
    #[test]
    fn test_get_entropy_ready() {
        let regs: [u32; 2] = [0xdead_beef, 1];
        let mmio_base = regs.as_ptr().expose_provenance();

        // 0-byte draw returns 0.
        let mut buf0: [u8; 0] = [];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let len0 = unsafe { get_entropy(mmio_base, &mut buf0) };
        assert_eq!(len0, 0);

        // Exact 4-byte word draw.
        let mut buf4 = [0u8; 4];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let len4 = unsafe { get_entropy(mmio_base, &mut buf4) };
        assert_eq!(len4, 4);
        assert_true!(buf4 == 0xdead_beefu32.to_ne_bytes());

        // Partial word draw (2 bytes).
        let mut buf2 = [0u8; 2];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let len2 = unsafe { get_entropy(mmio_base, &mut buf2) };
        assert_eq!(len2, 2);
        assert_true!(buf2 == 0xdead_beefu32.to_ne_bytes()[..2]);

        // Multi-word draw (6 bytes).
        let mut buf6 = [0u8; 6];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let len6 = unsafe { get_entropy(mmio_base, &mut buf6) };
        assert_eq!(len6, 6);
        let expected = [
            0xdead_beefu32.to_ne_bytes()[0],
            0xdead_beefu32.to_ne_bytes()[1],
            0xdead_beefu32.to_ne_bytes()[2],
            0xdead_beefu32.to_ne_bytes()[3],
            0xdead_beefu32.to_ne_bytes()[0],
            0xdead_beefu32.to_ne_bytes()[1],
        ];
        assert_true!(buf6 == expected);
    }

    /// Tests that exhausting retries when data is not ready scrubs the buffer to zeroes.
    #[test]
    fn test_get_entropy_not_ready_scrubs_buffer() {
        let regs: [u32; 2] = [0xdead_beef, 0];
        let mmio_base = regs.as_ptr().expose_provenance();

        let mut buf = [0xa5u8; 8];
        // SAFETY: `regs` is a live, aligned 8-byte array on the stack, satisfying `mmio_base`.
        let len = unsafe { get_entropy(mmio_base, &mut buf) };
        assert_eq!(len, 0);

        for val in buf {
            assert_eq!(val, 0);
        }
    }
}
