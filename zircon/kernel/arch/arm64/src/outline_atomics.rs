// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel unit tests and boot-test console check for the `-moutline-atomics` functions defined in
//! //zircon/kernel/arch/arm64/outline-atomics.

cfg_select! {
    console_enabled => {
        use core::ffi::c_int;

        use core::slice;
        use kprint::kprintln;
        use zx_status::Status;
        use zx_status::sys::ZX_OK;

        use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    }
}
use boot_options::Arm64Atomics;

use crate::vm::physmap::paddr_to_physmap_ptr;
use crate::vm::vm_aspace::VmAspace;

// The outline atomic functions are declared with full 64-bit arguments and return values, so
// the tests can pass garbage in the high bits of narrower arguments, as callers may. Only the
// low bits of the return values are meaningful.
type CasFn = unsafe extern "C" fn(u64, u64, *mut u8) -> u64;
type RmwFn = unsafe extern "C" fn(u64, *mut u8) -> u64;
type Cas16Fn = unsafe extern "C" fn(u128, u128, *mut u128) -> u128;

struct OutlineAtomicFn<F> {
    name: &'static str,
    f: F,
}

/// One outline atomic function, given its family, size, and order.
macro_rules! outline_atomic_fn_for {
    (@ $family:ident, $size:tt, $order:ident, $type:ty, ($($sig:tt)*) -> $ret:ty) => {{
        const NAME: &str = concat!(
            "__aarch64_",
            stringify!($family),
            stringify!($size),
            "_",
            stringify!($order)
        );
        unsafe extern "C" {
            #[link_name = concat!(
                "__aarch64_",
                stringify!($family),
                stringify!($size),
                "_",
                stringify!($order)
            )]
            fn f($($sig)*) -> $ret;
        }
        OutlineAtomicFn::<$type> { name: NAME, f: f as $type }
    }};
    (cas, 16, $order:ident) => {
        outline_atomic_fn_for!(@ cas, 16, $order, Cas16Fn, (expected: u128, desired: u128, ptr: *mut u128) -> u128)
    };
    (cas, $size:tt, $order:ident) => {
        outline_atomic_fn_for!(@ cas, $size, $order, CasFn, (expected: u64, desired: u64, ptr: *mut u8) -> u64)
    };
    ($family:ident, $size:tt, $order:ident) => {
        outline_atomic_fn_for!(@ $family, $size, $order, RmwFn, (value: u64, ptr: *mut u8) -> u64)
    };
}

/// All the orders of one family and size.
macro_rules! orders {
    ($family:ident, $size:tt) => {
        [
            outline_atomic_fn_for!($family, $size, relax),
            outline_atomic_fn_for!($family, $size, acq),
            outline_atomic_fn_for!($family, $size, rel),
            outline_atomic_fn_for!($family, $size, acq_rel),
        ]
    };
}

/// All the sizes and orders of one family, except cas16.
macro_rules! family {
    ($family:ident) => {
        [
            (1, orders!($family, 1)),
            (2, orders!($family, 2)),
            (4, orders!($family, 4)),
            (8, orders!($family, 8)),
        ]
    };
}

/// Reads the 32-bit instruction word mapped at kernel virtual address `vaddr`.
///
/// Kernel `.text` is mapped execute-only, so instructions are read through the physmap.
fn read_kernel_insn(vaddr: usize) -> Option<u32> {
    let (paddr, _flags) = VmAspace::kernel_aspace().arch_aspace().query(vaddr).ok()?;
    // SAFETY: `paddr` is the physical address of an aligned 4-byte instruction in the static
    // kernel text image, which resides permanently in the physmap and is read-only after physboot
    // code-patching.
    Some(unsafe { paddr_to_physmap_ptr::<u32>(paddr).read() })
}

/// Inspects one outline-atomic function's patched instructions and returns which alternative
/// (`llsc/...`, `lse/...`, or `lse-a55/...`) it was patched with.
fn fn_patched_atomics(fn_addr: usize) -> Result<Arm64Atomics, &'static str> {
    const BTI_MASK: u32 = 0xffff_ff3f;
    const BTI_OPCODE: u32 = 0xd503_241f;
    const BRK_1: u32 = 0xd420_0020;
    const PRFM_MASK: u32 = 0xffff_fc1f;
    const PRFM_PSTL1KEEP: u32 = 0xf980_0010;
    const RET: u32 = 0xd65f_03c0;

    let mut addr = fn_addr;
    let mut insn0 = read_kernel_insn(addr).ok_or("failed to query kernel text address")?;
    if (insn0 & BTI_MASK) == BTI_OPCODE {
        addr += size_of::<u32>();
        insn0 = read_kernel_insn(addr).ok_or("failed to query kernel text address")?;
    }
    if insn0 == BRK_1 {
        return Err("still unpatched trap");
    }
    let insn1 =
        read_kernel_insn(addr + size_of::<u32>()).ok_or("failed to query kernel text address")?;
    // Every `lse/...` alternative in outline-atomic.S is a single atomic instruction followed
    // immediately by `ret`. Every `lse-a55/...` alternative precedes that atomic instruction with
    // `prfm pstl1keep, [xN]`, so `ret` is one instruction later. Every `llsc/...` alternative has
    // a multi-instruction loop.
    if insn1 == RET {
        return Ok(Arm64Atomics::Lse);
    }
    if (insn0 & PRFM_MASK) == PRFM_PSTL1KEEP {
        let insn2 = read_kernel_insn(addr + 2 * size_of::<u32>())
            .ok_or("failed to query kernel text address")?;
        if insn2 == RET {
            return Ok(Arm64Atomics::LseA55);
        }
        return Err("unexpected instructions after prfm pstl1keep");
    }
    Ok(Arm64Atomics::Llsc)
}

/// Checks that all 100 `__aarch64_*` functions were patched with the same alternative and returns
/// which alternative was used.
fn all_patched_atomics() -> Result<Arm64Atomics, (&'static str, &'static str)> {
    let mut expected: Option<Arm64Atomics> = None;
    let mut count = 0usize;
    let mut check = |name: &'static str, addr: usize| -> Result<(), (&'static str, &'static str)> {
        let atomics = fn_patched_atomics(addr).map_err(|err| (name, err))?;
        match expected {
            None => expected = Some(atomics),
            Some(first) if first != atomics => {
                return Err((name, "patched with different alternative than earlier functions"));
            }
            Some(_) => {}
        }
        count += 1;
        Ok(())
    };

    for (_size, by_order) in family!(cas) {
        for f in by_order {
            check(f.name, f.f as *const () as usize)?;
        }
    }
    for f in orders!(cas, 16) {
        check(f.name, f.f as *const () as usize)?;
    }
    for family_fns in [family!(swp), family!(ldadd), family!(ldclr), family!(ldeor), family!(ldset)]
    {
        for (_size, by_order) in family_fns {
            for f in by_order {
                check(f.name, f.f as *const () as usize)?;
            }
        }
    }

    assert_eq!(count, 100);
    Ok(expected.expect("at least one outline-atomic function"))
}

/// # Safety
///
/// `argv` must point to `argc` initialized `CmdArgs`, and `argc` is at least 1.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_outline_atomics(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees that `argv` points to `argc` initialized `CmdArgs`.
    let args = unsafe { slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("Usage: {:s} <lse | lse-a55 | llsc>", args[0].as_str());
        Status::INVALID_ARGS.into_raw()
    };
    let atomics_name = |atomics: Arm64Atomics| -> &'static str {
        match atomics {
            Arm64Atomics::Auto => "auto",
            Arm64Atomics::Llsc => "llsc",
            Arm64Atomics::Lse => "lse",
            Arm64Atomics::LseA55 => "lse-a55",
        }
    };

    if argc != 2 {
        return usage();
    }

    let expected = match args[1].as_str() {
        "lse" => Arm64Atomics::Lse,
        "lse-a55" => Arm64Atomics::LseA55,
        "llsc" => Arm64Atomics::Llsc,
        _ => return usage(),
    };

    match all_patched_atomics() {
        Ok(actual) => {
            if actual != expected {
                kprintln!(
                    "outline_atomics: expected {:s}, found {:s}",
                    atomics_name(expected),
                    atomics_name(actual)
                );
                return Status::BAD_STATE.into_raw();
            }
            kprintln!("outline_atomics: verified {:s}", atomics_name(actual));
            ZX_OK
        }
        Err((name, reason)) => {
            kprintln!("outline_atomics: {:s}: {:s}", name, reason);
            Status::INTERNAL.into_raw()
        }
    }
}

#[cfg(console_enabled)]
static_command!(
    CMD_OUTLINE_ATOMICS,
    c"outline_atomics".as_ptr(),
    c"verify patched arm64 outline atomics implementation (lse | lse-a55 | llsc)".as_ptr(),
    cmd_outline_atomics,
    CMD_AVAIL_NORMAL
);

/// Tests for the outline atomic functions.
#[unittest::suite(name = "arm64_outline_atomics")]
mod tests {
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use kprint::kprintln;
    use unittest::{assert_eq, assert_true, expect_eq, subtest};

    use super::{OutlineAtomicFn, RmwFn, all_patched_atomics};
    use crate::kernel::mp::{MpIpiTarget, get_online_mask, sync_exec};

    /// The bytes around the value being tested, which should not be touched.
    const GUARD: u8 = 0xa5;

    /// Garbage for the high bits of arguments narrower than 64 bits.
    const GARBAGE: u64 = 0xdead_beef_cafe_f00d;

    /// A buffer with the value being tested in the middle of guard bytes.
    #[repr(C, align(16))]
    struct Buffer([u8; 48]);

    impl Buffer {
        const OFFSET: usize = 16;

        fn new(size: usize, value: u128) -> Self {
            let mut buffer = Self([GUARD; 48]);
            buffer.0[Self::OFFSET..][..size].copy_from_slice(&value.to_le_bytes()[..size]);
            buffer
        }

        fn ptr(&mut self) -> *mut u8 {
            self.0[Self::OFFSET..].as_mut_ptr()
        }

        fn value(&self, size: usize) -> u128 {
            let mut bytes = [0; 16];
            bytes[..size].copy_from_slice(&self.0[Self::OFFSET..][..size]);
            u128::from_le_bytes(bytes)
        }

        fn guards_intact(&self, size: usize) -> bool {
            self.0[..Self::OFFSET].iter().all(|&b| b == GUARD)
                && self.0[Self::OFFSET + size..].iter().all(|&b| b == GUARD)
        }
    }

    fn mask(size: usize) -> u64 {
        if size == 8 { u64::MAX } else { (1 << (size * 8)) - 1 }
    }

    /// Adds garbage to the bits of `value` above `size` bytes.
    fn with_garbage(size: usize, value: u64) -> u64 {
        (value & mask(size)) | (GARBAGE & !mask(size))
    }

    /// Initial values and operands to test with, chosen so that adds carry out of the top bit and
    /// the bitwise operations see a mix of bits.
    const RMW_CASES: [(u64, u64); 3] =
        [(0, 0x0123_4567_89ab_cdef), (0xf0f0_f0f0_f0f0_f0f0, 0xff00_ff00_ff00_ff00), (u64::MAX, 3)];

    /// Tests that all 100 functions were patched consistently.
    #[test]
    fn patched() {
        let res = all_patched_atomics();
        if let Err((name, reason)) = res {
            kprintln!("{:s}: {:s}", name, reason);
        }
        assert_true!(res.is_ok());
    }

    /// Tests every size and order of one read-modify-write family, given the expected new value
    /// for an old value and an operand.
    fn test_rmw_family(
        atomic_fns: [(usize, [OutlineAtomicFn<RmwFn>; 4]); 4],
        op: fn(u64, u64) -> u64,
    ) -> bool {
        let mut ok = true;
        for (size, by_order) in &atomic_fns {
            let size = *size;
            for atomic_fn in by_order {
                for (old, value) in RMW_CASES {
                    let old = old & mask(size);
                    let case = subtest!(|old: u64, value: u64| {
                        let mut buffer = Buffer::new(size, old.into());
                        // SAFETY: The pointer is to `size` bytes in `buffer` that are aligned to
                        // `size`.
                        let result =
                            unsafe { (atomic_fn.f)(with_garbage(size, value), buffer.ptr()) };
                        expect_eq!(old, result & mask(size), atomic_fn.name);
                        expect_eq!(
                            u128::from(op(old, value) & mask(size)),
                            buffer.value(size),
                            atomic_fn.name
                        );
                        assert_true!(buffer.guards_intact(size), atomic_fn.name);
                    });
                    if !case(old, value) {
                        kprintln!("{:s} failed", atomic_fn.name);
                        ok = false;
                    }
                }
            }
        }
        ok
    }

    /// Tests __aarch64_cas{1,2,4,8}_*.
    #[test]
    fn cas() {
        let atomic_fns = family!(cas);
        let mut ok = true;
        for (size, by_order) in &atomic_fns {
            let size = *size;
            for atomic_fn in by_order {
                for (old, new) in RMW_CASES {
                    let old = old & mask(size);
                    let new = new & mask(size);
                    let case = subtest!(|old: u64, new: u64| {
                        // A match, with garbage in the high bits of the arguments.
                        let mut buffer = Buffer::new(size, old.into());
                        // SAFETY: The pointer is to `size` bytes in `buffer` that are aligned to
                        // `size`.
                        let result = unsafe {
                            (atomic_fn.f)(
                                with_garbage(size, old),
                                with_garbage(size, new),
                                buffer.ptr(),
                            )
                        };
                        expect_eq!(old, result & mask(size), atomic_fn.name);
                        expect_eq!(u128::from(new), buffer.value(size), atomic_fn.name);
                        assert_true!(buffer.guards_intact(size), atomic_fn.name);

                        // A mismatch only in the top bit of the value.
                        let mut buffer = Buffer::new(size, old.into());
                        let expected = old ^ (1 << (size * 8 - 1));
                        // SAFETY: As above.
                        let result = unsafe {
                            (atomic_fn.f)(
                                with_garbage(size, expected),
                                with_garbage(size, new),
                                buffer.ptr(),
                            )
                        };
                        expect_eq!(old, result & mask(size), atomic_fn.name);
                        expect_eq!(u128::from(old), buffer.value(size), atomic_fn.name);
                        assert_true!(buffer.guards_intact(size), atomic_fn.name);
                    });
                    if !case(old, new) {
                        kprintln!("{:s} failed", atomic_fn.name);
                        ok = false;
                    }
                }
            }
        }
        assert_true!(ok);
    }

    /// Tests __aarch64_cas16_*.
    #[test]
    fn cas16() {
        let atomic_fns = orders!(cas, 16);
        const CASES: [(u128, u128); 3] = [
            (0, 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210),
            (u128::MAX, 1),
            (0x8000_0000_0000_0000_0000_0000_0000_0001, 0x7fff_ffff_ffff_ffff_ffff_ffff_ffff_fffe),
        ];
        let mut ok = true;
        for atomic_fn in &atomic_fns {
            for (old, new) in CASES {
                let case = subtest!(|old: u128, new: u128| {
                    // A match.
                    let mut buffer = Buffer::new(16, old);
                    // SAFETY: The pointer is to 16 bytes in `buffer` aligned to 16.
                    let result = unsafe { (atomic_fn.f)(old, new, buffer.ptr().cast()) };
                    expect_eq!(old, result, atomic_fn.name);
                    expect_eq!(new, buffer.value(16), atomic_fn.name);
                    assert_true!(buffer.guards_intact(16), atomic_fn.name);

                    // Mismatches in just the low half and in just the high half.
                    for expected in [old ^ 1, old ^ (1 << 127)] {
                        let mut buffer = Buffer::new(16, old);
                        // SAFETY: As above.
                        let result = unsafe { (atomic_fn.f)(expected, new, buffer.ptr().cast()) };
                        expect_eq!(old, result, atomic_fn.name);
                        expect_eq!(old, buffer.value(16), atomic_fn.name);
                        assert_true!(buffer.guards_intact(16), atomic_fn.name);
                    }
                });
                if !case(old, new) {
                    kprintln!("{:s} failed", atomic_fn.name);
                    ok = false;
                }
            }
        }
        assert_true!(ok);
    }

    /// Tests __aarch64_swp*.
    #[test]
    fn swp() {
        assert_true!(test_rmw_family(family!(swp), |_, value| value));
    }

    /// Tests __aarch64_ldadd*.
    #[test]
    fn ldadd() {
        assert_true!(test_rmw_family(family!(ldadd), |old, value| old.wrapping_add(value)));
    }

    /// Tests __aarch64_ldclr*.
    #[test]
    fn ldclr() {
        assert_true!(test_rmw_family(family!(ldclr), |old, value| old & !value));
    }

    /// Tests __aarch64_ldeor*.
    #[test]
    fn ldeor() {
        assert_true!(test_rmw_family(family!(ldeor), |old, value| old ^ value));
    }

    /// Tests __aarch64_ldset*.
    #[test]
    fn ldset() {
        assert_true!(test_rmw_family(family!(ldset), |old, value| old | value));
    }

    /// The number of updates each CPU makes in the contention tests.
    const CONTENTION_ITERATIONS: u64 = 1000;

    /// How long a CPU in the contention tests waits for the others before it starts anyway.
    const MAX_ARRIVAL_SPINS: u32 = 10_000_000;

    /// Runs `body` on all the online CPUs at once and returns the number of CPUs that ran it.
    fn contend(body: impl Fn() + Sync) -> u64 {
        let online = get_online_mask();
        let expected = online.count_ones() as usize;
        let arrived = AtomicUsize::new(0);
        let ran = AtomicU64::new(0);
        // SAFETY: The target is not `MpIpiTarget::AllButLocal`.
        unsafe {
            sync_exec(MpIpiTarget::Mask, online, || {
                // Start together so that the CPUs really contend, but don't wait forever in case
                // a CPU went offline.
                arrived.fetch_add(1, Ordering::Relaxed);
                let mut spins = 0;
                while arrived.load(Ordering::Relaxed) < expected && spins < MAX_ARRIVAL_SPINS {
                    core::hint::spin_loop();
                    spins += 1;
                }
                body();
                ran.fetch_add(1, Ordering::Relaxed);
            });
        }
        ran.load(Ordering::Relaxed)
    }

    /// Tests ldadd8 and cas8 with all CPUs updating one counter concurrently.
    #[test]
    fn contention() {
        let ldadd = outline_atomic_fn_for!(ldadd, 8, acq_rel);
        let cas = outline_atomic_fn_for!(cas, 8, acq_rel);
        let counter = AtomicU64::new(0);
        let retries = AtomicU64::new(0);
        let cpus = contend(|| {
            let ptr = counter.as_ptr().cast::<u8>();
            let mut my_retries = 0;
            for _ in 0..CONTENTION_ITERATIONS {
                // SAFETY: `ptr` is to an aligned `AtomicU64` that's only accessed atomically.
                unsafe { (ldadd.f)(1, ptr) };
                let mut old = counter.load(Ordering::Relaxed);
                loop {
                    // SAFETY: As above.
                    let prev = unsafe { (cas.f)(old, old + 1, ptr) };
                    if prev == old {
                        break;
                    }
                    old = prev;
                    my_retries += 1;
                }
            }
            retries.fetch_add(my_retries, Ordering::Relaxed);
        });
        kprintln!("{} CPUs, {} compare-and-swap retries", cpus, retries.load(Ordering::Relaxed));
        assert_eq!(cpus * 2 * CONTENTION_ITERATIONS, counter.load(Ordering::Relaxed));
    }

    /// A 16-byte value shared between CPUs.
    #[repr(C, align(16))]
    struct Shared16(UnsafeCell<u128>);

    // SAFETY: The value is only accessed through the outline atomic functions.
    unsafe impl Sync for Shared16 {}

    impl Shared16 {
        fn ptr(&self) -> *mut u128 {
            self.0.get()
        }
    }

    /// Tests cas16 with all CPUs updating one value concurrently.
    #[test]
    fn contention_cas16() {
        let cas16 = outline_atomic_fn_for!(cas, 16, acq_rel);
        let value = Shared16(UnsafeCell::new(0));
        let retries = AtomicU64::new(0);
        let cpus = contend(|| {
            let ptr = value.ptr();
            let mut my_retries = 0;
            // Read the value atomically: swapping 0 for 0 changes nothing either way.
            // SAFETY: `ptr` is to an aligned `u128` that's only accessed atomically.
            let mut old = unsafe { (cas16.f)(0, 0, ptr) };
            for _ in 0..CONTENTION_ITERATIONS {
                loop {
                    // Count in both halves, so that a torn update would show.
                    let new = old.wrapping_add((1 << 64) | 1);
                    // SAFETY: As above.
                    let prev = unsafe { (cas16.f)(old, new, ptr) };
                    if prev == old {
                        old = new;
                        break;
                    }
                    old = prev;
                    my_retries += 1;
                }
            }
            retries.fetch_add(my_retries, Ordering::Relaxed);
        });
        kprintln!("{} CPUs, {} compare-and-swap retries", cpus, retries.load(Ordering::Relaxed));
        // SAFETY: The pointer is to an aligned `u128` that's only accessed atomically.
        let result = unsafe { (cas16.f)(0, 0, value.ptr()) };
        assert_eq!(cpus * CONTENTION_ITERATIONS, result as u64);
        assert_eq!(cpus * CONTENTION_ITERATIONS, (result >> 64) as u64);
    }
}
