// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::evictor::Evictor;
use super::pmm_arena::PmmArenaInfo;
use super::pmm_checker::PmmChecker;
pub use super::pmm_node::{ALLOC_FLAG_ANY, ALLOC_FLAG_CAN_WAIT, AllocFailure, AllocFailureType};
use super::pmm_node::{PmmNode, PmmOptDelayReuse};
#[cfg(target_arch = "x86_64")]
use crate::arch_rs::feature::x86_has_hypervisor;
use crate::counters::define_kcounter;
use crate::kernel::types::PAddr;
use crate::vm::page::{VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::page_queues::PageQueues;
use crate::vm::vm::vm_ktrace_duration;
use boot_options::{BootOptions, CheckerEnable};
use core::mem::MaybeUninit;
use core::pin::Pin;
use debug::dprintf;
use kprint::kprint;
use lazy_init::{LazyInit, NoCheck};
use page_bindings::vm_page_t;
use zx_status::Status;
use zx_types::zx_status_t;

// Number of bytes available in the PMM after kernel init, but before userspace init.
define_kcounter!(BOOT_MEMORY_BYTES, "boot.memory.post_init_free_bytes", Sum);

/// The (currently) one and only PMM node, constructed by `pmm_node_init`.
static PMM_NODE: LazyInit<PmmNode, NoCheck> = LazyInit::uninit();

/// Constructs the global `PmmNode`.
///
/// The earliest use of the PmmNode is by `platform_early_init`, which initializes the PMM checker
/// and then the PMM itself, so this runs at the init level just before that.
fn pmm_node_init(_level: init::LkInitLevel) {
    // SAFETY: This is a primary CPU only init hook and so runs exactly once, during single-threaded
    // early boot, and nothing accesses the PmmNode before this hook.
    unsafe { Pin::static_ref(&PMM_NODE).init_pin(PmmNode::init()) }.unwrap();
}
init::lk_init_hook!(
    pmm_node_init,
    pmm_node_init,
    init::LkInitLevel(init::LK_INIT_LEVEL_PLATFORM_EARLY.0 - 1)
);

/// Check that if random should wait is requested that this is a debug build with assertions as it
/// is currently assumed that enabling this in a non-debug build would be a mistake that should be
/// caught.
fn pmm_init_alloc_random_should_wait(_level: init::LkInitLevel) {
    if BootOptions::get().pmm_alloc_random_should_wait {
        if !cfg!(debug_assertions) {
            panic!("pmm: alloc-random-should-wait requires a build with debug assertions");
        }
        kprint!("pmm: alloc-random-should-wait enabled\n");
        node().seed_random_should_wait();
    }
}
init::lk_init_hook!(
    pmm_init_alloc_random_should_wait,
    pmm_init_alloc_random_should_wait,
    init::LK_INIT_LEVEL_LAST
);

fn pmm_fill_free_pages(_level: init::LkInitLevel) {
    node().fill_free_pages_and_arm();
}
init::lk_init_hook!(pmm_fill, pmm_fill_free_pages, init::LK_INIT_LEVEL_VM);

/// Allocates a single physical page from the PMM.
pub fn alloc_page(flags: u32) -> Result<(VmPagePtr, PAddr), Status> {
    vm_ktrace_duration!(3, "pmm_alloc_page", "count" => 1u64, "alloc_flags" => flags);
    let page = node().alloc_page(flags)?;
    // SAFETY: The page was just allocated and is exclusively owned by this function.
    let paddr = unsafe { page.paddr() };
    Ok((page, paddr))
}

/// Frees a single physical page back to the PMM.
///
/// # Safety
///
/// Caller must transfer exclusive ownership of `page`, which must be a valid
/// allocated, non-loaned (`!page.is_loaned()`) PMM page that has not already
/// been freed and is not in any page queue.
pub unsafe fn free_page(page: VmPagePtr) {
    vm_ktrace_duration!(3, "pmm_free_page");
    // SAFETY: Caller guarantees exclusive ownership of a valid allocated, non-loaned PMM page.
    unsafe { node().free_page(page, PmmOptDelayReuse::Default) };
}

/// Frees every page on `list` back to the PMM, emptying the list.  The PMM lock
/// is taken once for the whole list rather than once per page.
///
/// # Safety
///
/// Caller must transfer exclusive ownership of every page on `list`, each of
/// which must be a valid allocated, non-loaned (`!page.is_loaned()`) PMM page
/// that has not already been freed.
pub unsafe fn free_list(list: Pin<&mut VmPageDoublyLinkedList>) {
    if list.is_empty() {
        return;
    }
    vm_ktrace_duration!(3, "pmm_free");
    // SAFETY: The caller guarantees exclusive ownership of valid allocated, non-loaned pages.
    unsafe { node().free_list(list, PmmOptDelayReuse::Default) };
}

/// Converts a physical address to a `VmPagePtr`.
pub fn paddr_to_vm_page(paddr: PAddr) -> Option<VmPagePtr> {
    node().paddr_to_page(paddr)
}

/// Allocate count pages of physical memory, adding to the tail of the passed list.
/// The list must be initialized.
/// Note that if PMM_ALLOC_FLAG_CAN_WAIT is passed in then this could always return
/// ZX_ERR_SHOULD_WAIT. Since there is no way to wait until an arbitrary number of pages can be
/// allocated (see comment on |pmm_wait_till_should_retry_single_alloc|) passing
/// PMM_ALLOC_FLAG_CAN_WAIT here should be used as an optimistic fast path, and the caller should
/// have a fallback of allocating single pages.
pub fn alloc_pages(
    count: usize,
    flags: u32,
    list: Pin<&mut VmPageDoublyLinkedList>,
) -> Result<(), Status> {
    vm_ktrace_duration!(3, "pmm_alloc_pages", "count" => count as u64, "alloc_flags" => flags);
    node().alloc_pages(count, flags, list)
}

/// Allocate a run of contiguous pages, aligned on log2 byte boundary (0-31).
/// Return the base address of the run in the physical address pointer and
/// append the allocate page structures to the tail of the passed in list.
pub fn alloc_contiguous(
    count: usize,
    flags: u32,
    align_log2: u8,
    mut list: Pin<&mut VmPageDoublyLinkedList>,
) -> Result<PAddr, Status> {
    vm_ktrace_duration!(
        3,
        "pmm_alloc_contiguous",
        "count" => count as u64,
        "alloc_flags" => flags
    );
    // If we're called with a single page, just fall through to the regular allocation routine.
    if count == 1 && (align_log2 as usize) <= page::SHIFT {
        core::hint::cold_path();
        let page = node().alloc_page(flags)?;
        // SAFETY: The page was just allocated and is exclusively owned by this function.
        let paddr = unsafe { page.paddr() };
        // SAFETY: The page was just allocated, is exclusively owned by this function and is not in
        // any other list.
        unsafe { list.as_mut().push_back_raw(page.as_non_null()) };
        return Ok(paddr);
    }
    node().alloc_contiguous(count, flags, align_log2, list)
}

/// Returns the number of physical memory arenas.
pub fn num_arenas() -> usize {
    node().num_arenas()
}

/// Fills `buffer` with `PmmArenaInfo` objects starting at `offset` arena, ordered by base address.
/// For example, passing an `offset` of 1 would skip the 1st arena.
///
/// Returns `OUT_OF_RANGE` if `offset` would yield an invalid range or `buffer` is too large.
pub fn get_arena_info(
    offset: usize,
    buffer: &mut [MaybeUninit<PmmArenaInfo>],
) -> Result<&mut [PmmArenaInfo], Status> {
    node().get_arena_info(offset, buffer)
}

/// Returns the static `PageQueues` instance associated with the PMM.
#[inline]
pub fn page_queues() -> &'static PageQueues {
    node().page_queues()
}

/// Returns the static `Evictor` instance associated with the PMM.
#[inline]
pub fn evictor() -> &'static Evictor {
    node().evictor()
}

/// Returns a reference to the global `PmmNode` instance.
///
/// Must not be called before the `pmm_node_init` init hook has run, which happens just before
/// `platform_early_init`.
#[inline]
pub fn node() -> &'static PmmNode {
    // SAFETY: `PMM_NODE` is constructed by `pmm_node_init` before its first use in
    // `platform_early_init`, and is never destroyed.
    unsafe { PMM_NODE.get_unchecked() }
}

/// Returns the total number of bytes across all physical memory arenas.
#[inline]
pub fn count_total_bytes() -> u64 {
    node().count_total_bytes()
}

/// Prints physical page borrowing related stats to the console.
pub fn print_physical_page_borrowing_stats() {
    const MB: u64 = 1024 * 1024;
    const PAGE_SIZE: u64 = page::SIZE as u64;
    let free_pages = node().count_free_pages();
    let loaned_free_pages = node().count_loaned_free_pages();
    let loaned_pages = node().count_loaned_pages();
    let loan_cancelled_pages = node().count_loan_cancelled_pages();
    let total_bytes = count_total_bytes();
    let used_loaned_pages = node().count_loaned_not_free_pages();
    kprint!(
        "PPB stats:\n  free pages: {} free MiB: {}\n  loaned free pages: {} loaned free MiB: {}\n  \
         loaned pages: {} loaned MiB: {}\n  used loaned pages: {} used loaned MiB: {}\n  \
         loan cancelled pages: {} loan cancelled MIB: {}\n  \
         total physical pages: {} total MiB: {}\n",
        free_pages,
        free_pages * PAGE_SIZE / MB,
        loaned_free_pages,
        loaned_free_pages * PAGE_SIZE / MB,
        loaned_pages,
        loaned_pages * PAGE_SIZE / MB,
        used_loaned_pages,
        used_loaned_pages * PAGE_SIZE / MB,
        loan_cancelled_pages,
        loan_cancelled_pages * PAGE_SIZE / MB,
        total_bytes / PAGE_SIZE,
        total_bytes / MB
    );
}

/// Initializes the PMM with the provided normalized memory ranges. This in
/// particular initializes its arenas and wires any previously allocated special
/// subranges or holes.
///
/// # Safety
///
/// Must only be called once during single-threaded early boot before the PMM is
/// used, and `ranges` must be normalized and accurately describe the system's
/// physical memory map.
pub unsafe fn init(ranges: &[memalloc::Range]) -> Result<(), Status> {
    // SAFETY: Caller guarantees early-boot single-threaded preconditions and a normalized, valid
    // physical memory map.
    unsafe { node().init_from_memory_ranges(ranges) }
}

/// Ends the PMM's role within the context of phys handoff: it frees all physical
/// memory temporarily used for the hand-off from physboot.
///
/// # Safety
///
/// Must be called at most once after hand-off from physboot completes, and not
/// concurrently with [`init`].
pub unsafe fn end_handoff() {
    // SAFETY: The caller guarantees the hand-off preconditions of `PmmNode::end_handoff`.
    unsafe { node().end_handoff() };
}

/// Should be called after the kernel command line has been parsed.
///
/// # Safety
///
/// Must only be called during early boot, before the kernel console can access the PMM checker.
pub unsafe fn pmm_checker_init_from_cmdline() {
    let enabled = match BootOptions::get().pmm_checker_enabled {
        CheckerEnable::True => true,
        CheckerEnable::False => false,
        CheckerEnable::Auto => {
            #[cfg(target_arch = "x86_64")]
            {
                if x86_has_hypervisor() {
                    kprint!(
                        "PMM: Checker enabled set to auto and hypervisor detected, disabling\n"
                    );
                    false
                } else {
                    kprint!(
                        "PMM: Checker enabled set to auto and hypervisor not detected, enabling\n"
                    );
                    true
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                kprint!(
                    "PMM: Checker enabled set to auto on platform without hypervisor detection, \
                     disabling\n"
                );
                false
            }
        }
    };
    if enabled {
        let mut fill_size = Option::<u64>::from(BootOptions::get().pmm_checker_fill_size)
            .map_or(page::SIZE, |size| size as usize);
        if !PmmChecker::is_valid_fill_size(fill_size) {
            kprint!(
                "PMM: value from {:s} is invalid ({}), using kPageSize instead\n",
                boot_options::PMM_CHECKER_FILL_SIZE_NAME,
                fill_size
            );
            fill_size = page::SIZE;
        }

        // SAFETY: `fill_size` satisfies `PmmChecker::is_valid_fill_size`, as checked above, and the
        // caller guarantees this runs during early boot, before the kernel console can access the
        // PMM checker.
        unsafe {
            node().enable_free_page_filling(fill_size, BootOptions::get().pmm_checker_action)
        };
    }
}

/// C++ entry point for [`pmm_checker_init_from_cmdline`].
///
/// # Safety
///
/// Must only be called during early boot, before the kernel console can access the PMM checker.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_pmm_checker_init_from_cmdline() {
    // SAFETY: The caller guarantees the early-boot requirements of `pmm_checker_init_from_cmdline`.
    unsafe { pmm_checker_init_from_cmdline() };
}

fn pmm_boot_memory(_level: init::LkInitLevel) {
    // Track the amount of free memory available in the PMM after kernel init, but before
    // userspace starts.
    //
    // We record this in a kcounter to be tracked by build infrastructure over time.
    let free_bytes = node().count_free_pages() * page::SIZE as u64;
    dprintf!(INFO, "Free memory after kernel init: {} bytes.\n", free_bytes);
    BOOT_MEMORY_BYTES.set(free_bytes);
}
init::lk_init_hook!(
    pmm_boot_memory,
    pmm_boot_memory,
    init::LkInitLevel(init::LK_INIT_LEVEL_USER.0 - 1)
);

/// Synchronously walk the PMM's free list and validate each page.  This is an incredibly expensive
/// operation and should only be used for debugging purposes.
pub fn pmm_checker_check_all_free_pages() {
    node().check_all_free_pages();
}

/// Returns the `VmCompression` instance if compression is enabled.
#[inline]
pub fn get_page_compression() -> Option<&'static crate::vm::compression::VmCompression> {
    node().get_page_compression()
}

// C++ entry points, declared in <vm/pmm.h>.

/// Returns the global `PmmNode`; see `Pmm::Node()`.
///
/// The pointee is uninitialized until `pmm_node_init` has run, and must not be dereferenced before
/// then.
#[unsafe(no_mangle)]
extern "C" fn rust_pmm_node() -> *mut PmmNode {
    PMM_NODE.as_ptr()
}

/// C++ entry point for [`init`]; see `pmm_init()`.
///
/// # Safety
///
/// `ranges` must point to `count` valid ranges, and the requirements of [`init`] must be met.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_pmm_init(ranges: *const memalloc::Range, count: usize) -> zx_status_t {
    // SAFETY: The caller guarantees that `ranges` points to `count` valid ranges; the Rust
    // `memalloc::Range` is layout-compatible with the C++ version.
    let ranges = unsafe { zr::slice_from_raw_parts(ranges, count) };
    // SAFETY: The caller guarantees the requirements of `init`.
    match unsafe { init(ranges) } {
        Ok(()) => zx_types::ZX_OK,
        Err(status) => status.into_raw(),
    }
}

/// C++ entry point for [`end_handoff`].
///
/// # Safety
///
/// Must be called at most once after hand-off from physboot completes, and not
/// concurrently with [`init`].
#[unsafe(no_mangle)]
unsafe extern "C" fn pmm_end_handoff() {
    // SAFETY: The caller guarantees the hand-off preconditions of `end_handoff`.
    unsafe { end_handoff() };
}

#[unsafe(no_mangle)]
extern "C" fn pmm_num_arenas() -> usize {
    num_arenas()
}

/// # Safety
///
/// `buffer` must be non-null, properly aligned for `PmmArenaInfo`, and valid for exclusive
/// writes of `buffer_size` bytes.
#[unsafe(no_mangle)]
unsafe extern "C" fn pmm_get_arena_info(
    count: usize,
    i: u64,
    buffer: *mut PmmArenaInfo,
    buffer_size: usize,
) -> zx_status_t {
    let num_arenas = num_arenas();
    let i = i as usize;
    if count == 0 || i >= num_arenas || count > num_arenas - i {
        return zx_types::ZX_ERR_OUT_OF_RANGE;
    }
    if buffer_size < count * core::mem::size_of::<PmmArenaInfo>() {
        return zx_types::ZX_ERR_BUFFER_TOO_SMALL;
    }
    // SAFETY: `count` is non-zero and the caller guarantees that `buffer` is non-null, aligned,
    // and exclusively writable for `buffer_size` bytes, which was checked above to hold `count`
    // elements.
    let buffer = unsafe {
        core::slice::from_raw_parts_mut(buffer.cast::<MaybeUninit<PmmArenaInfo>>(), count)
    };
    match get_arena_info(i, buffer) {
        Ok(_) => zx_types::ZX_OK,
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `list` must be non-null, properly aligned, and point to a valid, initialized, pinned page list
/// to which the caller has exclusive access.
#[unsafe(no_mangle)]
unsafe extern "C" fn pmm_alloc_pages(
    count: usize,
    alloc_flags: u32,
    list: *mut VmPageDoublyLinkedList,
) -> zx_status_t {
    // SAFETY: The caller guarantees that `list` is valid, exclusively accessed, and pinned; the
    // C++ list is never moved.
    let list = unsafe { Pin::new_unchecked(&mut *list) };
    match alloc_pages(count, alloc_flags, list) {
        Ok(()) => zx_types::ZX_OK,
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `pa` must be non-null, properly aligned, valid for writes, and disjoint from `*list`. `list`
/// must be non-null, properly aligned, and point to a valid, initialized, pinned page list to
/// which the caller has exclusive access.
#[unsafe(no_mangle)]
unsafe extern "C" fn pmm_alloc_contiguous(
    count: usize,
    alloc_flags: u32,
    align_log2: u8,
    pa: *mut u64,
    list: *mut VmPageDoublyLinkedList,
) -> zx_status_t {
    // SAFETY: The caller guarantees that `list` is valid, exclusively accessed, and pinned; the
    // C++ list is never moved.
    let list = unsafe { Pin::new_unchecked(&mut *list) };
    match alloc_contiguous(count, alloc_flags, align_log2, list) {
        Ok(paddr) => {
            // SAFETY: The caller guarantees that `pa` is non-null, aligned, valid for writes, and
            // disjoint from `*list`.
            unsafe { *pa = paddr.0 as u64 };
            zx_types::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `page` must be a valid page in the `WIRED` state, and the caller must transfer exclusive
/// ownership of `page`.
#[unsafe(no_mangle)]
unsafe extern "C" fn pmm_unwire_page(page: *mut vm_page_t) {
    // SAFETY: The caller guarantees that `page` is a valid page.
    let page = unsafe { VmPagePtr::from_ffi(page) }.expect("null page");
    node().unwire_page(page);
}

#[cfg(sanitize = "address")]
#[unsafe(no_mangle)]
extern "C" fn pmm_asan_poison_all_free_pages() {
    node().poison_all_free_pages();
}

/// Kernel console `pmm` command implementation.
#[cfg(console_enabled)]
mod console {
    use super::{node, pmm_checker_check_all_free_pages};
    use crate::console::{CMD_AVAIL_ALWAYS, CMD_FLAG_PANIC, CmdArgs, static_command};
    use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::platform_rs::timer::{DurationMono, InstantUnknown, current_mono_time};
    use crate::vm::pmm_checker::{CheckFailAction, PmmChecker};
    use crate::vm::vm_aspace::VmAspace;
    use core::ffi::{c_int, c_void};
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, Ordering};
    use kprint::kprintln;
    use ksync::KOnceCell;
    use zx_status::Status;

    crate::ksync_kernel::declare_singleton_mutex!(DumpFreeMemLock);

    /// Repeating timer used by `k pmm free` to periodically dump the free memory count.
    static DUMP_FREE_MEM_TIMER: KOnceCell<Timer, DumpFreeMemLock> = KOnceCell::new();

    /// Returns a pinned reference to the lazily-initialized `DUMP_FREE_MEM_TIMER`.
    fn dump_free_mem_timer() -> Pin<&'static Timer> {
        ksync::lock!(let mut guard = DumpFreeMemLock::get().lock());
        // SAFETY: `guard.token()` / `guard.token_mut()` belongs to the `DumpFreeMemLock` singleton
        // guarding `DUMP_FREE_MEM_TIMER`, which is pinned in static memory and never mutated or
        // dropped after initialization. Checking `get` before `get_or_pin_init` ensures we never
        // form `&mut Timer` once a shared `&'static Timer` reference has been handed out.
        unsafe {
            let timer: *const Timer = match DUMP_FREE_MEM_TIMER.get(guard.token()) {
                Some(timer) => timer,
                None => DUMP_FREE_MEM_TIMER
                    .get_or_pin_init(Timer::init(ZX_CLOCK_MONOTONIC), guard.token_mut())
                    .unwrap(),
            };
            Pin::new_unchecked(&*timer)
        }
    }

    /// Timer callback that dumps the PMM free count and rearms itself after 1 second.
    ///
    /// # Safety
    ///
    /// `t` must be non-null and point to the valid, initialized, pinned `Timer` in
    /// `DUMP_FREE_MEM_TIMER` for the duration of the call, invoked by the kernel timer subsystem.
    unsafe extern "C" fn pmm_dump_timer(t: *mut Timer, now: i64, _arg: *mut c_void) {
        let deadline = now.saturating_add(DurationMono::from_seconds(1).into_nanos());
        // SAFETY: `t` points to the pinned `Timer` in `DUMP_FREE_MEM_TIMER`, and `Timer` methods
        // only take `&self` (`Pin<&Timer>`).
        unsafe {
            Pin::new_unchecked(&*t).set_oneshot(deadline, pmm_dump_timer, core::ptr::null_mut());
        }
        node().dump_free();
    }

    /// Configures and arms the PMM free-page checker.
    fn pmm_checker_enable(fill_size: usize, action: CheckFailAction) {
        assert!(PmmChecker::is_valid_fill_size(fill_size));

        // Enable filling of pages going forward.
        // SAFETY: `PmmChecker::is_valid_fill_size(fill_size)` was asserted above, console commands
        // are serialized by the console's command lock, and the checker is only configured during
        // boot (before the console runs) or by this `pmm checker enable` console command.
        if !unsafe { node().enable_free_page_filling(fill_size, action) } {
            kprintln!("Checker already configured, requested fill size and action ignored.");
        }

        // From this point on, pages will be filled when they are freed.  However, the free list
        // may still have a lot of unfilled pages so make a pass over them and fill them all.
        node().fill_free_pages_and_arm();

        // All free pages have now been filled with `fill_size` and the checker is armed.
    }

    /// Returns whether the PMM free-page checker is currently armed.
    fn pmm_checker_is_enabled() -> bool {
        // SAFETY: Console commands are serialized by the console's command lock, and the checker is
        // only armed during boot (before the console runs) or by the console's `pmm checker enable`
        // command, so this cannot race with `PmmChecker::arm`.
        unsafe { node().checker().is_armed() }
    }

    /// Prints the status of the PMM free-page checker to standard output.
    fn pmm_checker_print_status() {
        // SAFETY: Console commands are serialized by the console's command lock, and the checker is
        // only armed during boot (before the console runs) or by the console's `pmm checker enable`
        // command, so this cannot race with `PmmChecker::arm`.
        unsafe { node().checker().print_status_stdout() };
    }

    /// Prints usage information for the `pmm` console command.
    fn cmd_usage(cmd_name: &str, is_panic: bool) -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} dump                                     : dump pmm info ", cmd_name);
        if !is_panic {
            kprintln!(
                "{:s} free                                     : periodically dump free mem count",
                cmd_name
            );
            kprintln!(
                "{:s} drop_user_pt                             : drop all user hardware page \
                 tables",
                cmd_name
            );
            kprintln!(
                "{:s} checker status                           : prints the status of the pmm \
                 checker",
                cmd_name
            );
            kprintln!(
                "{:s} checker enable [<size>] [oops|panic]     : enables the pmm checker with \
                 optional fill size and optional action",
                cmd_name
            );
            kprintln!(
                "{:s} checker check                            : forces a check of all free pages \
                 in the pmm",
                cmd_name
            );
        }
        Status::INTERNAL.into_raw()
    }

    /// Console command handler for `k pmm`.
    ///
    /// # Safety
    ///
    /// `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc` initialized
    /// `CmdArgs` entries for the duration of the call.
    unsafe extern "C" fn cmd_pmm(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        let is_panic = (flags & CMD_FLAG_PANIC) != 0;
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let name = args[0].as_str();

        if args.len() < 2 {
            kprintln!("not enough arguments");
            return cmd_usage(name, is_panic);
        }

        if args[1].as_str() == "dump" {
            node().dump(is_panic);
        } else if is_panic {
            // No other operations will work during a panic.
            kprintln!("Only the \"arenas\" command is available during a panic.");
            return cmd_usage(name, is_panic);
        } else if args[1].as_str() == "free" {
            static SHOW_MEM: AtomicBool = AtomicBool::new(false);

            if !SHOW_MEM.load(Ordering::Relaxed) {
                kprintln!("pmm free: issue the same command to stop.");
                let deadline = current_mono_time() + DurationMono::from_seconds(1);
                let slack =
                    TimerSlack::new(DurationMono::from_millis(20).into(), SlackMode::Center);
                let slack_deadline = Deadline::new(InstantUnknown(deadline.0), slack);
                // SAFETY: `DUMP_FREE_MEM_TIMER` is pinned in static memory and `pmm_dump_timer` is
                // a valid callback.
                unsafe {
                    dump_free_mem_timer().set_deadline(
                        &slack_deadline,
                        pmm_dump_timer,
                        core::ptr::null_mut(),
                    );
                }
                SHOW_MEM.store(true, Ordering::Relaxed);
            } else {
                dump_free_mem_timer().cancel();
                SHOW_MEM.store(false, Ordering::Relaxed);
            }
        } else if args[1].as_str() == "drop_user_pt" {
            VmAspace::drop_all_user_page_tables();
        } else if args[1].as_str() == "checker" {
            if args.len() < 3 || args.len() > 5 {
                return cmd_usage(name, is_panic);
            }
            if args[2].as_str() == "status" {
                pmm_checker_print_status();
            } else if args[2].as_str() == "enable" {
                let mut fill_size = page::SIZE;
                let mut action = PmmChecker::DEFAULT_ACTION;
                if args.len() >= 4 {
                    fill_size = args[3].arg_uint as usize;
                    if !PmmChecker::is_valid_fill_size(fill_size) {
                        kprintln!(
                            "error: fill size must be a multiple of 8 and be between 8 and \
                             kPageSize, inclusive"
                        );
                        return Status::INTERNAL.into_raw();
                    }
                }
                if args.len() == 5 {
                    action = match args[4].as_str() {
                        "oops" => CheckFailAction::Oops,
                        "panic" => CheckFailAction::Panic,
                        _ => {
                            kprintln!("error: invalid action");
                            return Status::INTERNAL.into_raw();
                        }
                    };
                }
                pmm_checker_enable(fill_size, action);
                // No need to print status as enabling automatically prints status.
            } else if args[2].as_str() == "check" {
                if !pmm_checker_is_enabled() {
                    kprintln!("error: pmm checker is not enabled");
                    return Status::INTERNAL.into_raw();
                }
                kprintln!("checking all free pages...");
                pmm_checker_check_all_free_pages();
                kprintln!("done");
            } else {
                return cmd_usage(name, is_panic);
            }
        } else {
            kprintln!("unknown command");

            return cmd_usage(name, is_panic);
        }

        zx_status::sys::ZX_OK
    }

    static_command!(
        PMM,
        c"pmm".as_ptr(),
        c"physical memory manager".as_ptr(),
        cmd_pmm,
        CMD_AVAIL_ALWAYS
    );
}

/// Unit tests for PMM.
#[cfg(ktest)]
#[unittest::suite(name = "pmm_rust")]
mod pmm_rust {
    use super::{
        alloc_contiguous, alloc_page, free_list, free_page, node, num_arenas, paddr_to_vm_page,
    };
    use crate::kernel::types::PAddr;
    use crate::vm::physmap::paddr_to_physmap;
    use crate::vm::pmm_arena::PmmArenaInfo;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_err, assert_ge, assert_gt, assert_ne, assert_true, unwrap_ok,
    };
    use zx_status::Status;

    /// Allocates a single page, translates it to a vm_page_t and frees it.
    #[test]
    fn smoke() {
        let (page, pa) = unwrap_ok!(alloc_page(0), "pmm_alloc single page");
        assert_ne!(pa.0, 0, "pmm_alloc single page");

        let page2 = paddr_to_vm_page(pa);
        assert_true!(page2 == Some(page), "paddr_to_vm_page on single page");

        unsafe { free_page(page) };
    }

    /// Allocates one page and frees it.
    #[test]
    fn alloc_contiguous_one() {
        stack_pin_init!(let list = VmPageDoublyLinkedList::new());
        let count = 1usize;
        let pa = unwrap_ok!(
            alloc_contiguous(count, 0, page::SHIFT as u8, list.as_mut()),
            "pmm_alloc_contiguous returned failure\n"
        );
        assert_eq!(count, list.iter().count(), "pmm_alloc_contiguous list size is wrong");
        assert_ne!(paddr_to_physmap(pa).0, 0);
        unsafe { free_list(list) };
    }

    /// Tests getting arena info.
    #[test]
    fn get_arena_info() {
        let n_arenas = num_arenas();
        assert_gt!(n_arenas, 0);

        let mut oversize_buffer = unwrap_ok!(
            kalloc::Box::<[PmmArenaInfo]>::try_new_uninit_slice(n_arenas + 1)
                .map_err(|_| Status::NO_MEMORY)
        );
        let mut buffer = unwrap_ok!(
            kalloc::Box::<[PmmArenaInfo]>::try_new_uninit_slice(n_arenas)
                .map_err(|_| Status::NO_MEMORY)
        );

        // Asking for none.
        let result = super::get_arena_info(0, &mut buffer[0..0]);
        assert_err!(result, Status::OUT_OF_RANGE);

        // Asking for more than exist.
        let result = super::get_arena_info(0, &mut oversize_buffer[..]);
        assert_err!(result, Status::OUT_OF_RANGE);

        // Attempting to skip them all.
        let result = super::get_arena_info(n_arenas, &mut buffer[0..1]);
        assert_err!(result, Status::OUT_OF_RANGE);

        // Asking for one.
        let result = super::get_arena_info(0, &mut buffer[0..1]);
        let _ = unwrap_ok!(result);

        // Asking for them all.
        let result = super::get_arena_info(0, &mut buffer[..]);
        let buffer = unwrap_ok!(result);

        // See they are in ascending order by base.
        let mut prev = PAddr(0);
        for (i, arena) in buffer.iter().enumerate() {
            if i == 0 {
                assert_ge!(arena.base.0, prev.0);
            } else {
                assert_gt!(arena.base.0, prev.0);
            }
            prev = arena.base;
            assert_gt!(arena.size, 0);
        }
    }

    /// Tests converting between pages and indexes.
    #[test]
    fn page_to_from_index() {
        // Assert that indexes have zero bits, can roundtrip and are distinct for distinct pages.
        let (page0, _pa0) = unwrap_ok!(alloc_page(0), "pmm_alloc single page");
        let index0 = node().page_to_index(page0);
        assert_ne!(0, index0);
        let zero_bits_mask = (1u32 << PmmNode::INDEX_ZERO_BITS) - 1;
        assert_eq!(0, index0 & zero_bits_mask);
        let same_page = unsafe { node().index_to_page(index0) };
        assert_true!(page0 == same_page);
        assert_eq!(unsafe { page0.paddr() }.0, unsafe { node().index_to_paddr(index0).0 });

        let (page1, _pa1) = unwrap_ok!(alloc_page(0), "pmm_alloc single page");
        let index1 = node().page_to_index(page1);
        assert_ne!(0, index1);

        assert_ne!(index0, index1);

        unsafe { free_page(page0) };
        unsafe { free_page(page1) };
    }

    /// Tests AllocFailure type string conversions.
    #[test]
    fn alloc_failure_type_to_str() {
        use crate::vm::pmm_node::{AllocFailure, AllocFailureType};
        assert_true!(AllocFailure::type_to_str(AllocFailureType::None) == "None");
        assert_true!(AllocFailure::type_to_str(AllocFailureType::Pmm) == "PMM");
        assert_true!(AllocFailure::type_to_str(AllocFailureType::Heap) == "Heap");
        assert_true!(AllocFailure::type_to_str(AllocFailureType::Handle) == "Handle");
        assert_true!(AllocFailure::type_to_str(AllocFailureType::Other) == "Other");
        let failure = AllocFailure { r#type: AllocFailureType::Pmm, size: 4096, free_count: 100 };
        assert_true!(failure.type_str() == "PMM");
    }
}
