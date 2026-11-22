// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Zero-cost environment seam (`SmmuEnv`) abstracting PMM page management, physmap conversions,
//! CPU data-cache cleaning, PTE write observation, and `PinnedVmObject` physical range lookups
//! across production kernel builds (`cfg(is_kernel)`) and host tests (`cfg(not(is_kernel))`).

use core::ffi::c_void;
#[cfg(is_kernel)]
use fbl::DoublyLinkedListContainable;
use zx_status::Status;

use super::constants::{ArmSmmuMode, BtiPageLeakReason, InterruptPolarity, InterruptTriggerMode};
use super::smmu_mode::get_smmu_mode_bytes;

/// Raw spinlock type alias: uses the kernel's `TrackedSpinlock` in the kernel and
/// `ksync::RawMutex` in host tests (since `TrackedSpinlock` is only available in the kernel).
#[cfg(is_kernel)]
pub type SmmuRawSpinLock = crate::ksync_kernel::TrackedSpinlock;

/// Raw mutex type alias: uses the kernel's `TrackedMutex` in the kernel and `ksync::RawMutex` in
/// host tests. Generic code (such as `region_alloc`) is parameterized over this type.
#[cfg(is_kernel)]
pub type SmmuRawMutex = crate::ksync_kernel::TrackedMutex;

/// Lock policy for acquiring an [`SmmuRawSpinLock`] from an interrupt handler where interrupts are
/// already disabled (`NoIrqSavePolicy` in kernel, default mutex policy on host).
#[cfg(is_kernel)]
pub type SmmuNoIrqSavePolicy = crate::ksync_kernel::NoIrqSavePolicy;

pub use debug::DRIVER_OOPS_BANNER;

/// Non-null handle to a physical page descriptor (`VmPage` in kernel, mock `vm_page_t` on host).
#[cfg_attr(is_kernel, repr(transparent))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmmuPage {
    #[cfg(is_kernel)]
    ptr: crate::vm::page::VmPagePtr,
}

impl SmmuPage {
    /// Constructs an `SmmuPage` from a raw `vm_page_t` pointer, returning `None` if `ptr` is null.
    ///
    /// # Safety
    /// If non-null, `ptr` must point to a valid, live `vm_page_t` managed by the kernel PMM or host
    /// `PmmMock`.
    #[inline]
    pub unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `ptr` is null or a valid `VmPage` pointer.
            unsafe { crate::vm::page::VmPagePtr::from_raw(ptr.cast()).map(|ptr| Self { ptr }) }
        }
    }

    /// Returns the raw `vm_page_t` pointer.
    #[inline]
    pub fn as_raw(self) -> *mut c_void {
        #[cfg(is_kernel)]
        {
            self.ptr.as_raw().cast()
        }
    }

    /// Returns the physical address of this page.
    #[inline]
    pub fn paddr(self) -> u64 {
        #[cfg(is_kernel)]
        {
            // SAFETY: In the kernel, `VmPage` descriptors reside in static PMM arenas for the
            // lifetime of the system, and `paddr_priv` is initialized once at arena boot and never
            // mutated thereafter. Reading `paddr()` through `self.ptr` therefore cannot race or
            // dereference freed memory.
            unsafe { u64::from(self.ptr.as_non_null().as_ref().paddr()) }
        }
    }

    /// Returns the `next` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), any non-null `next` pointer stored in `queue_node` must have
    ///   been written by [`SmmuPage::set_next`] and point to a live page (rather than an
    ///   `fbl::DoublyLinkedList` bit-0-tagged container sentinel), and the caller must have
    ///   exclusive access to the intrusive list containing `self`.
    #[inline]
    pub(crate) unsafe fn get_next(self) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                crate::vm::page::VmPagePtr::from_raw(*node.next.get()).map(|ptr| Self { ptr })
            }
        }
    }

    /// Sets the `next` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` (and `next` when `Some`) must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), and the caller must have exclusive access to the intrusive
    ///   list containing `self`.
    #[inline]
    pub(crate) unsafe fn set_next(self, next: Option<Self>) {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                *node.next.get() = next.map_or(core::ptr::null_mut(), |p| p.ptr.as_raw());
            }
        }
    }

    /// Returns the `prev` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), any non-null `prev` pointer stored in `queue_node` must have
    ///   been written by [`SmmuPage::set_prev`] and point to a live page, and the caller must have
    ///   exclusive access to the intrusive list containing `self`.
    #[inline]
    pub(crate) unsafe fn get_prev(self) -> Option<Self> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                crate::vm::page::VmPagePtr::from_raw(*node.prev.get()).map(|ptr| Self { ptr })
            }
        }
    }

    /// Sets the `prev` pointer stored in this page's intrusive `queue_node`.
    ///
    /// # Safety
    /// - `self` (and `prev` when `Some`) must point to a live, allocated `VmPage` / `HostVmPage`.
    /// - `self`'s `queue_node` must not be linked into an `fbl::DoublyLinkedList`
    ///   (`VmPageDoublyLinkedList`), and the caller must have exclusive access to the intrusive
    ///   list containing `self`.
    #[inline]
    pub(crate) unsafe fn set_prev(self, prev: Option<Self>) {
        #[cfg(is_kernel)]
        {
            // SAFETY: `self.ptr` is a valid live `VmPage` and caller owns the containing list.
            unsafe {
                let node = self.ptr.as_non_null().as_ref().get_node();
                *node.prev.get() = prev.map_or(core::ptr::null_mut(), |p| p.ptr.as_raw());
            }
        }
    }
}

/// A store into memory which the SMMU hardware reads, as reported to
/// [`SmmuEnv::observe_hw_visible_write`].
///
/// Today that memory is the stage 1 translation tables. SMMUv3 adds more in-memory structures
/// which the hardware reads (stream table entries, context descriptors, command queue entries);
/// they would be further variants here.
#[derive(Clone, Copy, Debug)]
pub enum HwVisibleWrite {
    /// One 64-bit translation table descriptor, in a table the writer believes is at `level`.
    Descriptor { level: u32, new_val: u64 },
    /// `len` bytes of a translation table being zero-filled, which clears every descriptor in
    /// the range.
    TableZeroFill { len: usize },
}

/// Zero-cost environment façade for SMMU page-table and page-cache operations.
///
/// [`SmmuEnv`] provides a unified static API over PMM allocation, physmap conversion, cache
/// maintenance, thread sleep, and PTE write observation that compiles to direct Zircon kernel
/// calls under `cfg(is_kernel)` and routes to the host test simulation mocks (`PmmMock`) under
/// `cfg(not(is_kernel))`.
pub struct SmmuEnv;

impl SmmuEnv {
    /// Allocates a single physical page from the PMM.
    #[inline]
    pub fn alloc_page(alloc_flags: u32) -> Result<SmmuPage, Status> {
        #[cfg(is_kernel)]
        {
            let (ptr, _pa) = crate::vm::pmm::alloc_page(alloc_flags)?;
            Ok(SmmuPage { ptr })
        }
    }

    /// Returns a single unlinked physical page to the PMM.
    ///
    /// # Safety
    /// `page` must have been allocated via [`SmmuEnv::alloc_page`], must not be linked into any
    /// intrusive list, and must no longer be referenced by any active translation table or IOTLB.
    #[inline]
    pub unsafe fn free_page(page: SmmuPage) {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `page` is unlinked and uniquely owned for reclamation.
            unsafe {
                crate::vm::pmm::free_page(page.ptr);
            }
        }
    }

    /// Frees a chain of physical pages back to the PMM, taking the PMM lock once for the whole
    /// chain rather than once per page.
    ///
    /// `first` is the first page of a chain linked through the pages' intrusive `queue_node`
    /// `next` pointers and terminated by a null `next` pointer.
    ///
    /// # Safety
    /// Every page in the chain must be a valid allocated PMM page which is uniquely owned by the
    /// caller for reclamation, the chain must not be reachable from any other list, and
    /// `first`'s `prev` pointer must be null.
    pub unsafe fn free_page_chain(first: SmmuPage) {
        #[cfg(is_kernel)]
        {
            // Move the chain onto a proper `VmPageDoublyLinkedList`, which is what the PMM's
            // batched free consumes.
            pin_init::stack_pin_init!(let list = crate::vm::page::VmPageDoublyLinkedList::new());
            let mut cur = Some(first);
            while let Some(page) = cur {
                // SAFETY: Caller guarantees the chain is valid and uniquely owned. Each page is
                // fully unlinked from the chain before it is added to `list`, which is never
                // moved; `push_back_raw` only mutates its sentinel links and the page's node.
                unsafe {
                    cur = page.get_next();
                    page.set_next(None);
                    page.set_prev(None);
                    list.as_mut().push_back_raw(page.ptr.as_non_null());
                }
            }
            // SAFETY: Caller guarantees every page now on `list` is a valid allocated PMM page
            // owned by us. `free_list` drains `list`, leaving it empty.
            unsafe {
                crate::vm::pmm::free_list(list);
            }
        }
    }

    /// Looks up the [`SmmuPage`] corresponding to physical address `pa`.
    #[inline]
    pub fn paddr_to_page(pa: u64) -> Option<SmmuPage> {
        #[cfg(is_kernel)]
        {
            crate::vm::pmm::paddr_to_vm_page(crate::kernel::types::PAddr(pa as usize))
                .map(|ptr| SmmuPage { ptr })
        }
    }

    /// Converts physical address `pa` to its virtual address in the kernel/mock physmap.
    #[inline]
    pub fn paddr_to_physmap(pa: u64) -> usize {
        #[cfg(is_kernel)]
        {
            crate::vm::physmap::paddr_to_physmap(crate::kernel::types::PAddr(pa as usize)).0
        }
    }

    /// Converts virtual address `vaddr` in the kernel/mock physmap back to its physical address.
    #[inline]
    pub fn physmap_to_paddr(vaddr: usize) -> u64 {
        #[cfg(is_kernel)]
        {
            u64::from(crate::vm::physmap::physmap_to_paddr(crate::kernel::types::VAddr(vaddr)))
        }
    }

    /// Cleans CPU data cache lines covering `[start, start + len)` to the Point of Coherency.
    ///
    /// On arm64 this ends with a `dsb sy` (`CleanDataCacheRange`), so it returns only once the
    /// cleaned stores are in memory. Callers rely on that where a later store, with no barrier
    /// of its own in between, makes the cleaned memory reachable by the SMMU; each such call
    /// site says so. An implementation without the barrier would need one added at those sites.
    ///
    /// # Safety
    /// `[start, start + len)` must be a mapped kernel virtual address range (in practice, the
    /// physmap address of a translation table page), or the mock physmap equivalent in host tests.
    #[inline]
    pub unsafe fn clean_dcache_range(start: usize, len: usize) {
        #[cfg(is_kernel)]
        {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                crate::arch_rs::clean_dcache_range(start, len);
            }
        }
    }

    /// Tells the host simulation hardware model about a store into memory which the SMMU
    /// hardware reads, before the store is made. Compiles to a zero-cost no-op in production
    /// kernel builds.
    ///
    /// This is the single observation point for such memory. Every store into a translation
    /// table goes through it (see `vmsav8_64::write_pte` and `vmsav8_64::zero_table_page`), so
    /// the model sees each one. `vaddr` is the physmap address the store is about to write.
    ///
    /// # Safety
    /// - For [`HwVisibleWrite::Descriptor`], `vaddr` must be a non-null, 8-byte-aligned physmap
    ///   address with exposed provenance valid for reading a `u64` within an allocated 4 KiB
    ///   translation table page.
    /// - For [`HwVisibleWrite::TableZeroFill`], `vaddr` must be a non-null, 8-byte-aligned physmap
    ///   address with exposed provenance valid for reading `len / 8` initialized `u64` words
    ///   within a single allocated 4 KiB translation table page (`(vaddr & PAGE_MASK) + len <=
    ///   PAGE_SIZE`).
    #[inline]
    pub(super) unsafe fn observe_hw_visible_write(vaddr: usize, write: HwVisibleWrite) {
        #[cfg(is_kernel)]
        {
            let _ = (vaddr, write);
        }
    }

    /// Converts a peripheral physical address `paddr` to its mapped virtual address (`0` if
    /// unmapped).
    #[inline]
    pub fn periph_paddr_to_vaddr(paddr: u64) -> usize {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::periph_paddr_to_vaddr(crate::kernel::types::PAddr::from(paddr))
                .map_or(0, |v| v.0)
        }
    }

    /// Converts a peripheral virtual address `vaddr` back to its physical address (`0` if
    /// unmapped).
    #[inline]
    pub fn periph_vaddr_to_paddr(vaddr: usize) -> u64 {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::periph_vaddr_to_paddr(crate::kernel::types::VAddr(vaddr))
                .map_or(0, u64::from)
        }
    }

    /// Executes a full-system Device Memory Barrier (`dsb sy` on arm64, no-op on host).
    #[inline]
    pub fn device_memory_barrier() {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::device_memory_barrier();
        }
    }

    /// Hints to the processor that the caller is in a busy-wait spin loop.
    ///
    /// Issues the architectural spin-wait yield instruction (`yield` on aarch64, `pause` on
    /// x86_64) via `libarch::r#yield()` rather than `core::hint::spin_loop()`, which on aarch64
    /// emits an `isb` (a pipeline flush on every poll) and omits the compiler memory clobber.
    #[inline]
    pub fn yield_cpu() {
        #[cfg(is_kernel)]
        {
            crate::arch_rs::r#yield();
        }
    }

    /// Sleeps the calling thread for `delay_ns` nanoseconds (relative).
    #[inline]
    pub fn sleep_relative(delay_ns: i64) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            crate::kernel::thread::sleep_relative(crate::platform_rs::timer::DurationMono(delay_ns))
        }
    }

    /// Sleeps the calling thread for `usec` microseconds.
    #[inline]
    pub fn sleep_usec(usec: u64) {
        let delay_ns = i64::try_from(usec).unwrap_or(i64::MAX).saturating_mul(1_000);
        let _ = Self::sleep_relative(delay_ns);
    }

    /// Queries `BootOptions::Get()->arm_smmu_mode` and parses the mode for `base_addr`, falling
    /// back to [`ArmSmmuMode::Disabled`] if the boot option string is invalid.
    pub fn boot_option_smmu_mode(base_addr: Option<u64>) -> ArmSmmuMode {
        #[cfg(is_kernel)]
        {
            get_smmu_mode_bytes(&boot_options::BootOptions::get().arm_smmu_mode, base_addr)
                .unwrap_or(ArmSmmuMode::Disabled)
        }
    }

    /// Adds `[base, base + size)` of resource `kind` to the root resource filter deny list.
    #[inline]
    pub fn root_resource_filter_add_deny_region(base: u64, size: usize, kind: u32) {
        #[cfg(is_kernel)]
        {
            crate::root_resource_filter::root_resource_filter_add_deny_region(
                base as usize,
                size,
                kind,
            );
        }
    }

    /// Configures interrupt `vector` with `trigger_mode` and `polarity`.
    #[inline]
    pub fn configure_interrupt(
        vector: u32,
        trigger_mode: InterruptTriggerMode,
        polarity: InterruptPolarity,
    ) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: `vector` is validated by the interrupt controller and global interrupt ops
            // are initialized before SMMU driver init.
            unsafe {
                crate::pdev_interrupt::configure_interrupt(
                    crate::dev_interrupt::InterruptVector(vector),
                    trigger_mode,
                    polarity,
                )
            }
        }
    }

    /// Masks interrupt `vector`.
    #[inline]
    pub fn mask_interrupt(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::mask_interrupt(crate::dev_interrupt::InterruptVector(vector))
            }
        }
    }

    /// Unmasks interrupt `vector`.
    #[inline]
    pub fn unmask_interrupt(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::unmask_interrupt(crate::dev_interrupt::InterruptVector(
                    vector,
                ))
            }
        }
    }

    /// Registers `handler(cookie)` for interrupt `vector`.
    ///
    /// # Safety
    /// `cookie` must remain valid for as long as the interrupt handler remains registered (until
    /// [`SmmuEnv::unregister_int_handler`] completes).
    #[inline]
    pub unsafe fn register_int_handler(
        vector: u32,
        cookie: *mut c_void,
        handler: extern "C" fn(*mut c_void),
    ) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: Caller guarantees `cookie` remains valid while registered.
            unsafe {
                crate::pdev_interrupt::register_int_handler(
                    crate::dev_interrupt::InterruptVector(vector),
                    crate::dev_interrupt::InterruptHandler { cookie, r#fn: Some(handler) },
                )
            }
        }
    }

    /// Unregisters the interrupt handler for `vector`.
    #[inline]
    pub fn unregister_int_handler(vector: u32) -> Result<(), Status> {
        #[cfg(is_kernel)]
        {
            // SAFETY: The interrupt controller driver registers the global interrupt ops during
            // early platform init, before any SMMU instance is created. `vector` needs no
            // validation here: the controller driver checks it and returns an error for a vector
            // it does not handle.
            unsafe {
                crate::pdev_interrupt::register_int_handler(
                    crate::dev_interrupt::InterruptVector(vector),
                    crate::dev_interrupt::InterruptHandler::DEFAULT,
                )
            }
        }
    }

    /// Emits a fully rendered `DRIVER OOPS` `report` with a single console write in kernel builds,
    /// and suppresses it in host test builds so CI `tefmocheck` does not flag expected negative
    /// tests.
    ///
    /// `report` must start with [`DRIVER_OOPS_BANNER`]. Callers render the entire report into a
    /// buffer first instead of using `dprintf!` directly: `dprintf!` issues a separate console
    /// write for every literal fragment and argument of its format string, which would allow a
    /// multi-line report to be interleaved with output from other CPUs.
    #[inline]
    pub fn print_driver_oops_report(report: &str) {
        debug_assert!(report.starts_with(DRIVER_OOPS_BANNER));
        #[cfg(is_kernel)]
        {
            use core::fmt::Write as _;
            let _ = debug::dprintf::KernelConsoleWriter.write_str(report);
        }
    }

    /// Logs a BTI quarantine warning for `bti_id` and `bti_name`.
    #[inline]
    pub fn print_quarantine_warning(
        bti_id: u64,
        bti_name: &[u8],
        reason: BtiPageLeakReason,
        total_leaked_pages: u64,
        total_leaked_vmos: usize,
    ) {
        #[cfg(is_kernel)]
        {
            crate::iommu::print_quarantine_warning(
                bti_id,
                bti_name,
                reason,
                total_leaked_pages,
                total_leaked_vmos,
            );
        }
    }
}

/// Owned `PinnedVmObject` wrapper used by [`super::super::v2::SmmuV2Bti`] and
/// [`super::super::v2::SmmuV2Pmt`].
///
/// In kernel builds (`cfg(is_kernel)`), this wraps [`crate::vm::pinned_vm_object::PinnedVmObject`]
/// directly without any C++ FFI round-trip. In standalone host test builds (`cfg(not(is_kernel))`),
/// it wraps the 24-byte host mock `PinnedVmObject` storage.
#[repr(C)]
pub struct SmmuPinnedVmo {
    #[cfg(is_kernel)]
    inner: crate::vm::pinned_vm_object::PinnedVmObject,
}

zr::static_assert!(core::mem::size_of::<SmmuPinnedVmo>() == 24);
zr::static_assert!(core::mem::align_of::<SmmuPinnedVmo>() == 8);

impl SmmuPinnedVmo {
    /// Creates an empty (unpinned) `SmmuPinnedVmo`.
    #[must_use]
    #[inline]
    pub const fn empty() -> Self {
        #[cfg(is_kernel)]
        {
            Self { inner: crate::vm::pinned_vm_object::PinnedVmObject::new() }
        }
    }

    /// Wraps a kernel [`crate::vm::pinned_vm_object::PinnedVmObject`] directly.
    #[cfg(is_kernel)]
    #[must_use]
    #[inline]
    pub const fn from_kernel_pinned_vmo(
        pinned_vmo: crate::vm::pinned_vm_object::PinnedVmObject,
    ) -> Self {
        Self { inner: pinned_vmo }
    }

    /// Returns `true` if this `SmmuPinnedVmo` holds a non-null `VmObject`.
    #[must_use]
    #[inline]
    pub fn has_vmo(&self) -> bool {
        #[cfg(is_kernel)]
        {
            self.inner.vmo().is_some()
        }
    }

    /// Resets this `SmmuPinnedVmo`, unpinning its pages and dropping its `VmObject` reference.
    #[inline]
    pub fn reset(&mut self) {
        #[cfg(is_kernel)]
        {
            self.inner = crate::vm::pinned_vm_object::PinnedVmObject::new();
        }
    }

    /// Takes ownership of the underlying `PinnedVmObject`, leaving `self` empty.
    #[must_use]
    #[inline]
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

impl Default for SmmuPinnedVmo {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl PinnedVmoSource for SmmuPinnedVmo {
    #[inline]
    fn offset(&self) -> u64 {
        #[cfg(is_kernel)]
        {
            self.inner.offset()
        }
    }

    #[inline]
    fn size(&self) -> u64 {
        #[cfg(is_kernel)]
        {
            self.inner.size()
        }
    }

    #[inline]
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        #[cfg(is_kernel)]
        {
            PinnedVmoSource::lookup_contiguous(&self.inner, offset, size)
        }
    }
}

/// Trait abstracting a pinned VMO range for [`super::device_aspace::DeviceAspace::map`].
pub trait PinnedVmoSource {
    /// Returns the starting byte offset within the underlying VMO.
    fn offset(&self) -> u64;

    /// Returns the size in bytes of the pinned VMO range.
    fn size(&self) -> u64;

    /// Looks up the contiguous physical address for `[offset, offset + size)` in the VMO.
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status>;
}

#[cfg(is_kernel)]
impl PinnedVmoSource for crate::vm::pinned_vm_object::PinnedVmObject {
    #[inline]
    fn offset(&self) -> u64 {
        self.offset()
    }

    #[inline]
    fn size(&self) -> u64 {
        self.size()
    }

    #[inline]
    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        let vmo = self.vmo().ok_or(Status::INVALID_ARGS)?;
        let pa = vmo.lookup_contiguous(offset, size)?;
        Ok(u64::from(pa))
    }
}
