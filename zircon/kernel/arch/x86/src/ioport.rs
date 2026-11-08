// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::convert::Infallible;
use core::mem::{align_of, size_of};
use core::pin::Pin;

use crate::ksync_kernel::{NoIrqSavePolicy, TrackedSpinlock};
use bitmap::{FreeList, RleBitmap, RleBitmapElement};
use ksync::{KMutex, guarded};
use pin_init::{PinInit, pin_init};
use zx_status::Status;

use super::descriptor::{IO_BITMAP_BITS, with_curr_tss_bitmap_mut};
use crate::kernel::auto_preempt_disabler::AutoPreemptDisabler;
use crate::kernel::mp::{self, MpIpiTarget};
use crate::kernel::thread;

/// Sets `bitlen` bits starting at `bitoff` (`[bitoff, bitoff + bitlen)`) to `1` in `bytes`.
#[inline]
fn bitmap_set_bits(bytes: &mut [u8], bitoff: usize, bitlen: usize) {
    debug_assert!(bitoff <= bytes.len() * 8 && bitlen <= bytes.len() * 8 - bitoff);
    // The bit range is constrained to fit within `bytes`.
    if bitlen == 0 {
        return;
    }
    let bitmax = bitoff + bitlen;
    let first_byte = bitoff / 8;
    let last_byte = (bitmax - 1) / 8;
    let first_mask = !0u8 << (bitoff % 8);
    let last_mask = !0u8 >> (7 - ((bitmax - 1) % 8));

    if first_byte >= last_byte {
        bytes[first_byte] |= first_mask & last_mask;
    } else {
        bytes[first_byte] |= first_mask;
        bytes[first_byte + 1..last_byte].fill(0xff);
        bytes[last_byte] |= last_mask;
    }
}

/// Clears `bitlen` bits starting at `bitoff` (`[bitoff, bitoff + bitlen)`) to `0` in `bytes`.
#[inline]
fn bitmap_clear_bits(bytes: &mut [u8], bitoff: usize, bitlen: usize) {
    debug_assert!(bitoff <= bytes.len() * 8 && bitlen <= bytes.len() * 8 - bitoff);
    // The bit range is constrained to fit within `bytes`.
    if bitlen == 0 {
        return;
    }
    let bitmax = bitoff + bitlen;
    let first_byte = bitoff / 8;
    let last_byte = (bitmax - 1) / 8;
    let first_mask = !0u8 << (bitoff % 8);
    let last_mask = !0u8 >> (7 - ((bitmax - 1) % 8));

    if first_byte >= last_byte {
        bytes[first_byte] &= !(first_mask & last_mask);
    } else {
        bytes[first_byte] &= !first_mask;
        bytes[first_byte + 1..last_byte].fill(0x00);
        bytes[last_byte] &= !last_mask;
    }
}

/// Resets the current CPU's TSS I/O permission bitmap to all-ones (`x86_reset_tss_io_bitmap`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled (`ints_disabled()`).
#[inline]
pub unsafe fn reset_tss_io_bitmap() {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled on the
    // current CPU, providing exclusive access to this CPU's `default_tss.tss_bitmap`.
    unsafe {
        with_curr_tss_bitmap_mut(|tss_bitmap| {
            tss_bitmap.fill(0xff);
        });
    }
}

/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled (`ints_disabled()`).
#[cold]
#[inline(never)]
unsafe fn clear_tss_io_bitmap_rle(bitmap: &RleBitmap) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled on the
    // current CPU, and extents in `bitmap` are validated by `set_io_bitmap` to never exceed
    // `IO_BITMAP_BITS`.
    unsafe {
        with_curr_tss_bitmap_mut(|tss_bitmap| {
            for extent in bitmap {
                debug_assert!(extent.bitoff + extent.bitlen <= IO_BITMAP_BITS);
                bitmap_set_bits(tss_bitmap, extent.bitoff, extent.bitlen);
            }
        });
    }
}

/// Clears the I/O port permissions of `io_bitmap` from the current CPU's TSS I/O bitmap
/// (`x86_clear_tss_io_bitmap`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled (`ints_disabled()`).
#[inline]
pub unsafe fn clear_tss_io_bitmap(io_bitmap: &IoBitmap) {
    ksync::lock!(let guard = io_bitmap.lock_lock_policy::<NoIrqSavePolicy>());
    let Some(bitmap) = guard.fields().bitmap.as_ref() else {
        return;
    };
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    unsafe { clear_tss_io_bitmap_rle(bitmap) };
}

/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled (`ints_disabled()`).
#[cold]
#[inline(never)]
unsafe fn set_tss_io_bitmap_rle(bitmap: &RleBitmap) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled on the
    // current CPU, and extents in `bitmap` are validated by `set_io_bitmap` to never exceed
    // `IO_BITMAP_BITS`.
    unsafe {
        with_curr_tss_bitmap_mut(|tss_bitmap| {
            for extent in bitmap {
                debug_assert!(extent.bitoff + extent.bitlen <= IO_BITMAP_BITS);
                bitmap_clear_bits(tss_bitmap, extent.bitoff, extent.bitlen);
            }
        });
    }
}

/// Applies the I/O port permissions of `io_bitmap` to the current CPU's TSS I/O bitmap
/// (`x86_set_tss_io_bitmap`).
///
/// # Safety
///
/// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts are
/// disabled (`ints_disabled()`).
#[inline]
pub unsafe fn set_tss_io_bitmap(io_bitmap: &IoBitmap) {
    ksync::lock!(let guard = io_bitmap.lock_lock_policy::<NoIrqSavePolicy>());
    let Some(bitmap) = guard.fields().bitmap.as_ref() else {
        return;
    };
    // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
    unsafe { set_tss_io_bitmap_rle(bitmap) };
}

/// Per-address-space x86 I/O permission bitmap (`IoBitmap`).
#[guarded]
#[repr(C)]
pub struct IoBitmap {
    #[mutex]
    lock: KMutex<TrackedSpinlock>,
    #[guarded_by(lock)]
    bitmap: Option<Pin<kalloc::Box<RleBitmap>>>,
    #[pin]
    _pin: core::marker::PhantomData<core::marker::PhantomPinned>,
}

zr::static_assert!(core::mem::offset_of!(IoBitmap, lock) == 0);
zr::static_assert!(
    core::mem::offset_of!(IoBitmap, bitmap) == size_of::<TrackedSpinlock>().next_multiple_of(8)
);

zr::static_assert!(
    size_of::<IoBitmap>() == size_of::<TrackedSpinlock>().next_multiple_of(8) + size_of::<usize>()
);
zr::static_assert!(align_of::<IoBitmap>() == 8);

// SAFETY: All access to `IoBitmap::bitmap` is synchronized through `IoBitmap::lock`.
unsafe impl Sync for IoBitmap {}

impl IoBitmap {
    /// Creates an in-place pinned initializer for [`IoBitmap`].
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KMutex::init(),
            bitmap: None.into(),
            _pin: core::marker::PhantomData,
        })
    }

    /// Invokes `f` with a reference to the [`IoBitmap`] associated with the current thread, or
    /// returns `None` if the thread has no associated [`IoBitmap`] (such as idle threads).
    #[inline]
    pub fn with_current<R>(f: impl FnOnce(Pin<&IoBitmap>) -> R) -> Option<R> {
        // Fetch current thread's address space. If we have no address space (e.g.,
        // the idle thread), we also don't have an IO Bitmap.
        // SAFETY: `%gs.base` is initialized on all online CPUs. The returned reference remains
        // valid because switching the current thread's active `VmAspace` cannot happen while
        // `f` executes.
        let aspace = unsafe { thread::current_active_aspace()? };
        let arch_aspace = aspace.arch_aspace();
        // SAFETY: `arch_aspace` is a valid `X86ArchVmAspace` whose embedded `io_bitmap_` lives
        // for the duration of `aspace` remaining active on the current thread while `f` executes.
        let ptr = unsafe { super::ioport_ffi::x86_arch_aspace_io_bitmap(arch_aspace) };
        // SAFETY: `ptr` is non-null, properly aligned, and points to the live (not freed)
        // `IoBitmap` of the current active `VmAspace`, which remains pinned and valid for the
        // duration of `f`.
        let bitmap = unsafe { ptr.as_ref()? };
        // SAFETY: The `IoBitmap` is pinned in the `VmAspace`.
        Some(f(unsafe { Pin::new_unchecked(bitmap) }))
    }

    /// Task used for updating IO permissions on each CPU.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `%gs.base` is initialized for the current CPU and interrupts
    /// are disabled (`ints_disabled()`).
    unsafe fn update_task() {
        debug_assert!(crate::arch_rs::ints_disabled());

        // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
        unsafe {
            reset_tss_io_bitmap();
        }

        // Reprogram the hardware to match the current CPU's active bitmap.
        Self::with_current(|io_bitmap| {
            // SAFETY: Caller guarantees `%gs.base` is initialized and interrupts are disabled.
            unsafe { set_tss_io_bitmap(&io_bitmap) };
        });
    }

    /// Modifies the I/O port bitmap.
    pub fn set_io_bitmap(
        self: Pin<&Self>,
        port: u32,
        len: u32,
        enable: bool,
    ) -> Result<(), Status> {
        debug_assert!(!crate::arch_rs::ints_disabled());

        let port = port as usize;
        let len = len as usize;
        let Some(end) = port.checked_add(len) else {
            return Err(Status::INVALID_ARGS);
        };
        if end > IO_BITMAP_BITS {
            return Err(Status::INVALID_ARGS);
        }

        let needs_bitmap = {
            ksync::lock!(let guard = self.lock_lock());
            guard.fields().bitmap.is_none()
        };
        let mut optimistic_bitmap: Option<Pin<kalloc::Box<RleBitmap>>> = None;
        if needs_bitmap {
            // Optimistically allocate a bitmap structure if we don't have one, and
            // we'll see if we actually need this allocation later. In the common
            // case, when we make the allocation we will use it.
            let mut uninit =
                kalloc::Box::<RleBitmap>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;
            // SAFETY: `uninit.as_mut_ptr()` points to valid, aligned, uninitialized
            // heap memory for `RleBitmap` that remains pinned until `Box::drop` drops it in
            // place, and `RleBitmap::new()` is infallible.
            optimistic_bitmap = Some(unsafe {
                let _ = RleBitmap::new().__pinned_init(uninit.as_mut_ptr());
                Pin::new_unchecked(uninit.assume_init())
            });
        }

        // Create a free-list in case any of our bitmap operations need to free any
        // nodes.
        pin_init::stack_pin_init!(let bitmap_freelist = FreeList::<usize>::new());

        // Optimistically allocate an element for the bitmap, in case we need one.
        let elem =
            fbl::UniquePtr::try_new(RleBitmapElement::default()).map_err(|_| Status::NO_MEMORY)?;
        bitmap_freelist.as_mut().push_back(elem);

        // Update this thread's bitmap.
        //
        // Keep in mind there are really two bitmaps, this thread's bitmap (`bitmap`)
        // and the one *in* the CPU on which this thread is executing. The procedure
        // for updating `bitmap` is security critical.
        //
        // During a context switch, the in-CPU bitmap is adjusted using both the old
        // thread's `bitmap` and the new thread's `bitmap`. The bits that were set
        // in old thread's `bitmap` are cleared from the in-CPU state and the bits
        // that are set in the new thread's `bitmap` are set in the in-CPU state.
        //
        // At the time of context switch, it is crucial that the old thread's
        // `bitmap` match the in-CPU state. Otherwise, the context switch may fail
        // to clear some bits and inadvertently grant the new thread elevated
        // privilege.
        //
        // Once we have modified `bitmap` we must ensure that no other thread executes
        // on this CPU until the in-CPU state has been updated. To accomplish that,
        // we disable preemption and take care to not call any functions that might
        // block or otherwise enter the scheduler.
        {
            let _preempt_disabler = AutoPreemptDisabler::new();

            {
                ksync::lock!(let mut guard = self.lock_lock());
                let fields = guard.as_mut().fields_mut();
                let bitmap = fields
                    .bitmap
                    .get_or_insert_with(|| optimistic_bitmap.take().expect("allocated above"));
                if enable {
                    bitmap.as_mut().set_no_alloc(port, end, bitmap_freelist.as_mut())?;
                } else {
                    bitmap.as_mut().clear_no_alloc(port, end, bitmap_freelist.as_mut())?;
                }
            }

            // Let all CPUs know about the update.
            // TODO(https://fxbug.dev/540178114): `update_task()` unconditionally calls
            // `reset_tss_io_bitmap()` on every CPU when the IPI arrives. Revoked port bits
            // can only remain clear in the brief window between releasing `self.lock` and
            // the IPI arriving on another CPU that context-switched away from this aspace.
            // A follow-up should optimize this by invalidating the TSS I/O map base
            // on context switch.
            // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled, and
            // `mp::sync_exec` executes the closure with interrupts disabled on each target CPU.
            unsafe {
                mp::sync_exec(MpIpiTarget::All, 0, || {
                    Self::update_task();
                });
            }

            // Now that we've returned from `mp::sync_exec`, we know this CPU's state
            // matches the updated `bitmap`. It's now safe to re-enable preemption.
            // RAII `_preempt_disabler` is dropped here.
            Ok(())
        }
    }
}

/// Unit tests for x86 TSS I/O permission bitmap manipulation.
#[cfg(ktest)]
#[unittest::suite(name = "x86_ioport")]
mod tests {
    use crate::arch_rs::InterruptDisableGuard;
    use unittest::{assert_eq, assert_err, assert_ok, assert_true};
    use zx_status::Status;

    fn is_bit_set(bytes: &[u8], bit: usize) -> bool {
        (bytes[bit / 8] & (1u8 << (bit % 8))) != 0
    }

    /// Tests `bitmap_set_bits` and `bitmap_clear_bits`.
    #[test]
    fn test_bitmap_set_and_clear_bits() {
        let mut bytes = [0xffu8; 32];

        // Clear a sub-byte range `[3, 7)` (4 bits in byte 0).
        bitmap_clear_bits(&mut bytes, 3, 4);
        assert_eq!(bytes[0], 0b1000_0111);
        assert_eq!(bytes[1], 0xff);

        // Set `[4, 6)` back to 1.
        bitmap_set_bits(&mut bytes, 4, 2);
        assert_eq!(bytes[0], 0b1011_0111);

        // Clear a multi-byte unaligned range `[6, 21)`.
        bytes.fill(0xff);
        bitmap_clear_bits(&mut bytes, 6, 15);
        for bit in 0..32 {
            let expected = !(6..21).contains(&bit);
            assert_eq!(is_bit_set(&bytes, bit), expected);
        }

        // Reset `[10, 19)` back to 1.
        bitmap_set_bits(&mut bytes, 10, 9);
        for bit in 0..32 {
            let expected = !(6..10).contains(&bit) && !(19..21).contains(&bit);
            assert_eq!(is_bit_set(&bytes, bit), expected);
        }

        // Zero-length operations are no-ops.
        bitmap_clear_bits(&mut bytes, 0, 0);
        assert_true!(is_bit_set(&bytes, 0));
    }

    /// Tests `IoBitmap::set_io_bitmap` and TSS I/O bitmap programming.
    #[test]
    fn test_io_bitmap_set_and_clear() {
        pin_init::stack_pin_init!(let io_bitmap = IoBitmap::init());

        assert_err!(io_bitmap.as_ref().set_io_bitmap(65536, 1, true), Status::INVALID_ARGS);
        assert_err!(io_bitmap.as_ref().set_io_bitmap(u32::MAX, 2, true), Status::INVALID_ARGS);

        assert_ok!(io_bitmap.as_ref().set_io_bitmap(0x80, 4, true));
        let (after_set, after_clear) = {
            let _irqd = InterruptDisableGuard::new();
            // SAFETY: Interrupts are disabled on the current CPU and `%gs.base` is initialized.
            unsafe {
                set_tss_io_bitmap(&io_bitmap);
                let after_set = with_curr_tss_bitmap_mut(|tss_bitmap| {
                    [
                        is_bit_set(tss_bitmap, 0x7f),
                        is_bit_set(tss_bitmap, 0x80),
                        is_bit_set(tss_bitmap, 0x81),
                        is_bit_set(tss_bitmap, 0x82),
                        is_bit_set(tss_bitmap, 0x83),
                        is_bit_set(tss_bitmap, 0x84),
                    ]
                });
                clear_tss_io_bitmap(&io_bitmap);
                let after_clear = with_curr_tss_bitmap_mut(|tss_bitmap| {
                    [
                        is_bit_set(tss_bitmap, 0x80),
                        is_bit_set(tss_bitmap, 0x81),
                        is_bit_set(tss_bitmap, 0x82),
                        is_bit_set(tss_bitmap, 0x83),
                    ]
                });
                reset_tss_io_bitmap();
                (after_set, after_clear)
            }
        };
        assert_true!(after_set == [true, false, false, false, false, true]);
        assert_true!(after_clear == [true, true, true, true]);
        assert_ok!(io_bitmap.as_ref().set_io_bitmap(0x80, 4, false));
    }
}
