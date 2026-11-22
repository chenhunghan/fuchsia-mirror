// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    BusTransactionInitiatorDispatcher, HandleOwner, HandleValue, IOMMU_FLAG_PERM_EXECUTE,
    IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, IommuDispatcher, PinnedMemoryTokenDispatcher,
    ProcessDispatcher, QueryAddressResult, VmObjectDispatcher, dev_vaddr_t,
};
use crate::user_copy::UserOutPtr;
use crate::vm::pmm;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_BTI_COMPRESS, ZX_BTI_CONTIGUOUS, ZX_BTI_PERM_EXECUTE, ZX_BTI_PERM_READ, ZX_BTI_PERM_WRITE,
    ZX_RIGHT_MAP, ZX_RIGHT_NONE, ZX_RIGHT_READ, ZX_RIGHT_WRITE, zx_paddr_t,
};

use buffered::BufferedUserOutPtr;

const LOCAL_TRACE: u32 = 0;

/// Number of device virtual addresses buffered on the kernel stack per user-copy batch in
/// [`sys_bti_pin`].
const PIN_ADDR_BATCH_SIZE: usize = 32;

mod buffered {
    use core::mem::MaybeUninit;

    use zerocopy::{Immutable, IntoBytes};
    use zx_status::Status;

    use crate::user_copy::UserOutPtr;

    /// Helper for optimizing writing many small elements of a user pointer array by allowing for a
    /// variable amount of buffering.
    ///
    /// Elements are appended via [`Self::write`] inside [`Self::with`], which flushes any
    /// remaining buffered entries when the closure returns `Ok`. If the closure returns `Err` (or
    /// a full-batch flush inside [`Self::write`] fails), any unflushed elements currently in the
    /// buffer are discarded without writing them to user space, while any full `N`-element batches
    /// already flushed during earlier [`Self::write`] calls remain written in user space.
    pub struct BufferedUserOutPtr<T, const N: usize> {
        out_ptr: UserOutPtr<T>,
        buffer: [MaybeUninit<T>; N],
        index: usize,
    }

    impl<T: IntoBytes + Immutable + Copy, const N: usize> BufferedUserOutPtr<T, N> {
        /// Calls `f` with a stack-buffered writer targeting `out_ptr`, flushing any remaining
        /// buffered elements to user space if `f` returns `Ok`.
        #[inline]
        pub fn with<R>(
            out_ptr: UserOutPtr<T>,
            f: impl FnOnce(&mut Self) -> Result<R, Status>,
        ) -> Result<R, Status> {
            // Expectation is this is going to be stack allocated, so ensure it's non-empty and not
            // too big.
            const {
                assert!(N > 0 && core::mem::size_of::<[MaybeUninit<T>; N]>() < page::SIZE);
            };
            let mut buffered =
                Self { out_ptr, buffer: [const { MaybeUninit::uninit() }; N], index: 0 };
            let result = f(&mut buffered)?;
            buffered.flush()?;
            Ok(result)
        }

        /// Appends a single element to the buffer, flushing to user space if the buffer becomes
        /// full.
        #[inline]
        pub fn write(&mut self, val: T) -> Result<(), Status> {
            self.buffer[self.index].write(val);
            self.index += 1;
            if self.index == N {
                self.flush()?;
            }
            Ok(())
        }

        /// Flushes any currently buffered elements to user space and advances `out_ptr`.
        #[inline]
        fn flush(&mut self) -> Result<(), Status> {
            if self.index == 0 {
                return Ok(());
            }
            let count = self.index;
            self.index = 0;
            let init = &self.buffer[..count];
            // SAFETY: `self.index` only advances after `write` initializes
            // `self.buffer[self.index]`, so all `count` elements in `init` are initialized.
            let slice = unsafe { init.assume_init_ref() };
            self.out_ptr.copy_slice_to_user(slice)?;
            self.out_ptr = self.out_ptr.element_offset(count);
            Ok(())
        }
    }
}

/// Queries device virtual addresses for `[0, size)` in `target_contig`-sized chunks via
/// `query_address` and writes `addrs_count` entries to `addrs` in [`PIN_ADDR_BATCH_SIZE`] batches.
#[inline]
fn write_pin_addrs(
    size: u64,
    target_contig: usize,
    addrs: UserOutPtr<dev_vaddr_t>,
    addrs_count: usize,
    mut query_address: impl FnMut(u64, usize) -> Result<QueryAddressResult, Status>,
) -> Result<(), Status> {
    // Define a helper closure with some state that can fetch potentially large ranges from the
    // PMT, but return them gradually. This just serves as an optimization around repeatedly
    // querying the PMT for a range that it knows is contiguous, but where we need to fill out
    // multiple addresses for the user.
    struct ConsumeState {
        offset: u64,
        addr: dev_vaddr_t,
        remaining: usize,
    }
    let mut consume_state = ConsumeState { offset: 0, addr: 0, remaining: 0 };

    let mut consume_addr = |expected_contig: usize| -> Result<dev_vaddr_t, Status> {
        // If the amount of contiguous memory we are tracking in our consume_state
        // is not enough to satisfy the request, we need to perform another query.
        // Otherwise, we can just consume from the existing consume state.
        if expected_contig > consume_state.remaining {
            let remain = (size - consume_state.offset) as usize;
            let range = query_address(consume_state.offset, remain)?;
            if expected_contig > range.size {
                // This happening suggests an error with contiguity calculations and/or the
                // underlying IOMMU implementation not reporting its contiguity correctly.
                //
                // TODO(teisenbe): consider making this a louder error.
                return Err(Status::INVALID_ARGS);
            }
            consume_state.addr = range.device_vaddr;
            consume_state.remaining = range.size;
        }

        let ret = consume_state.addr;
        consume_state.offset += expected_contig as u64;
        consume_state.addr += expected_contig as dev_vaddr_t;
        consume_state.remaining -= expected_contig;
        Ok(ret)
    };

    // Calculate / lookup the addresses.
    BufferedUserOutPtr::<dev_vaddr_t, PIN_ADDR_BATCH_SIZE>::with(addrs, |buffered_addrs| {
        for i in 0..addrs_count {
            let expected_size = core::cmp::min(size as usize - (target_contig * i), target_contig);
            let addr = consume_addr(expected_size)?;
            buffered_addrs.write(addr)?;
        }
        Ok(())
    })
}

#[syscall]
pub fn sys_bti_create(
    iommu: HandleValue,
    options: u32,
    bti_id: u64,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options {options:#x}, bti_id {bti_id}\n");

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // TODO(teisenbe): This should probably have a right on it.
    let iommu_dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<IommuDispatcher>(up, iommu, ZX_RIGHT_NONE)
    })?;

    let (handle, rights) =
        BusTransactionInitiatorDispatcher::create(iommu_dispatcher.iommu(), bti_id)?;
    *out = handle.make_and_add_handle(rights)?;
    Ok(())
}

#[syscall]
pub fn sys_bti_release_quarantine(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let bti_dispatcher = ProcessDispatcher::with_current(|up| {
        up.handle_table().get_dispatcher_with_rights::<BusTransactionInitiatorDispatcher>(
            up,
            handle,
            ZX_RIGHT_WRITE,
        )
    })?;

    bti_dispatcher.release_quarantine();
    Ok(())
}

#[syscall]
pub fn sys_bti_pin(
    handle: HandleValue,
    options: u32,
    vmo: HandleValue,
    offset: u64,
    size: u64,
    addrs: UserOutPtr<zx_paddr_t>,
    addrs_count: usize,
    pmt: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!(
        "handle {:#x}, options {options:#x}, vmo {:#x}, offset {offset:#x}, size {size:#x}, addrs_count {addrs_count}\n",
        handle.raw_value(),
        vmo.raw_value(),
    );

    let up = ProcessDispatcher::get_current();
    let bti_dispatcher =
        up.handle_table().get_dispatcher_with_rights::<BusTransactionInitiatorDispatcher>(
            &up,
            handle,
            ZX_RIGHT_MAP,
        )?;

    // Address count is currently limited to the amount of addresses that can fit on 64 pages. This
    // is large enough for all current usage of bti_pin, but protects against the case of an
    // arbitrarily large array being allocated on the heap.
    const MAX_ADDRS: usize = (page::SIZE * 64) / core::mem::size_of::<dev_vaddr_t>();
    if !page::is_aligned(offset as usize)
        || !page::is_aligned(size as usize)
        || addrs_count > MAX_ADDRS
    {
        return Err(Status::INVALID_ARGS);
    }

    let (vmo_dispatcher, vmo_rights) = up
        .handle_table()
        .get_dispatcher_with_rights_and_actual::<VmObjectDispatcher>(&up, vmo, ZX_RIGHT_MAP)?;

    // Convert requested permissions and check against VMO rights
    let mut iommu_perms = 0u32;
    let mut compress_results = false;
    let mut contiguous = false;
    let mut remaining_options = options;

    if (remaining_options & ZX_BTI_PERM_READ) != 0 {
        if (vmo_rights & ZX_RIGHT_READ) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_READ;
        remaining_options &= !ZX_BTI_PERM_READ;
    }
    if (remaining_options & ZX_BTI_PERM_WRITE) != 0 {
        if (vmo_rights & ZX_RIGHT_WRITE) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_WRITE;
        remaining_options &= !ZX_BTI_PERM_WRITE;
    }
    if (remaining_options & ZX_BTI_PERM_EXECUTE) != 0 {
        // Note: We check ZX_RIGHT_READ instead of ZX_RIGHT_EXECUTE
        // here because the latter applies to execute permission of
        // the host CPU, whereas ZX_BTI_PERM_EXECUTE applies to
        // transactions initiated by the bus device.
        if (vmo_rights & ZX_RIGHT_READ) == 0 {
            return Err(Status::ACCESS_DENIED);
        }
        iommu_perms |= IOMMU_FLAG_PERM_EXECUTE;
        remaining_options &= !ZX_BTI_PERM_EXECUTE;
    }
    if !((remaining_options & ZX_BTI_COMPRESS != 0) && (remaining_options & ZX_BTI_CONTIGUOUS != 0))
    {
        if (remaining_options & ZX_BTI_COMPRESS) != 0 {
            compress_results = true;
            remaining_options &= !ZX_BTI_COMPRESS;
        }
        if (remaining_options & ZX_BTI_CONTIGUOUS) != 0 && vmo_dispatcher.vmo().is_contiguous() {
            contiguous = true;
            remaining_options &= !ZX_BTI_CONTIGUOUS;
        }
    }
    if remaining_options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let (new_pmt_handle, new_pmt_rights) =
        bti_dispatcher.pin(vmo_dispatcher.vmo().clone(), offset, size, iommu_perms)?;

    // Until `clear_kernel_cleanup_needed()` is called below, `new_pmt_handle`
    // owns the responsibility for unpinning on error: if anything fails before
    // or during handle allocation, dropping `new_pmt_handle` invokes
    // `on_zero_handles()`, which unpins the PMT before notifying it of zero
    // handles so that it is not placed on the BTI's quarantine list.

    // Based on the passed in options, determine what size chunks we are going to report to the
    // user, and how many of those there will be.
    let (target_contig, expected_addrs) = if compress_results {
        let min_contig = bti_dispatcher.minimum_contiguity() as usize;
        let expected = (size as usize).div_ceil(min_contig);
        (min_contig, expected)
    } else if contiguous {
        (size as usize, 1)
    } else {
        (page::SIZE, size as usize / page::SIZE)
    };

    if addrs_count != expected_addrs {
        return Err(Status::INVALID_ARGS);
    }

    const {
        assert!(
            core::mem::size_of::<dev_vaddr_t>() == core::mem::size_of::<zx_paddr_t>(),
            "mismatched types"
        );
    };

    write_pin_addrs(
        size,
        target_contig,
        addrs.reinterpret::<dev_vaddr_t>(),
        addrs_count,
        |query_offset, remain| new_pmt_handle.dispatcher().query_address(query_offset, remain),
    )?;

    // Allocate the `HandleOwner` before clearing `kernel_cleanup_needed` and
    // before adding the handle to the process's handle table:
    // 1. `HandleOwner::make` is the only fallible step. If it fails for want of
    //    memory, `new_pmt_handle` is dropped inside `HandleOwner::make` while
    //    `kernel_cleanup_needed` is still `true`, so `on_zero_handles()` unpins
    //    the PMT rather than quarantining it.
    // 2. Once `HandleOwner::make` succeeds, `clear_kernel_cleanup_needed()` must
    //    run *before* `up.handle_table().add_handle(handle_owner)` publishes the
    //    handle to the process's handle table (at which point another thread in
    //    the process could close the handle and trigger `on_zero_handles()`,
    //    which must quarantine the PMT if `zx_pmt_unpin` was not called).
    // 3. Downcasting `handle_owner.dispatcher()` avoids cloning the `RefPtr`
    //    across `HandleOwner::make` (which moves the `RefPtr` out of
    //    `new_pmt_handle` with no extra atomic reference count updates).
    let Some(handle_owner) = HandleOwner::make(new_pmt_handle, new_pmt_rights) else {
        // See comment in `ProcessDispatcher::MakeAndAddHandle`.
        pmm::node().report_alloc_failure(pmm::AllocFailure {
            r#type: pmm::AllocFailureType::Handle,
            size: 1,
            free_count: 0,
        });
        return Err(Status::NO_MEMORY);
    };
    handle_owner
        .dispatcher()
        .downcast::<PinnedMemoryTokenDispatcher>()
        .unwrap()
        .clear_kernel_cleanup_needed();
    let handle = up.handle_table().map_handle_to_value(&handle_owner);
    up.handle_table().add_handle(handle_owner);
    *pmt = handle;
    Ok(())
}

/// Kernel unit tests for BTI syscall helpers.
#[cfg(ktest)]
#[unittest::suite(name = "bti_syscall_tests")]
mod tests {
    use super::{
        BufferedUserOutPtr, PIN_ADDR_BATCH_SIZE, QueryAddressResult, dev_vaddr_t, write_pin_addrs,
    };
    use crate::user_copy::UserOutPtr;
    use crate::user_memory::UserMemory;
    use unittest::{expect_eq, expect_err, expect_ok, expect_true, unwrap_ok, unwrap_some};
    use zx_status::Status;

    /// Sentinel value written to output slots to detect unintended overwrites.
    const SENTINEL: dev_vaddr_t = 0xDEAD_BEEF_CAFE_BABE;

    /// Returns a deterministic non-zero test device virtual address for slot `i`.
    fn addr_for_index(i: usize) -> dev_vaddr_t {
        (i as dev_vaddr_t + 1) * 0x1000
    }

    /// Writes `total` test addresses using an `N`-entry buffer, optionally failing at `fail_at`.
    ///
    /// Marked `#[inline(never)]` to keep each call's `[MaybeUninit<dev_vaddr_t>; N]` buffer in its
    /// own stack frame rather than accumulating multiple batch buffers in a caller `#[test]` frame.
    #[inline(never)]
    fn write_addrs_failing_at<const N: usize>(
        out: UserOutPtr<dev_vaddr_t>,
        total: usize,
        fail_at: Option<usize>,
    ) -> Result<(), Status> {
        BufferedUserOutPtr::<dev_vaddr_t, N>::with(out, |buffered| {
            for i in 0..total {
                if fail_at == Some(i) {
                    return Err(Status::INVALID_ARGS);
                }
                buffered.write(addr_for_index(i))?;
            }
            Ok(())
        })
    }

    /// Invokes [`write_pin_addrs`] in its own stack frame so caller `#[test]` functions do not
    /// accumulate multiple 256-byte batch buffers.
    #[inline(never)]
    fn run_write_pin_addrs(
        size: u64,
        target_contig: usize,
        addrs: UserOutPtr<dev_vaddr_t>,
        addrs_count: usize,
        query_address: impl FnMut(u64, usize) -> Result<QueryAddressResult, Status>,
    ) -> Result<(), Status> {
        write_pin_addrs(size, target_contig, addrs, addrs_count, query_address)
    }

    /// Dropping BufferedUserOutPtr with unflushed elements on mid-loop error does not panic.
    #[test]
    fn buffered_user_out_ptr_drop_unflushed_mid_loop_error() {
        let mem = unwrap_some!(UserMemory::create(page::SIZE));
        unwrap_ok!(mem.commit_and_map(0..page::SIZE));

        // 1. Mid-batch failure before any full batch is flushed (0 < index < N): nothing is
        //    written to user memory and dropping `BufferedUserOutPtr` does not panic.
        for i in 0..8 {
            unwrap_ok!(mem.put(SENTINEL, i));
        }
        expect_err!(write_addrs_failing_at::<4>(mem.user_out(), 8, Some(2)), Status::INVALID_ARGS);
        for i in 0..8 {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), SENTINEL);
        }

        // 2. Mid-batch failure after one full batch of `PIN_ADDR_BATCH_SIZE` addresses has already
        //    been flushed: the first batch is present in user memory, the partial second batch is
        //    discarded without panicking, and trailing slots remain untouched.
        const TOTAL: usize = PIN_ADDR_BATCH_SIZE + 8;
        const FAIL_AT: usize = PIN_ADDR_BATCH_SIZE + 3;
        for i in 0..TOTAL {
            unwrap_ok!(mem.put(SENTINEL, i));
        }
        expect_err!(
            write_addrs_failing_at::<PIN_ADDR_BATCH_SIZE>(mem.user_out(), TOTAL, Some(FAIL_AT)),
            Status::INVALID_ARGS
        );
        for i in 0..PIN_ADDR_BATCH_SIZE {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), addr_for_index(i));
        }
        for i in PIN_ADDR_BATCH_SIZE..TOTAL {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), SENTINEL);
        }
    }

    /// BufferedUserOutPtr::with flushes empty, partial, exact, and multi-batch writes.
    #[test]
    fn buffered_user_out_ptr_with_flushes_all_counts() {
        // Zero writes: final flush in `with` is a no-op even on a null `UserOutPtr` and forwards
        // the closure's return value.
        let count =
            unwrap_ok!(BufferedUserOutPtr::<dev_vaddr_t, 4>::with(UserOutPtr::default(), |_| {
                Ok(0usize)
            }));
        expect_eq!(count, 0);

        let mem = unwrap_some!(UserMemory::create(page::SIZE));
        unwrap_ok!(mem.commit_and_map(0..page::SIZE));

        for &count in &[3usize, 4, 11] {
            for i in 0..(count + 1) {
                unwrap_ok!(mem.put(SENTINEL, i));
            }
            expect_ok!(write_addrs_failing_at::<4>(mem.user_out(), count, None));
            for i in 0..count {
                expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), addr_for_index(i));
            }
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(count)), SENTINEL);
        }

        // Multi-batch write using `sys_bti_pin`'s batch size.
        const MULTI_BATCH_COUNT: usize = PIN_ADDR_BATCH_SIZE + 5;
        for i in 0..(MULTI_BATCH_COUNT + 1) {
            unwrap_ok!(mem.put(SENTINEL, i));
        }
        expect_ok!(write_addrs_failing_at::<PIN_ADDR_BATCH_SIZE>(
            mem.user_out(),
            MULTI_BATCH_COUNT,
            None
        ));
        for i in 0..MULTI_BATCH_COUNT {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), addr_for_index(i));
        }
        expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(MULTI_BATCH_COUNT)), SENTINEL);
    }

    /// Flush failures in BufferedUserOutPtr::write and with return errors without panicking.
    #[test]
    fn buffered_user_out_ptr_flush_errors_propagate_cleanly() {
        // Partial batch failing during final flush in `with` on a null pointer.
        let mut reached_final_flush = false;
        expect_err!(
            BufferedUserOutPtr::<dev_vaddr_t, 4>::with(UserOutPtr::default(), |buffered| {
                buffered.write(0x1000)?;
                buffered.write(0x2000)?;
                reached_final_flush = true;
                Ok(())
            }),
            Status::INVALID_ARGS
        );
        expect_true!(reached_final_flush);

        // Full batch failing inside the N-th `write()` on a null pointer.
        let mut writes_completed = 0usize;
        expect_err!(
            BufferedUserOutPtr::<dev_vaddr_t, 4>::with(UserOutPtr::default(), |buffered| {
                for i in 0..3 {
                    buffered.write(addr_for_index(i))?;
                    writes_completed += 1;
                }
                buffered.write(addr_for_index(3))
            }),
            Status::INVALID_ARGS
        );
        expect_eq!(writes_completed, 3);

        // Read-only user mapping failing during both final flush and full-batch `write()`.
        let mem = unwrap_some!(UserMemory::create(page::SIZE));
        unwrap_ok!(mem.commit_and_map(0..page::SIZE));
        unwrap_ok!(mem.make_rx());

        expect_err!(write_addrs_failing_at::<4>(mem.user_out(), 2, None), Status::INVALID_ARGS);
        expect_err!(write_addrs_failing_at::<4>(mem.user_out(), 4, None), Status::INVALID_ARGS);
    }

    /// write_pin_addrs coalesces contiguous PMT ranges across batches and handles a partial tail.
    #[test]
    fn write_pin_addrs_coalesces_ranges_and_handles_tail() {
        let mem = unwrap_some!(UserMemory::create(page::SIZE));
        unwrap_ok!(mem.commit_and_map(0..page::SIZE));

        const TARGET_CONTIG: usize = 2 * page::SIZE;
        const ADDRS_COUNT: usize = PIN_ADDR_BATCH_SIZE + 3;
        const TOTAL_PAGES: usize = (ADDRS_COUNT - 1) * 2 + 1;
        const TOTAL_SIZE: u64 = (TOTAL_PAGES * page::SIZE) as u64;
        const BASE_0: dev_vaddr_t = 0x1000_0000;
        const BASE_1: dev_vaddr_t = 0x2000_0000;
        const BASE_2: dev_vaddr_t = 0x3000_0000;

        for i in 0..(ADDRS_COUNT + 1) {
            unwrap_ok!(mem.put(SENTINEL, i));
        }

        let mut queries = [(0u64, 0usize); 4];
        let mut query_count = 0usize;
        expect_ok!(run_write_pin_addrs(
            TOTAL_SIZE,
            TARGET_CONTIG,
            mem.user_out(),
            ADDRS_COUNT,
            |query_offset, remain| {
                let idx = query_count;
                query_count += 1;
                queries[idx] = (query_offset, remain);
                match idx {
                    // Covers entries 0..3 (6 pages).
                    0 => Ok(QueryAddressResult { device_vaddr: BASE_0, size: 6 * page::SIZE }),
                    // Covers entries 3..33 (60 pages, crossing the batch boundary at 32).
                    1 => Ok(QueryAddressResult { device_vaddr: BASE_1, size: 60 * page::SIZE }),
                    // Covers entry 33 (2 pages) plus tail entry 34 (1 page).
                    2 => Ok(QueryAddressResult { device_vaddr: BASE_2, size: 3 * page::SIZE }),
                    _ => Err(Status::INTERNAL),
                }
            },
        ));

        expect_eq!(query_count, 3);
        expect_eq!(queries[0].0, 0);
        expect_eq!(queries[0].1, TOTAL_PAGES * page::SIZE);
        expect_eq!(queries[1].0, (6 * page::SIZE) as u64);
        expect_eq!(queries[1].1, (TOTAL_PAGES - 6) * page::SIZE);
        expect_eq!(queries[2].0, (66 * page::SIZE) as u64);
        expect_eq!(queries[2].1, 3 * page::SIZE);

        for i in 0..ADDRS_COUNT {
            let expected = if i < 3 {
                BASE_0 + (i * TARGET_CONTIG) as dev_vaddr_t
            } else if i < 33 {
                BASE_1 + ((i - 3) * TARGET_CONTIG) as dev_vaddr_t
            } else {
                BASE_2 + ((i - 33) * TARGET_CONTIG) as dev_vaddr_t
            };
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), expected);
        }
        expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(ADDRS_COUNT)), SENTINEL);
    }

    /// write_pin_addrs discards unflushed entries without panicking when consume_addr fails.
    #[test]
    fn write_pin_addrs_mid_loop_errors_discard_unflushed() {
        let mem = unwrap_some!(UserMemory::create(page::SIZE));
        unwrap_ok!(mem.commit_and_map(0..page::SIZE));

        // 1. `query_address` failing mid-batch after one full `PIN_ADDR_BATCH_SIZE` batch was
        //    already flushed: the first batch remains written and the partial second batch is
        //    discarded without panicking.
        const MULTI_COUNT: usize = PIN_ADDR_BATCH_SIZE + 4;
        const FAIL_INDEX: usize = PIN_ADDR_BATCH_SIZE + 2;
        for i in 0..(MULTI_COUNT + 1) {
            unwrap_ok!(mem.put(SENTINEL, i));
        }
        expect_err!(
            run_write_pin_addrs(
                (MULTI_COUNT * page::SIZE) as u64,
                page::SIZE,
                mem.user_out(),
                MULTI_COUNT,
                |query_offset, _remain| {
                    let page_index = (query_offset as usize) / page::SIZE;
                    if page_index == FAIL_INDEX {
                        return Err(Status::NOT_FOUND);
                    }
                    Ok(QueryAddressResult {
                        device_vaddr: addr_for_index(page_index),
                        size: page::SIZE,
                    })
                },
            ),
            Status::NOT_FOUND
        );
        for i in 0..PIN_ADDR_BATCH_SIZE {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), addr_for_index(i));
        }
        for i in PIN_ADDR_BATCH_SIZE..(MULTI_COUNT + 1) {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), SENTINEL);
        }

        // 2. `expected_contig > range.size` failing mid-batch after buffering two addresses:
        //    nothing is flushed to user memory and dropping `BufferedUserOutPtr` does not panic.
        const CONTIG_COUNT: usize = 4;
        const TARGET_CONTIG: usize = 2 * page::SIZE;
        for i in 0..(CONTIG_COUNT + 1) {
            unwrap_ok!(mem.put(SENTINEL, i));
        }
        expect_err!(
            run_write_pin_addrs(
                (CONTIG_COUNT * TARGET_CONTIG) as u64,
                TARGET_CONTIG,
                mem.user_out(),
                CONTIG_COUNT,
                |query_offset, _remain| {
                    if query_offset == 0 {
                        // Satisfies entries 0 and 1.
                        Ok(QueryAddressResult {
                            device_vaddr: 0x4000_0000,
                            size: 2 * TARGET_CONTIG,
                        })
                    } else {
                        // Smaller than `TARGET_CONTIG`, triggering `Status::INVALID_ARGS`.
                        Ok(QueryAddressResult { device_vaddr: 0x5000_0000, size: page::SIZE })
                    }
                },
            ),
            Status::INVALID_ARGS
        );
        for i in 0..(CONTIG_COUNT + 1) {
            expect_eq!(unwrap_ok!(mem.get::<dev_vaddr_t>(i)), SENTINEL);
        }
    }
}
