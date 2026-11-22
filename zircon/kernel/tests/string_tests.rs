// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Interactive `memcpy` and `memset` validation and benchmark console command.

use core::ffi::{c_int, c_void};
use core::mem::{MaybeUninit, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};
use kprint::{kprint, kprintln};
use libc::string::{memcmp, memcpy, memset};

use super::{InactiveCpuGuard, opaque_const_ptr, opaque_ptr};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::thread::sleep_relative;
use crate::platform_rs::timer::{DurationMono, current_mono_time};
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::physmap::paddr_to_physmap_mut;
use crate::vm::pmm;

const BUFFER_SIZE: usize = 8 * 1024 * 1024;

/// Extra trailing bytes allocated per buffer to allow unaligned offsets up to [`MAX_ALIGN`].
const BUFFER_PADDING: usize = 256;

const BUFFER_STRIDE: usize = BUFFER_SIZE + BUFFER_PADDING;

/// Alignment sweep upper bound used by validation and benchmark loops.
const MAX_ALIGN: usize = 64;

/// Enough iterations to copy or set 1 GiB of memory.
const ITERATIONS: usize = (1024 * 1024 * 1024) / BUFFER_SIZE;

/// Total bytes copied or set per benchmark timing pass (`1 GiB`).
const TOTAL_BYTES: u64 = (BUFFER_SIZE as u64) * (ITERATIONS as u64);

// Reference implementations of memmove/memcpy.

type Word = usize;

const LSIZE: usize = size_of::<Word>();
const LMASK: usize = LSIZE - 1;

zr::static_assert!(LSIZE.is_power_of_two());
zr::static_assert!(MAX_ALIGN <= BUFFER_PADDING);

/// Volatile `Word`-sized load using [`AtomicUsize::load_volatile`] (`Ordering::Relaxed`) so LLVM
/// does not lower reference loops to `libc::memcpy`/`memmove`, and `lk_debug_level_2`
/// (`-C debug-assertions=yes`) does not emit redundant `ub_checks` alignment branches.
///
/// # Safety
///
/// The caller must ensure that `src` is non-null, aligned to `LSIZE`
/// (`align_of::<AtomicUsize>()`), valid for reads of one initialized `Word` (`usize`), and not
/// concurrently mutated for the duration of the call.
#[inline(always)]
unsafe fn read_volatile_word(src: *const Word) -> Word {
    // SAFETY: `Word` and `AtomicUsize` have identical size and alignment (`LSIZE`), and caller
    // guarantees `src` is valid for an aligned read of one initialized `Word` with no concurrent
    // writes. `Ordering::Relaxed` suffices because no cross-thread synchronization is required.
    unsafe { AtomicUsize::from_ptr_raw(src.cast_mut()).load_volatile(Ordering::Relaxed) }
}

/// Volatile `Word`-sized store using [`AtomicUsize::store_volatile`] (`Ordering::Relaxed`) so LLVM
/// does not lower reference loops to `libc::memcpy`/`memset`, and `lk_debug_level_2`
/// (`-C debug-assertions=yes`) does not emit redundant `ub_checks` alignment branches.
///
/// # Safety
///
/// The caller must ensure that `dst` is non-null, aligned to `LSIZE`
/// (`align_of::<AtomicUsize>()`), valid for writes of one `Word` (`usize`), and not
/// concurrently accessed for the duration of the call. On return, the `LSIZE` bytes at `dst` are
/// initialized to `val`.
#[inline(always)]
unsafe fn write_volatile_word(dst: *mut Word, val: Word) {
    // SAFETY: `Word` and `AtomicUsize` have identical size and alignment (`LSIZE`), and caller
    // guarantees `dst` is valid for an aligned write of one `Word` with no concurrent accesses.
    // `Ordering::Relaxed` suffices because no cross-thread synchronization is required.
    unsafe { AtomicUsize::from_ptr_raw(dst).store_volatile(val, Ordering::Relaxed) };
}

/// Disjoint physmap working buffers used by string validation and benchmark routines.
struct TestBuffers {
    /// Primary source buffer (`BUFFER_STRIDE` bytes).
    src: NonNull<u8>,
    /// Primary destination buffer (`BUFFER_STRIDE` bytes).
    dst: NonNull<u8>,
    /// Reference source buffer (`BUFFER_STRIDE` bytes) for validation comparisons.
    src2: NonNull<u8>,
    /// Reference destination buffer (`BUFFER_STRIDE` bytes) for validation comparisons.
    dst2: NonNull<u8>,
}

type MemcpyRoutine = unsafe extern "C" fn(*mut c_void, *const c_void, usize) -> *mut c_void;
type MemsetRoutine = unsafe extern "C" fn(*mut c_void, c_int, usize) -> *mut c_void;

/// Reference C-style `memmove` implementation using volatile word and byte accesses.
///
/// On `arm64`, volatile loads/stores prevent AArch64 post-index addressing, adding two pointer
/// `add`s per word/byte loop iteration (6 vs 4 instructions/word; `x64` is unaffected).
///
/// # Safety
///
/// The caller must ensure that if `count > 0`, `count <= isize::MAX as usize`, `src` is non-null
/// and valid for reads of `count` initialized bytes within a single allocated object (without
/// wrapping the address space and not concurrently mutated for the duration of the call), and
/// `dest` is non-null and valid for writes of `count` bytes within a single allocated object
/// (without wrapping the address space and not concurrently accessed for the duration of the call;
/// `[src, src + count)` and `[dest, dest + count)` may overlap). On return, all `count` bytes in
/// `[dest, dest + count)` are initialized with the bytes originally in `[src, src + count)`.
unsafe extern "C" fn c_memmove(
    dest: *mut c_void,
    src: *const c_void,
    mut count: usize,
) -> *mut c_void {
    let mut d = dest.cast::<u8>();
    let mut s = src.cast::<u8>();
    let mut len: usize;

    if count == 0 || core::ptr::eq(dest.cast_const(), src) {
        return dest;
    }

    // Loop invariants (with `orig_count` denoting the initial `count` argument):
    // - Forward path (`d.addr() < s.addr()`): `d - dest == s - src`,
    //   `(s - src) + len + count == orig_count` in the prefix byte loop,
    //   `(s - src) + len * LSIZE + (count & LMASK) == orig_count` in the word loop,
    //   `(s - src) + len == orig_count` in the trailing byte loop, and `d.addr() < s.addr()` so
    //   reading before writing preserves overlapping bytes.
    // - Backward path (`d.addr() > s.addr()`): after advancing `d` and `s` by `orig_count`,
    //   `d - dest == s - src`, `s - src == len + count` in the suffix byte loop,
    //   `s - src == len * LSIZE + (count & LMASK)` (`sw = s - LSIZE`, `dw = d - LSIZE`) in the
    //   word loop, and `s - src == len` in the leading byte loop, preserving overlapping bytes.
    if d.addr() < s.addr() {
        if ((d.addr() | s.addr()) & LMASK) != 0 {
            // src and/or dest do not align on word boundary
            if (((d.addr() ^ s.addr()) & LMASK) != 0) || (count < LSIZE) {
                len = count; // copy the rest of the buffer with the byte mover
            } else {
                len = LSIZE - (d.addr() & LMASK); // move the ptrs up to a word boundary
            }

            count -= len;
            while len > 0 {
                // SAFETY: By the forward loop invariant, `s` is in `[src, src + orig_count)` and
                // valid for an initialized volatile byte read (preventing LLVM `memcpy` lowering).
                let byte = unsafe { s.read_volatile() };
                // SAFETY: By the forward loop invariant, `d` is in `[dest, dest + orig_count)`,
                // uniquely valid for a byte write, and `d < s` preserves overlapping bytes.
                unsafe { d.write_volatile(byte) };
                d = d.wrapping_add(1);
                s = s.wrapping_add(1);
                len -= 1;
            }
        }
        len = count / LSIZE;
        if len > 0 {
            let mut dw = d.cast::<Word>();
            let mut sw = s.cast::<Word>();
            // Invariant: Since `len > 0` implies `count >= LSIZE`, the
            // `((d.addr() ^ s.addr()) & LMASK) != 0 || count < LSIZE` branch (which sets
            // `count = 0`) was not taken, and any shared misalignment was cleared by advancing
            // `LSIZE - (d.addr() & LMASK)` bytes; hence both `dw` and `sw` are `Word`-aligned
            // (`LSIZE`) and `s.addr() - d.addr() >= LSIZE` so forward word copies preserve
            // overlapping bytes.
            while len > 0 {
                // SAFETY: `sw` is `Word`-aligned and lies in `[src, src + orig_count)` for `LSIZE`
                // initialized bytes by the forward loop invariant.
                let word = unsafe { read_volatile_word(sw) };
                // SAFETY: `dw` is `Word`-aligned, lies in `[dest, dest + orig_count)` for `LSIZE`
                // writable bytes, and `s.addr() - d.addr() >= LSIZE` preserves overlapping bytes.
                unsafe { write_volatile_word(dw, word) };
                dw = dw.wrapping_add(1);
                sw = sw.wrapping_add(1);
                len -= 1;
            }
            d = dw.cast::<u8>();
            s = sw.cast::<u8>();
        }
        len = count & LMASK;
        while len > 0 {
            // SAFETY: By the forward loop invariant, `s` is in `[src, src + orig_count)` and
            // valid for an initialized volatile byte read.
            let byte = unsafe { s.read_volatile() };
            // SAFETY: By the forward loop invariant, `d` is in `[dest, dest + orig_count)`,
            // uniquely valid for a byte write, and `d < s` preserves overlapping bytes.
            unsafe { d.write_volatile(byte) };
            d = d.wrapping_add(1);
            s = s.wrapping_add(1);
            len -= 1;
        }
    } else {
        d = d.wrapping_add(count);
        s = s.wrapping_add(count);
        if ((d.addr() | s.addr()) & LMASK) != 0 {
            // src and/or dest do not align on word boundary
            if (((d.addr() ^ s.addr()) & LMASK) != 0) || (count <= LSIZE) {
                len = count;
            } else {
                len = d.addr() & LMASK;
            }

            count -= len;
            while len > 0 {
                d = d.wrapping_sub(1);
                s = s.wrapping_sub(1);
                // SAFETY: By the backward loop invariant, `s` stepped back into
                // `[src, src + orig_count)` and is valid for an initialized volatile byte read.
                let byte = unsafe { s.read_volatile() };
                // SAFETY: By the backward loop invariant, `d` stepped back into
                // `[dest, dest + orig_count)`, is uniquely valid for a byte write, and `d > s`
                // preserves overlapping bytes.
                unsafe { d.write_volatile(byte) };
                len -= 1;
            }
        }
        len = count / LSIZE;
        if len > 0 {
            let mut dw = d.wrapping_sub(LSIZE).cast::<Word>();
            let mut sw = s.wrapping_sub(LSIZE).cast::<Word>();
            // Invariant: Since `len > 0` implies `count >= LSIZE`, the
            // `((d.addr() ^ s.addr()) & LMASK) != 0 || count <= LSIZE` branch (which sets
            // `count = 0`) was not taken, and any shared misalignment was cleared by stepping back
            // `d.addr() & LMASK` bytes; hence `dw` and `sw` are `Word`-aligned (`LSIZE`) and
            // `!ptr::eq(dest, src)` with `d.addr() > s.addr()` implies `d.addr() - s.addr() >=
            // LSIZE`, so backward word copies preserve overlapping bytes.
            while len > 0 {
                // SAFETY: `sw` is `Word`-aligned and lies in `[src, src + orig_count)` for `LSIZE`
                // initialized bytes by the backward loop invariant.
                let word = unsafe { read_volatile_word(sw) };
                // SAFETY: `dw` is `Word`-aligned, lies in `[dest, dest + orig_count)` for `LSIZE`
                // writable bytes, and `d.addr() - s.addr() >= LSIZE` preserves overlapping bytes.
                unsafe { write_volatile_word(dw, word) };
                dw = dw.wrapping_sub(1);
                sw = sw.wrapping_sub(1);
                len -= 1;
            }
            d = dw.cast::<u8>().wrapping_add(LSIZE);
            s = sw.cast::<u8>().wrapping_add(LSIZE);
        }
        len = count & LMASK;
        while len > 0 {
            d = d.wrapping_sub(1);
            s = s.wrapping_sub(1);
            // SAFETY: By the backward loop invariant, `s` stepped back into
            // `[src, src + orig_count)` and is valid for an initialized volatile byte read.
            let byte = unsafe { s.read_volatile() };
            // SAFETY: By the backward loop invariant, `d` stepped back into
            // `[dest, dest + orig_count)`, is uniquely valid for a byte write, and `d > s`
            // preserves overlapping bytes.
            unsafe { d.write_volatile(byte) };
            len -= 1;
        }
    }

    dest
}

/// Reference C-style `memset` implementation using volatile word and byte stores.
///
/// On `arm64`, volatile `write_volatile_word` prevents AArch64 paired 16-byte `stp xzr, xzr` stores
/// (32 B/iter in C++ `-fno-builtin` vs unpaired 8 B/iter in Rust: ~4x instructions and ~2x store
/// ops per byte; `x64` is unaffected), so `arm64` `c` memset benchmark numbers are not directly
/// comparable to pre-port C++ results.
///
/// # Safety
///
/// The caller must ensure that if `count > 0`, `count <= isize::MAX as usize` and `s` is non-null
/// and valid for writes of `count` bytes within a single allocated object (without wrapping the
/// address space and not concurrently accessed for the duration of the call). On return, all
/// `count` bytes in `[s, s + count)` are initialized to `c as u8`.
unsafe extern "C" fn c_memset(s: *mut c_void, c: c_int, mut count: usize) -> *mut c_void {
    let mut xs = s.cast::<u8>();
    let mut len = s.addr().wrapping_neg() & LMASK;

    // Loop invariant (with `orig_count` denoting the initial `count` argument): across the
    // prefix (`if count > len`), word, and trailing byte loops, `(xs - s) + remaining ==
    // orig_count` (`remaining` is `len + count` in the prefix loop, `len * LSIZE + (count & LMASK)`
    // in the word loop, and `count` in the trailing loop), keeping `xs` in `[s, s + orig_count)`.
    if count > len {
        count -= len;
        let cc = Word::from_ne_bytes([c as u8; LSIZE]);

        // write to non-aligned memory byte-wise
        while len > 0 {
            // SAFETY: By the loop invariant, `xs` is in `[s, s + orig_count)` and uniquely valid
            // for a volatile byte write (preventing LLVM `memset` lowering).
            unsafe { xs.write_volatile(c as u8) };
            xs = xs.wrapping_add(1);
            len -= 1;
        }

        // write to aligned memory dword-wise
        len = count / LSIZE;
        while len > 0 {
            // SAFETY: Advancing `(-s.addr()) & LMASK` bytes cleared `xs`'s low `LMASK` bits, so
            // `xs.cast::<Word>()` is `Word`-aligned (`LSIZE`) and in `[s, s + orig_count)` for
            // `LSIZE` writable bytes by the loop invariant.
            unsafe { write_volatile_word(xs.cast::<Word>(), cc) };
            xs = xs.wrapping_add(LSIZE);
            len -= 1;
        }

        count &= LMASK;
    }

    // write remaining bytes
    while count > 0 {
        // SAFETY: By the loop invariant, `xs` is in `[s, s + orig_count)` and uniquely valid for
        // a volatile byte write.
        unsafe { xs.write_volatile(c as u8) };
        xs = xs.wrapping_add(1);
        count -= 1;
    }

    s
}

/// No-op `memcpy` baseline returning `dst` immediately to measure benchmark loop overhead.
extern "C" fn null_memcpy(dst: *mut c_void, _src: *const c_void, _len: usize) -> *mut c_void {
    dst
}

/// Times [`ITERATIONS`] invocations of `memcpy_routine` copying [`BUFFER_SIZE`] bytes from
/// `bufs.src + srcalign` to `bufs.dst + dstalign`, returning elapsed milliseconds.
///
/// # Safety
///
/// The caller must ensure that `dstalign + BUFFER_SIZE` and `srcalign + BUFFER_SIZE` do not
/// overflow `usize` or wrap the address space (with `BUFFER_SIZE <= isize::MAX as usize`),
/// `bufs.dst` is valid for writes of `dstalign + BUFFER_SIZE` bytes within a single allocated
/// object, `bufs.src` is valid for reads of `srcalign + BUFFER_SIZE` initialized bytes within a
/// single allocated object, the two regions do not overlap, and neither buffer is concurrently
/// accessed during the call. `memcpy_routine` must be sound to invoke whenever [`memcpy`]'s
/// `# Safety` contract holds.
unsafe fn bench_memcpy_routine<const OPAQUE: bool>(
    bufs: &TestBuffers,
    memcpy_routine: MemcpyRoutine,
    srcalign: usize,
    dstalign: usize,
) -> i64 {
    let dst_ptr = bufs.dst.as_ptr().wrapping_add(dstalign).cast::<c_void>();
    let src_ptr = bufs.src.as_ptr().wrapping_add(srcalign).cast::<c_void>().cast_const();
    let t0 = current_mono_time();
    for _ in 0..ITERATIONS {
        // When benchmarking `libc::string::memcpy`, `opaque_ptr` / `opaque_const_ptr` prevent LLVM
        // from dead-store-eliminating or hoisting repeated calls across iterations; `c_memmove`
        // uses volatile accesses, so passing pointers directly keeps them loop-invariant.
        let (dst, src) = if OPAQUE {
            (opaque_ptr(dst_ptr), opaque_const_ptr(src_ptr))
        } else {
            (dst_ptr, src_ptr)
        };
        // SAFETY: By caller precondition, `dst_ptr` and `src_ptr` (and, when `OPAQUE` is true,
        // `opaque_ptr(dst_ptr)` and `opaque_const_ptr(src_ptr)` by their
        // `# Safety-usable invariant`s) are non-null, disjoint regions within their respective
        // single allocated objects, valid for writes and initialized reads of
        // `BUFFER_SIZE <= isize::MAX as usize` bytes without wrapping the address space or
        // concurrent access/mutation, and `memcpy_routine` is sound under those conditions.
        unsafe {
            memcpy_routine(dst, src, BUFFER_SIZE);
        }
    }
    (current_mono_time() - t0).into_millis()
}

#[inline]
fn bytes_per_sec(msecs: i64) -> u64 {
    u64::try_from(msecs).ok().and_then(|ms| (TOTAL_BYTES * 1000).checked_div(ms)).unwrap_or(0)
}

/// Benchmarks `null_memcpy`, `c_memmove`, `libc::string::memcpy`, and the `my` column (a second
/// `libc::string::memcpy` pass) across aligned and unaligned source/destination offsets.
///
/// # Safety
///
/// The caller must ensure that `bufs.src` is valid for reads of [`BUFFER_STRIDE`] initialized
/// bytes within a single allocated object, `bufs.dst` is valid for writes of [`BUFFER_STRIDE`]
/// bytes within a single allocated object (both without wrapping the address space), `bufs.src` and
/// `bufs.dst` are disjoint, neither buffer is concurrently accessed during the call, the calling
/// CPU's scheduler is initialized, and its `queue_lock_` is not held.
unsafe fn bench_memcpy(bufs: &TestBuffers) {
    kprintln!("memcpy speed test");
    // let the debug string clear the serial port
    let _ = sleep_relative(DurationMono::from_millis(200));

    let mut srcalign = 0;
    while srcalign < MAX_ALIGN {
        let mut dstalign = 0;
        while dstalign < MAX_ALIGN {
            let (null, c, libc, mine) = {
                // SAFETY: The caller guarantees the current CPU's scheduler is initialized and
                // its `queue_lock_` is unheld, and nothing in this block acquires `queue_lock_` or
                // re-enables interrupts before `_inactive_cpu_guard` drops.
                let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };
                // SAFETY: `bufs.src` and `bufs.dst` satisfy `BUFFER_STRIDE` bounds/disjointness
                // for `srcalign, dstalign < MAX_ALIGN`, and `null_memcpy` is safe.
                let null =
                    unsafe { bench_memcpy_routine::<false>(bufs, null_memcpy, srcalign, dstalign) };
                // SAFETY: `srcalign < MAX_ALIGN`, `dstalign < MAX_ALIGN`, and
                // `BUFFER_SIZE <= isize::MAX as usize` keep both offsets within the disjoint
                // `bufs.src` (initialized) and `bufs.dst` (writable) objects without wrapping or
                // concurrent access, and `c_memmove` is sound for `BUFFER_SIZE` bytes.
                let c =
                    unsafe { bench_memcpy_routine::<false>(bufs, c_memmove, srcalign, dstalign) };
                // SAFETY: `srcalign < MAX_ALIGN`, `dstalign < MAX_ALIGN`, and
                // `BUFFER_SIZE <= isize::MAX as usize` keep both offsets within the disjoint
                // `bufs.src` (initialized) and `bufs.dst` (writable) objects without wrapping or
                // concurrent access, and `memcpy` is sound for `BUFFER_SIZE` disjoint bytes.
                let libc =
                    unsafe { bench_memcpy_routine::<true>(bufs, memcpy, srcalign, dstalign) };
                // SAFETY: `srcalign < MAX_ALIGN`, `dstalign < MAX_ALIGN`, and
                // `BUFFER_SIZE <= isize::MAX as usize` keep both offsets within the disjoint
                // `bufs.src` (initialized) and `bufs.dst` (writable) objects without wrapping or
                // concurrent access, and `memcpy` is sound for `BUFFER_SIZE` disjoint bytes.
                let mine =
                    unsafe { bench_memcpy_routine::<true>(bufs, memcpy, srcalign, dstalign) };
                (null, c, libc, mine)
            };

            kprint!("srcalign {}, dstalign {}: ", srcalign, dstalign);
            kprintln!("   null memcpy {} msecs", null);
            kprint!("c {} msecs, {} bytes/sec; ", c, bytes_per_sec(c));
            kprint!("libc {} msecs, {} bytes/sec; ", libc, bytes_per_sec(libc));
            kprintln!("my {} msecs, {} bytes/sec; ", mine, bytes_per_sec(mine));

            if dstalign < 8 {
                dstalign += 1;
            } else {
                dstalign <<= 1;
            }
        }
        if srcalign < 8 {
            srcalign += 1;
        } else {
            srcalign <<= 1;
        }
    }
}

/// Converts `*ptr` into a mutable fixed-size `MaybeUninit<u8>` array reference tied to
/// `ptr`'s mutable borrow lifetime.
///
/// Marked `#[inline(never)]` so `validate_memcpy` and `validate_memset` perform any `ub_checks`
/// validation once per buffer outside their validation loops rather than allowing LLVM to sink
/// diverging checks past earlier stores into the inner loops.
///
/// # Safety
///
/// The caller must ensure that the underlying allocation behind `*ptr` remains live and valid for
/// reads and writes of `N` bytes within a single allocated object (without wrapping the address
/// space) for the entire borrow lifetime of `ptr`, and that the memory behind `*ptr` is not
/// accessed through any other pointer for that entire borrow lifetime.
#[inline(never)]
unsafe fn uninit_array_mut<const N: usize>(ptr: &mut NonNull<u8>) -> &mut [MaybeUninit<u8>; N] {
    // SAFETY: `*ptr` is non-null (`NonNull<u8>`), `MaybeUninit<u8>` has 1-byte alignment and
    // imposes no initialization requirement, and the caller guarantees the underlying allocation
    // behind `*ptr` remains live and valid for reads and writes of `N` bytes within a single
    // allocated object with no other accesses for the entire borrow lifetime of `ptr`.
    unsafe { &mut *ptr.as_ptr().cast::<[MaybeUninit<u8>; N]>() }
}

/// Fills `buf` with a deterministic pseudo-random byte sequence seeded by `seed`.
///
/// # Safety-usable invariant
///
/// On return, every byte in `buf[0..buf.len()]` is initialized.
fn fillbuf(buf: &mut [MaybeUninit<u8>], mut seed: u32) {
    // Index explicitly rather than iterating with `IterMut` so `lk_debug_level_2` does not emit
    // `IterMut::new` address-space wraparound `ub_checks` inside validation loops.
    let mut i = 0;
    while i < buf.len() {
        buf[i].write(seed as u8);
        seed = seed.wrapping_mul(0x1234567);
        i += 1;
    }
}

/// Validates `c_memmove` and `libc::string::memcpy` against each other across all alignment and
/// small-size combinations.
///
/// # Safety
///
/// The caller must ensure that `bufs.src`, `bufs.dst`, `bufs.src2`, and `bufs.dst2` point to four
/// disjoint buffers, each valid for reads and writes of at least `512` bytes within a single
/// allocated object (without wrapping the address space) and not concurrently accessed for the
/// duration of the call.
unsafe fn validate_memcpy(bufs: &mut TestBuffers) {
    const MAXSIZE: usize = 256;
    const BUF_LEN: usize = MAXSIZE * 2;
    zr::static_assert!(MAX_ALIGN + MAXSIZE <= BUF_LEN);

    kprintln!("testing memcpy for correctness");

    // SAFETY: Caller guarantees `bufs.src` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let src = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.src) };
    // SAFETY: Caller guarantees `bufs.src2` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let src2 = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.src2) };
    // SAFETY: Caller guarantees `bufs.dst` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let dst = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.dst) };
    // SAFETY: Caller guarantees `bufs.dst2` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let dst2 = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.dst2) };

    // do the simple tests to make sure that memcpy doesn't color outside
    // the lines for all alignment cases
    for srcalign in 0..MAX_ALIGN {
        kprintln!("srcalign {}", srcalign);
        for dstalign in 0..MAX_ALIGN {
            for size in 0..MAXSIZE {
                fillbuf(src, 567);
                fillbuf(src2, 567);
                fillbuf(dst, 123514);
                fillbuf(dst2, 123514);

                // SAFETY: By `fillbuf`'s `# Safety-usable invariant`, all `512` bytes of `src` and
                // `dst` are initialized. `size <= 255 <= isize::MAX as usize`, and
                // `srcalign + size` and `dstalign + size` are at most `63 + 255 = 318 <= 512`, so
                // both offset pointers are valid, non-null, and in-bounds within their disjoint
                // single allocated buffers (without wrapping the address space, even when
                // `size == 0`) with no concurrent access per caller precondition.
                unsafe {
                    c_memmove(
                        dst.as_mut_ptr().wrapping_add(dstalign).cast::<c_void>(),
                        src.as_ptr().wrapping_add(srcalign).cast::<c_void>(),
                        size,
                    );
                }
                // SAFETY: By `fillbuf`'s `# Safety-usable invariant`, all `512` bytes of `src2`
                // and `dst2` are initialized. `size <= 255 <= isize::MAX as usize`, and
                // `srcalign + size` and `dstalign + size` are at most `63 + 255 = 318 <= 512`, so
                // both offset pointers are valid, non-null, and in-bounds within their disjoint
                // single allocated buffers (without wrapping the address space, even when
                // `size == 0`) with no concurrent access per caller precondition.
                unsafe {
                    memcpy(
                        dst2.as_mut_ptr().wrapping_add(dstalign).cast::<c_void>(),
                        src2.as_ptr().wrapping_add(srcalign).cast::<c_void>(),
                        size,
                    );
                }

                // SAFETY: By `fillbuf`'s `# Safety-usable invariant`, all `512` bytes of `dst`
                // and `dst2` were initialized, and the intervening `c_memmove` and `memcpy` calls
                // preserved full `512`-byte initialization per their `# Safety` postconditions
                // (writing only within `dst[dstalign..dstalign + size]` and
                // `dst2[dstalign..dstalign + size]` where `dstalign + size <= 318 <= 512`). By
                // `uninit_array_mut` and caller preconditions, `dst` and `dst2` are non-null and
                // each lies within a single allocated object valid for reads of `BUF_LEN`
                // (`512 <= isize::MAX`) bytes without wrapping the address space or concurrent
                // mutation.
                let comp = unsafe {
                    memcmp(dst.as_ptr().cast::<c_void>(), dst2.as_ptr().cast::<c_void>(), BUF_LEN)
                };
                if comp != 0 {
                    core::hint::cold_path();
                    kprintln!("error! srcalign {}, dstalign {}, size {}", srcalign, dstalign, size);
                }
            }
        }
    }
}

/// Times [`ITERATIONS`] invocations of `memset_routine` filling `len` bytes starting at
/// `bufs.dst + dstalign`, returning elapsed milliseconds.
///
/// # Safety
///
/// The caller must ensure that `len <= isize::MAX as usize`, `dstalign + len` does not overflow
/// `usize` or wrap the address space, `bufs.dst` is valid for writes of `dstalign + len` bytes
/// within a single allocated object, and `bufs.dst` is not concurrently accessed during the call.
/// `memset_routine` must be sound to invoke whenever [`memset`]'s `# Safety` contract holds.
unsafe fn bench_memset_routine<const OPAQUE: bool>(
    bufs: &TestBuffers,
    memset_routine: MemsetRoutine,
    dstalign: usize,
    len: usize,
) -> i64 {
    let ptr = bufs.dst.as_ptr().wrapping_add(dstalign).cast::<c_void>();
    let t0 = current_mono_time();
    for _ in 0..ITERATIONS {
        // When benchmarking `libc::string::memset`, `opaque_ptr` prevents LLVM from folding or
        // hoisting repeated calls across iterations; `c_memset` uses volatile stores, so passing
        // `ptr` directly keeps its alignment loop-invariant and lets LLVM hoist prefix setup.
        let dst = if OPAQUE { opaque_ptr(ptr) } else { ptr };
        // SAFETY: By caller precondition, `ptr` (and, when `OPAQUE` is true, `opaque_ptr(ptr)` by
        // `opaque_ptr`'s `# Safety-usable invariant`) is non-null and valid for writes of
        // `len <= isize::MAX as usize` bytes within a single allocated object without wrapping the
        // address space or concurrent access, and `memset_routine` is sound under those conditions.
        unsafe {
            memset_routine(dst, 0, len);
        }
    }
    (current_mono_time() - t0).into_millis()
}

/// Benchmarks `c_memset`, `libc::string::memset`, and the `my` column (a second
/// `libc::string::memset` pass) across all destination alignments.
///
/// # Safety
///
/// The caller must ensure that `bufs.dst` is valid for writes of at least [`BUFFER_STRIDE`] bytes
/// within a single allocated object (without wrapping the address space and not concurrently
/// accessed during the call), the calling CPU's scheduler is initialized, and its `queue_lock_`
/// is not held.
unsafe fn bench_memset(bufs: &TestBuffers) {
    kprintln!("memset speed test");
    // let the debug string clear the serial port
    let _ = sleep_relative(DurationMono::from_millis(200));

    for dstalign in 0..MAX_ALIGN {
        let (c, libc, mine) = {
            // SAFETY: The caller guarantees the current CPU's scheduler is initialized and its
            // `queue_lock_` is unheld, and nothing in this block acquires `queue_lock_` or
            // re-enables interrupts before `_inactive_cpu_guard` drops.
            let _inactive_cpu_guard = unsafe { InactiveCpuGuard::new() };
            // SAFETY: `dstalign < MAX_ALIGN` and `BUFFER_SIZE <= isize::MAX as usize` keep
            // `dstalign + BUFFER_SIZE <= BUFFER_STRIDE` in-bounds of `bufs.dst` without wrapping or
            // concurrent access, and `c_memset` is sound for `BUFFER_SIZE` bytes.
            let c = unsafe { bench_memset_routine::<false>(bufs, c_memset, dstalign, BUFFER_SIZE) };
            // SAFETY: `dstalign < MAX_ALIGN` and `BUFFER_SIZE <= isize::MAX as usize` keep
            // `dstalign + BUFFER_SIZE <= BUFFER_STRIDE` in-bounds of `bufs.dst` without wrapping or
            // concurrent access, and `memset` is sound for `BUFFER_SIZE` bytes.
            let libc = unsafe { bench_memset_routine::<true>(bufs, memset, dstalign, BUFFER_SIZE) };
            // SAFETY: `dstalign < MAX_ALIGN` and `BUFFER_SIZE <= isize::MAX as usize` keep
            // `dstalign + BUFFER_SIZE <= BUFFER_STRIDE` in-bounds of `bufs.dst` without wrapping or
            // concurrent access, and `memset` is sound for `BUFFER_SIZE` bytes.
            let mine = unsafe { bench_memset_routine::<true>(bufs, memset, dstalign, BUFFER_SIZE) };
            (c, libc, mine)
        };

        kprint!("dstalign {}: ", dstalign);
        kprint!("c {} msecs, {} bytes/sec; ", c, bytes_per_sec(c));
        kprint!("libc {} msecs, {} bytes/sec; ", libc, bytes_per_sec(libc));
        kprintln!("my {} msecs, {} bytes/sec; ", mine, bytes_per_sec(mine));
    }
}

/// Validates `c_memset` and `libc::string::memset` against each other across all alignment, byte
/// value, and small-size combinations.
///
/// # Safety
///
/// The caller must ensure that `bufs.dst` and `bufs.dst2` point to two disjoint buffers, each
/// valid for reads and writes of at least `512` bytes within a single allocated object (without
/// wrapping the address space) and not concurrently accessed for the duration of the call.
unsafe fn validate_memset(bufs: &mut TestBuffers) {
    const MAXSIZE: usize = 256;
    const BUF_LEN: usize = MAXSIZE * 2;
    zr::static_assert!(MAX_ALIGN + MAXSIZE <= BUF_LEN);

    kprintln!("testing memset for correctness");

    // SAFETY: Caller guarantees `bufs.dst` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let dst = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.dst) };
    // SAFETY: Caller guarantees `bufs.dst2` is live and valid for reads and writes of `BUF_LEN`
    // (`512 <= isize::MAX`) bytes within a single allocated object (without wrapping the address
    // space) with no other accesses for the duration of `bufs`'s borrow in this call.
    let dst2 = unsafe { uninit_array_mut::<BUF_LEN>(&mut bufs.dst2) };

    for dstalign in 0..MAX_ALIGN {
        kprintln!("align {}", dstalign);
        for size in 0..MAXSIZE {
            for c in -1..257 {
                fillbuf(dst, 123514);
                fillbuf(dst2, 123514);

                // Pass `dst` through `opaque_ptr` so LLVM does not hoist `c_memset`'s alignment
                // prefix pointers and word-loop parameters out of the `c` loop across
                // `memset`/`memcmp`/`kprintln!` and spill them to the stack.
                // SAFETY: `dst` is valid, non-null, and valid for writes of `512` bytes within a
                // single allocated object (without wrapping the address space and with no
                // concurrent access per caller precondition); `size <= 255 <= isize::MAX as usize`;
                // and `dstalign + size <= 63 + 255 = 318 <= 512`, so the offset pointer (with
                // address and provenance preserved by `opaque_ptr`'s `# Safety-usable invariant`)
                // is in-bounds even when `size == 0`.
                unsafe {
                    c_memset(
                        opaque_ptr(dst.as_mut_ptr().wrapping_add(dstalign).cast::<c_void>()),
                        c,
                        size,
                    );
                }
                // SAFETY: `dst2` is valid, non-null, and valid for writes of `512` bytes within a
                // single allocated object (without wrapping the address space and with no
                // concurrent access per caller precondition); `size <= 255 <= isize::MAX as usize`;
                // and `dstalign + size <= 63 + 255 = 318 <= 512`, so the offset pointer is
                // in-bounds even when `size == 0`.
                unsafe {
                    memset(dst2.as_mut_ptr().wrapping_add(dstalign).cast::<c_void>(), c, size);
                }

                // SAFETY: By `fillbuf`'s `# Safety-usable invariant`, all `512` bytes of `dst`
                // and `dst2` were initialized, and the intervening `c_memset` and `memset` calls
                // preserved full `512`-byte initialization per their `# Safety` postconditions
                // (writing only within `dst[dstalign..dstalign + size]` and
                // `dst2[dstalign..dstalign + size]` where `dstalign + size <= 318 <= 512`). By
                // `uninit_array_mut` and caller preconditions, `dst` and `dst2` are non-null and
                // each lies within a single allocated object valid for reads of `BUF_LEN`
                // (`512 <= isize::MAX`) bytes without wrapping the address space or concurrent
                // mutation.
                let comp = unsafe {
                    memcmp(dst.as_ptr().cast::<c_void>(), dst2.as_ptr().cast::<c_void>(), BUF_LEN)
                };
                if comp != 0 {
                    core::hint::cold_path();
                    kprintln!("error! align {}, c 0x{:x}, size {}", dstalign, c as u8, size);
                }
            }
        }
    }
}

/// Console command entry point (`string`) for `memcpy` and `memset` validation and benchmarks.
///
/// # Safety
///
/// Must be called from normal, blockable kernel thread context after early boot with interrupts
/// enabled, the calling CPU's `Scheduler` initialized, no spinlocks (specifically `queue_lock_`)
/// held, and no `ChainLockTransaction` active (as guaranteed by `CMD_AVAIL_NORMAL` console
/// dispatch). The caller must also ensure that if `argc > 0`, `argv` is non-null, properly aligned
/// for [`CmdArgs`], valid for reads of `argc` initialized [`CmdArgs`] elements within a single
/// allocated object (`(argc as usize) * size_of::<CmdArgs>() <= isize::MAX as usize`), and not
/// mutated for the duration of the call, and that every non-null `arg_str` in `argv[0..argc]`
/// points to a valid, immutable NUL-terminated C string of total byte length (including the NUL
/// terminator) `<= isize::MAX as usize` for the duration of the call.
unsafe extern "C" fn string_tests(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    pin_init::stack_pin_init!(let list = VmPageDoublyLinkedList::new());

    // allocate a large run of physically contiguous pages and get the address out
    // of the physmap
    let total_size = BUFFER_STRIDE * 4;
    let page_count = page::round_up(total_size) / page::SIZE;
    let Ok(pa) = pmm::alloc_contiguous(page_count, 0, page::SHIFT as u8, list.as_mut()) else {
        kprintln!("failed to allocate {} bytes of contiguous memory for test", total_size);
        return -1;
    };

    // free the physical pages on exit
    let _free_pages = zr::defer(|| {
        // SAFETY: `list` contains only the exclusively owned, non-loaned pages allocated by
        // `pmm::alloc_contiguous` above, and no pointers into those pages are used after
        // `_free_pages` drops.
        unsafe { pmm::free_list(list.as_mut()) };
    });

    let Some(src) = NonNull::new(paddr_to_physmap_mut::<u8>(pa)) else {
        kprintln!("physmap returned null for physical address {:#x}", pa.0);
        return -1;
    };
    // SAFETY: `pmm::alloc_contiguous` allocated `page_count * page::SIZE >= BUFFER_STRIDE * 4`
    // contiguous bytes mapped in the kernel physmap starting at `src`, so each `BUFFER_STRIDE * k`
    // (`k <= 3`) offset stays within that single allocated object.
    let (dst, src2, dst2) =
        unsafe { (src.add(BUFFER_STRIDE), src.add(BUFFER_STRIDE * 2), src.add(BUFFER_STRIDE * 3)) };
    let mut bufs = TestBuffers { src, dst, src2, dst2 };

    kprintln!("src {:p}, dst {:p}", bufs.src.as_ptr(), bufs.dst.as_ptr());
    kprintln!("src2 {:p}, dst2 {:p}", bufs.src2.as_ptr(), bufs.dst2.as_ptr());

    if argc < 1 || argv.is_null() || !argv.is_aligned() {
        return -1;
    }
    // SAFETY: `argc >= 1` and `argv` is non-null and `CmdArgs`-aligned; by caller precondition,
    // `argv` is valid for reads of `argc` initialized `CmdArgs` elements within a single allocated
    // object (`(argc as usize) * size_of::<CmdArgs>() <= isize::MAX as usize`) and immutable for
    // the duration of the call. The returned `&[CmdArgs]` slice borrows `argv` immutably only for
    // the duration of `string_tests`.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        let cmd = args[0].as_str();
        kprintln!("{:s} validate <routine>", cmd);
        kprintln!("{:s} bench <routine>", cmd);
        -1
    };

    if argc < 3 {
        kprintln!("not enough arguments:");
        return usage();
    }

    match args[1].as_str() {
        "validate" => match args[2].as_str() {
            "memcpy" => {
                // SAFETY: `bufs` holds four disjoint `BUFFER_STRIDE`-byte (`>= 512`) physmap
                // buffers uniquely valid for reads and writes (within the single contiguous
                // physmap allocation, without wrapping the address space, and with no concurrent
                // access) until `_free_pages` drops.
                unsafe { validate_memcpy(&mut bufs) };
            }
            "memset" => {
                // SAFETY: `bufs.dst` and `bufs.dst2` are disjoint `BUFFER_STRIDE`-byte (`>= 512`)
                // physmap buffers uniquely valid for reads and writes (within the single contiguous
                // physmap allocation, without wrapping the address space, and with no concurrent
                // access) until `_free_pages` drops.
                unsafe { validate_memset(&mut bufs) };
            }
            _ => {}
        },
        "bench" => match args[2].as_str() {
            "memcpy" => {
                // SAFETY: `bufs.src` is non-null, 1-byte aligned for `u8`, and uniquely valid for
                // writes of `BUFFER_STRIDE <= isize::MAX as usize` bytes (with no concurrent
                // access) within the contiguous physmap allocation until `_free_pages` drops. On
                // return, all `BUFFER_STRIDE` bytes at `bufs.src` are initialized to `0`.
                unsafe { bufs.src.write_bytes(0, BUFFER_STRIDE) };
                // SAFETY: `bufs.src` was just zero-initialized for `BUFFER_STRIDE` bytes, and
                // `bufs.src` and `bufs.dst` are disjoint `BUFFER_STRIDE`-byte physmap buffers
                // uniquely valid for reads and writes respectively (within the single contiguous
                // physmap allocation, without wrapping the address space, and with no concurrent
                // access) until `_free_pages` drops. Per `string_tests`'s `# Safety` preconditions,
                // the calling CPU's scheduler is initialized and its `queue_lock_` is not held.
                unsafe { bench_memcpy(&bufs) };
            }
            "memset" => {
                // SAFETY: `bufs.dst` is a `BUFFER_STRIDE`-byte physmap buffer uniquely valid for
                // writes (within the single contiguous physmap allocation, without wrapping the
                // address space, and with no concurrent access) until `_free_pages` drops. Per
                // `string_tests`'s `# Safety` preconditions, the calling CPU's scheduler is
                // initialized and its `queue_lock_` is not held.
                unsafe { bench_memset(&bufs) };
            }
            _ => {}
        },
        _ => return usage(),
    }

    0
}

static_command!(
    STRING_CMD,
    c"string".as_ptr(),
    c"memcpy tests".as_ptr(),
    string_tests,
    CMD_AVAIL_NORMAL
);
