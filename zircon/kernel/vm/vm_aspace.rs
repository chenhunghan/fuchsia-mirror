// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_ASPACE_FLAG_GUEST, ARCH_ASPACE_FLAG_KERNEL, ARCH_MMU_FLAG_CACHE_MASK,
    ARCH_UNMAP_OPTION_ENLARGE, ARCH_UNMAP_OPTION_NONE, ArchAspaceFlags, ArchMmuFlags,
    ArchUnmapOptions, ArchVmAspace, NonTerminalAction, TerminalAction,
};
use super::attribution::{AttributionCounts, FractionalBytes};
use super::fault::flag as pf_flag;
use super::name::{name_or_unnamed, name_to_c_buf};
use super::page_source::MultiPageRequest;
use super::vm::vm_ktrace_duration;
use super::vm_address_region::{
    OwnedVmAddressRegionOrMapping, VmAddressRegion, VmAspaceGuard, VmEnumerator, allocate_exposed,
    flag as vmar_flag, free_exposed,
};
use super::vm_mapping::VmMapping;
use super::vm_object::VmObject;
use super::vm_object_paged::VmObjectPaged;
use super::vm_object_physical::VmObjectPhysical;
use crate::arch_rs::{
    KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, MMU_GUEST_SIZE_SHIFT, USER_ASPACE_BASE,
    USER_ASPACE_SIZE,
};
use crate::counters::define_kcounter;
use crate::crypto::{Prng, global_prng};
use crate::kconcurrent::clt_tag;
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::kernel::thread::{ThreadLockGuard, ThreadPtr, thread_lock_guard};
use crate::kernel::types::{PAddr, VAddr};
use crate::ksync_kernel::{TrackedCriticalMutex, TrackedMutex};
use crate::ktrace_rs::ArgValue;
use crate::userabi::vdso::VDso;
use crate::vm::pmm;
use ::page as kernel_page;
use boot_options::BootOptions;
use core::cell::Cell;
use core::ffi::{CStr, c_char, c_void};
use core::hint;
use core::mem::{ManuallyDrop, MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicI64, Ordering};
use debug::ltracef;
use fbl::{
    Canary, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, HasRefCount,
    Recyclable, RefCounted, RefPtr, UninitRecyclable,
};
use kalloc::AllocError;
use kprint::kprintln;
use ksync::{KCell, KMutex, LockClass, LockToken};
use lazy_init::{LazyInit, NoCheck};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use vm_aspace_bindings as bindings;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

const LOCAL_TRACE: u32 = 0;

pub const GUEST_PHYSICAL_ASPACE_BASE: usize = 0;
pub const GUEST_PHYSICAL_ASPACE_SIZE: usize = 1 << MMU_GUEST_SIZE_SHIFT;

define_kcounter!(
    VM_ASPACE_ACCESSED_HARVESTS_PERFORMED,
    "vm.aspace.accessed_harvest.performed",
    Sum
);
define_kcounter!(VM_ASPACE_ACCESSED_HARVESTS_SKIPPED, "vm.aspace.accessed_harvest.skipped", Sum);
define_kcounter!(VM_ASPACE_HIGH_PRIORITY, "vm.aspace.high_priority", Sum);
define_kcounter!(VM_ASPACE_LAST_FAULT_HIT, "vm.aspace.last_fault.hit", Sum);
define_kcounter!(VM_ASPACE_LAST_FAULT_MISS, "vm.aspace.last_fault.miss", Sum);

/// For region creation routines
pub mod vmm_flag {
    /// allocate at specific address
    pub const VALLOC_SPECIFIC: u32 = 1 << 0;

    /// commit memory up front (no demand paging)
    pub const COMMIT: u32 = 1 << 1;
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Type {
    User = 0,
    Kernel = 1,
    /// You probably do not want to use `LowKernel`. It is primarily used for SMP bootstrap
    /// to allow mappings of very low memory using the standard VMM subsystem.
    LowKernel = 2,
    /// Used to construct an address space representing hypervisor guest memory.
    GuestPhysical = 3,
}

zr::static_assert!(size_of::<Type>() == size_of::<bindings::VmAspace_Type>());
zr::static_assert!(align_of::<Type>() == align_of::<bindings::VmAspace_Type>());
zr::static_assert!(Type::User as u32 == bindings::VmAspace_Type::User.0 as u32);
zr::static_assert!(Type::Kernel as u32 == bindings::VmAspace_Type::Kernel.0 as u32);
zr::static_assert!(Type::LowKernel as u32 == bindings::VmAspace_Type::LowKernel.0 as u32);
zr::static_assert!(Type::GuestPhysical as u32 == bindings::VmAspace_Type::GuestPhysical.0 as u32);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareOpt {
    /// A normal independent address space initialized using [`ArchVmAspace::init`].
    None = 0,
    /// A restricted address space whose underlying [`ArchVmAspace`] will be initialized using
    /// `init_restricted`.
    Restricted = 1,
    /// A shared address space whose underlying [`ArchVmAspace`] will be initialized using
    /// `init_shared`.
    Shared = 2,
}

zr::static_assert!(size_of::<ShareOpt>() == size_of::<bindings::VmAspace_ShareOpt>());
zr::static_assert!(align_of::<ShareOpt>() == align_of::<bindings::VmAspace_ShareOpt>());
zr::static_assert!(ShareOpt::None as u32 == bindings::VmAspace_ShareOpt::None.0 as u32);
zr::static_assert!(ShareOpt::Restricted as u32 == bindings::VmAspace_ShareOpt::Restricted.0 as u32);
zr::static_assert!(ShareOpt::Shared as u32 == bindings::VmAspace_ShareOpt::Shared.0 as u32);

#[derive(Debug, Copy, Clone)]
pub struct VmAspaceLockClass;

ksync::declare_interned_string!(VM_ASPACE_LOCK_STRING_REG, "VmAspace::lock");

// SAFETY: The linker script defines the `rust_lock_classes` section for
// `LockClassRegistration` entries.
#[unsafe(link_section = "rust_lock_classes")]
#[used]
static VM_ASPACE_LOCK_REGISTRATION: ksync::LockClassRegistration =
    ksync::LockClassRegistration::with_flags(
        VM_ASPACE_LOCK_STRING_REG,
        <VmAspaceLockClass as LockClass>::FLAGS,
    );

impl LockClass for VmAspaceLockClass {
    const ID: *mut c_void = VM_ASPACE_LOCK_REGISTRATION.get();
    const FLAGS: ksync::LockFlags = TrackedCriticalMutex::LOCK_FLAGS;
}

#[derive(Debug, Copy, Clone)]
pub struct VmAspaceRegionLockClass;

ksync::declare_interned_string!(VM_ASPACE_REGION_LOCK_STRING_REG, "VmAspace::region_lock");

// SAFETY: The linker script defines the `rust_lock_classes` section for
// `LockClassRegistration` entries.
#[unsafe(link_section = "rust_lock_classes")]
#[used]
static VM_ASPACE_REGION_LOCK_REGISTRATION: ksync::LockClassRegistration =
    ksync::LockClassRegistration::with_flags(
        VM_ASPACE_REGION_LOCK_STRING_REG,
        <VmAspaceRegionLockClass as LockClass>::FLAGS,
    );

impl LockClass for VmAspaceRegionLockClass {
    const ID: *mut c_void = VM_ASPACE_REGION_LOCK_REGISTRATION.get();
    const FLAGS: ksync::LockFlags = TrackedCriticalMutex::LOCK_FLAGS;
}

pub type VmAspaceMutex = KMutex<VmAspaceLockClass, TrackedCriticalMutex>;

pub type VmAspaceRegionMutex = KMutex<VmAspaceRegionLockClass, TrackedCriticalMutex>;

/// A collection of memory usage counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmUsage {
    /// A count of bytes covered by VmMapping ranges.
    pub mapped_bytes: usize,

    // For the fields below, a byte is considered committed if a VmMapping covers a range of a
    // VmObject that contains that byte's page, and the page has physical memory allocated to it.
    /// A count of committed bytes that are only mapped into this address space and are not shared
    /// between VMOs via copy-on-write.
    pub private_bytes: usize,

    /// A count of committed bytes that are mapped into this and at least one other address space,
    /// or are shared between VMOs via copy-on-write (even if the VMOs are both mapped into this
    /// address space).
    pub shared_bytes: usize,

    /// A number that estimates the fraction of shared_bytes that this address space is responsible
    /// for keeping alive.
    ///
    /// An estimate of:
    ///   For each shared, committed page:
    ///   share_factor = (number of VMOs sharing this page) *
    ///                  (number of address spaces mapping this page)
    ///   scaled_shared_bytes += kPageSize / share_factor
    ///
    /// This number is strictly smaller than shared_bytes.
    pub scaled_shared_bytes: FractionalBytes,
}

/// Represents the ASLR configuration for a [`VmAspace`]. This is grouped in a struct so it can be
/// conveniently grouped together as it is const over the lifetime of a [`VmAspace`].
#[repr(C)]
struct AslrConfig {
    enabled: bool,
    entropy_bits: u8,
    compact_entropy_bits: u8,
    /// We record the PRNG seed to enable reproducible debugging.
    seed: [u8; Prng::MIN_ENTROPY],
}

/// A virtual address space. Its regions and mappings descend from its root VMAR, and its arch
/// aspace holds the translations for them.
///
/// `repr(C)`, here and on the embedded types, because `VmAspace` is used in `extern "C"`
/// signatures, and C++ uses the reference count at offset 0.
#[ksync::guarded]
#[derive(DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmAspace {
    ref_count: RefCounted,
    canary: Canary<{ fbl::magic(b"VMAS") }>,
    #[guarded_by(lock)]
    vdso_code_mapping: Option<RefPtr<VmMapping>>,

    /// The root VMAR of the address space, from which every other VMAR and mapping descends. It is
    /// `None` until the aspace is initialized, it stays `None` for a unified aspace, and
    /// [`VmAspace::destroy`] sets it back to `None`. It may only be read or changed with `lock`
    /// held.
    ///
    /// It is always `None` by the time the aspace is dropped: the root VMAR holds a reference back
    /// to the aspace, so the aspace cannot lose its last reference while this is still set.
    #[guarded_by(lock)]
    root_vmar: Option<RefPtr<VmAddressRegion>>,

    /// Primary lock that is used by all objects (VMARs and mappings) in the aspace hierarchy, and
    /// serializes most modifications. Holding this lock is not sufficient to perform allocations or
    /// deallocations (i.e. access the `subregions` field in VMARs and transition objects to/from
    /// `Alive`). For this the `region_lock` must also be held.
    #[mutex(VmAspaceLockClass)]
    lock: KMutex<TrackedCriticalMutex>,

    /// Keep a cache of the `VmMapping` of the last page fault that occurred. On a page fault this
    /// can be checked to see if it matches more quickly than walking the full vmar tree. A raw
    /// pointer is stored here since the `VmMapping` must be alive and in tree anyway and if it
    /// were a `RefPtr` we would not be able to handle being the one to drop the last ref and
    /// perform destruction.
    ///
    /// Safety invariant: Mappings that are stored here must be in the `Alive` state, implying that
    /// they are in the VMAR tree. It is then the responsibility of the `VmMapping` to remove itself
    /// from here should it transition out of `Alive`, and remove itself from the VMAR tree.
    #[guarded_by(lock)]
    last_fault: Option<NonNull<VmMapping>>,

    /// # Safety
    ///
    /// Always contains a NUL byte and is never mutated after initialization.
    name: [u8; ZX_MAX_NAME_LEN],

    type_: Type,

    /// Set by [`VmAspace::destroy`] once it has torn the address space down.
    ///
    /// # Safety
    ///
    /// This is `true` by the time the last reference to the aspace is dropped. While an aspace has
    /// a root VMAR, that VMAR holds a reference back to the aspace, so the last reference can only
    /// go away after `destroy` has set this and released the root VMAR. An aspace that never got a
    /// root VMAR is a unified one, which its creator must destroy before dropping it (see
    /// [`VmAspace::create_unified`]), or one whose initialization failed, which `create_with_opts`
    /// and `create_unified` destroy before dropping. The kernel aspace never loses its last
    /// reference.
    #[guarded_by(lock)]
    aspace_destroyed: bool,

    /// The number of page table reclamations attempted since last active. This is used since we
    /// need to perform pt reclamation twice in a row (once to clear accessed bits, another time to
    /// reclaim page tables) before the aspace is at a fixed point and we can actually stop
    /// performing the harvests. Is only ever updated to a maximum value of 2.
    pt_harvest_since_active: KCell<Cell<u8>, AspaceListLockClass>,

    aslr_config: AslrConfig,

    base: usize,

    size: usize,

    /// The high priority count is used to determine whether this aspace should perform page table
    /// reclamation, with any non-zero count completely disabling reclamation. This is an atomic so
    /// that it can be safely read outside the lock, however writes should occur inside the lock.
    high_priority_count: AtomicI64,

    #[pin]
    #[dll_node(tag = AspaceListTag)]
    list_node: DoublyLinkedListNode<VmAspace>,

    /// Conceptually the `region_lock` represents the authority to manipulate the portions of the
    /// address space that are allocated. This is implemented by requiring both `lock` and
    /// `region_lock` to be held in order to modify the `subregions` list, or transition a
    /// vmar/mapping to/from the `Alive` state. As a consequence, holding either `lock` OR
    /// `region_lock` is sufficient to know that any presently `Alive` objects cannot change state,
    /// and that the `subregions` can be safely iterated.
    /// Where both must be held, the `region_lock` is to be acquired before `lock`.
    #[mutex(VmAspaceRegionLockClass)]
    region_lock: KMutex<TrackedCriticalMutex>,

    /// PRNG used by VMARs for address choices. The PRNG is thread safe and does not need to be
    /// guarded by the lock.
    #[pin]
    aslr_prng: Prng,

    /// Architecturally specific part of the aspace. This is internally locked and does not need to
    /// be guarded by `lock`.
    ///
    /// # Safety
    ///
    /// By the time the last reference to the aspace is dropped, the aspace is not active on any CPU
    /// and no unified aspace built from it is still live, as `new` requires of its caller. An
    /// aspace is only active on a CPU while it is assigned to a thread running there.
    /// [`VmAspace::attach_to_thread`] requires a reference to be held elsewhere for as long as the
    /// aspace is assigned, and `vmm::set_active_aspace` requires the previous aspace to be restored
    /// before the aspace is destroyed, which happens before its last reference is dropped (see
    /// `aspace_destroyed`). [`VmAspace::create_unified`] requires the aspaces a unified aspace is
    /// built from to outlive it, and the kernel aspace never loses its last reference.
    #[pin]
    arch_aspace: ArchVmAspace,
}

zr::static_assert!(size_of::<VmAspaceMutex>() == bindings::kVmAspaceLockSize);
zr::static_assert!(align_of::<VmAspaceMutex>() == bindings::kVmAspaceLockAlign);
zr::static_assert!(offset_of!(VmAspace, ref_count) == 0);
zr::static_assert!(size_of::<RefCounted>() == bindings::kVmAspaceRefCountedSize);
zr::static_assert!(align_of::<RefCounted>() == bindings::kVmAspaceRefCountedAlign);

impl HasRefCount for VmAspace {
    #[inline]
    fn ref_count(&self) -> &RefCounted {
        &self.ref_count
    }
}

// SAFETY: `allocate` is not overridden. `allocate_uninit` allocates with `allocate_exposed`,
// `recycle_uninit` frees with `kalloc::Box` through the pointer `allocate_uninit` returned, and
// `recycle` frees with `free_exposed`, so through any pointer to the aspace, whatever its
// provenance. Every aspace but the kernel aspace is constructed in place by `RefPtr::pin_init` in
// storage that `allocate_uninit` returned. A reference is only added while another is held, so the
// count reaches zero, and `recycle` runs, once per allocation. The aspace list also points to the
// aspace without holding a reference. `recycle` erases it from the list under the list lock before
// freeing it, and walkers use the aspaces they reach only while holding that lock, so none uses it
// once it is freed. The kernel aspace lives in the static `KERNEL_ASPACE`, and the reference that
// `kernel_aspace_init_pre_heap` adopted for it is never released, so it is never recycled.
unsafe impl Recyclable for VmAspace {
    #[cold]
    #[inline(never)]
    unsafe fn recycle(ptr: NonNull<Self>) {
        {
            // SAFETY: By the `recycle` contract, `ptr` points to a valid, fully initialized
            // `VmAspace`. Other CPUs walking the aspace list may hold shared references to it until
            // it is erased from the list, which this shared reference does not conflict with.
            let aspace = unsafe { ptr.as_ref() };
            aspace.canary.assert();
            ltracef!("{:p} '{}'\n", aspace, zr::from_utf8_lossy(aspace.name_ptr().to_bytes()));

            // pop it out of the global aspace list
            aspace.aspaces_list_erase();
        }

        // SAFETY: `ptr` points to an initialized `VmAspace`, which is in storage that
        // `allocate_exposed` returned, has no references left and is not used afterwards. It is no
        // longer in the aspace list, so no other CPU can reach it.
        unsafe { free_exposed(ptr) }
    }
}

// SAFETY: As for `Recyclable` above.
unsafe impl UninitRecyclable for VmAspace {
    fn allocate_uninit() -> Result<NonNull<MaybeUninit<Self>>, AllocError> {
        allocate_exposed()
    }

    unsafe fn recycle_uninit(ptr: NonNull<MaybeUninit<Self>>) {
        // SAFETY: `ptr` was allocated as a `kalloc::Box`, has no references left and is not used
        // afterwards.
        drop(unsafe { kalloc::Box::from_non_null(ptr) });
    }
}

#[pinned_drop]
impl PinnedDrop for VmAspace {
    #[inline]
    fn drop(mut self: Pin<&mut Self>) {
        debug_assert!(!self.get_node().in_container());

        // we have to have already been destroyed before freeing
        let this = self.as_mut().project();
        debug_assert!(*this.aspace_destroyed.get_inner_mut());
        debug_assert!(this.vdso_code_mapping.get_inner_mut().is_none());
        debug_assert!(this.root_vmar.get_inner_mut().is_none());

        // destroy the arch portion of the aspace
        // TODO(teisenbe): Move this to `destroy()`. Currently can't move since
        // `ProcessDispatcher` calls `destroy()` from the context of a thread in the
        // aspace and `harvest_all_user_accessed_bits` assumes the `arch_aspace` is valid if
        // the aspace is in the global list.
        // SAFETY: `self` is exclusively borrowed, and by the `AspaceList` invariant it is not in
        // the aspace list while it is dropped, so no other method of its arch aspace runs
        // concurrently with `destroy`, and none runs after it, as `self` is not used afterwards.
        // `self` was constructed by `new`, whose `# Safety` contract requires that when it is
        // dropped its arch aspace has no mappings left unless it is unified. By the invariant on
        // the `arch_aspace` field, it is not active on any CPU and no unified address space built
        // from it is live.
        let status = unsafe { self.arch_aspace().destroy() };
        debug_assert!(status.is_ok());

        debug_assert!(!self.is_high_memory_priority());
    }
}

// SAFETY: `VmAspace` is not tied to the thread that created it.
unsafe impl Send for VmAspace {}
// SAFETY: Mutable state of `VmAspace` is only accessed with the aspace lock, the region lock or
// the aspace list lock held, atomically, or through internally synchronized members.
unsafe impl Sync for VmAspace {}

/// Tag for the list of all aspaces.
struct AspaceListTag;

#[ksync::guarded]
struct AspaceList {
    #[mutex]
    lock: KMutex<TrackedMutex>,
    /// # Safety
    ///
    /// Every element points to a pinned `VmAspace` with an initialized arch aspace, which is erased
    /// from the list before its arch aspace is destroyed or it is freed: it is owned by `RefPtr`s,
    /// whose `recycle` erases it before dropping it, or is never dropped.
    #[pin]
    #[guarded_by(lock)]
    list: DoublyLinkedList<NonNull<VmAspace>, AspaceListTag>,
}

// SAFETY: The list holds raw pointers to aspaces, and every access to it is serialized by `lock`.
unsafe impl Sync for AspaceList {}

/// Singleton list of all aspaces in the system.
///
/// # Safety
///
/// Initialized at `LK_INIT_LEVEL_EARLIEST`, before any aspace exists or the list is otherwise
/// used.
static ASPACES_LIST: LazyInit<AspaceList, NoCheck> = LazyInit::uninit();

impl AspaceList {
    #[inline]
    fn get() -> &'static Self {
        // SAFETY: `ASPACES_LIST` is initialized before the list is used.
        unsafe { ASPACES_LIST.get_unchecked() }
    }
}

fn initialize_aspaces_list(_level: init::LkInitLevel) {
    let init = pin_init!(AspaceList {
        lock <- KMutex::init(),
        list <- KCell::pin_init(DoublyLinkedList::new()),
    });
    // SAFETY: Single-threaded initialization during early boot, before any aspace exists or the
    // list is otherwise used.
    let Ok(()) = unsafe { Pin::static_ref(&ASPACES_LIST).init_pin(init) };
}

init::lk_init_hook!(vm_aspace_list_init, initialize_aspaces_list, init::LK_INIT_LEVEL_EARLIEST);

/// The singleton kernel address space.
///
/// # Safety
///
/// Initialized by `kernel_aspace_init_pre_heap` in `vm_init_preheap` before
/// `VmAspace::kernel_aspace` is called.
static KERNEL_ASPACE: LazyInit<VmAspace, NoCheck> = LazyInit::uninit();

pub(super) static KERNEL_ROOT_VMAR: LazyInit<VmAddressRegion, NoCheck> = LazyInit::uninit();

/// Returns true if the base + size is valid for the given `type_`.
fn is_valid_for_type(base: usize, size: usize, type_: Type) -> bool {
    if size == 0 || !kernel_page::is_aligned(base) || !kernel_page::is_aligned(size) {
        return false;
    }
    if base.wrapping_add(size) < base {
        return false;
    }

    let (min, max) = match type_ {
        Type::User => (USER_ASPACE_BASE, USER_ASPACE_BASE.wrapping_add(USER_ASPACE_SIZE)),
        Type::Kernel => return false,
        Type::LowKernel => (0, USER_ASPACE_BASE.wrapping_add(USER_ASPACE_SIZE)),
        Type::GuestPhysical => (
            GUEST_PHYSICAL_ASPACE_BASE,
            GUEST_PHYSICAL_ASPACE_BASE.wrapping_add(GUEST_PHYSICAL_ASPACE_SIZE),
        ),
    };
    base >= min && base.wrapping_add(size) <= max
}

fn arch_aspace_flags_from_type(type_: Type) -> ArchAspaceFlags {
    let is_high_kernel = type_ == Type::Kernel;
    let is_guest = type_ == Type::GuestPhysical;
    (if is_high_kernel { ARCH_ASPACE_FLAG_KERNEL } else { 0 })
        | (if is_guest { ARCH_ASPACE_FLAG_GUEST } else { 0 })
}

impl VmAspace {
    /// Creates an address space of the type specified in `type_` with name `name`.
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy).
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    pub fn create(type_: Type, name: &[u8]) -> Option<RefPtr<VmAspace>> {
        let (base, size) = match type_ {
            Type::User => (USER_ASPACE_BASE, USER_ASPACE_SIZE),
            Type::Kernel => return None,
            Type::LowKernel => (0, USER_ASPACE_BASE + USER_ASPACE_SIZE),
            Type::GuestPhysical => (GUEST_PHYSICAL_ASPACE_BASE, GUEST_PHYSICAL_ASPACE_SIZE),
        };

        Self::create_with_opts(base, size, type_, name, ShareOpt::None)
    }

    /// Allocates and constructs an address space, adopted by the returned `RefPtr`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that, by the time the last reference to the returned address space
    /// is released, [`Self::destroy`] has been called on it, it is not active on any CPU, its arch
    /// aspace has no mappings left unless it is unified, and no unified address space built from
    /// it is still live.
    ///
    /// `base`, `size` and `type_` define a valid address space configuration, and the caller
    /// upholds the `ArchVmAspace` lifecycle of the returned address space: its arch aspace is
    /// initialized by one `init*` call before any other use.
    #[inline]
    unsafe fn new_ref(base: usize, size: usize, type_: Type, name: &[u8]) -> Option<RefPtr<Self>> {
        // SAFETY: `base`, `size` and `type_` define a valid address space configuration, and the
        // caller upholds the lifecycle of the arch aspace. The address space is only dropped by
        // `recycle`, once its last reference is released.
        let init = unsafe { Self::new(base, size, type_, name) };
        let Ok(aspace) = RefPtr::pin_init(init) else {
            hint::cold_path();
            return None;
        };
        aspace.trace_new();
        Some(aspace)
    }

    /// Constructs an address space in place.
    ///
    /// # Safety
    ///
    /// `base`, `size` and `type_` define a valid address space configuration, and the caller
    /// upholds the `ArchVmAspace` lifecycle of the constructed address space: its arch aspace is
    /// initialized by one `init*` call before any other use, and when the address space is
    /// dropped, it is not active on any CPU, its arch aspace has no mappings left unless it is
    /// unified, and no unified address space built from it is live.
    #[inline]
    unsafe fn new(base: usize, size: usize, type_: Type, name: &[u8]) -> impl PinInit<Self> {
        let arch_flags = arch_aspace_flags_from_type(type_);
        pin_init!(Self {
            ref_count: RefCounted::new(),
            canary: Canary::new(),
            vdso_code_mapping: KCell::new(None),
            root_vmar: KCell::new(None),
            lock <- KMutex::init(),
            last_fault: KCell::new(None),
            name: name_to_c_buf(name),
            type_,
            aspace_destroyed: KCell::new(false),
            pt_harvest_since_active: KCell::new(Cell::new(0)),
            aslr_config: Self::create_aslr_config(*type_),
            base,
            size,
            high_priority_count: AtomicI64::new(0),
            list_node: DoublyLinkedListNode::new(),
            region_lock <- KMutex::init(),
            aslr_prng <- Prng::init(&[]),
            // SAFETY: `base`, `size` and the flags of `type_` define a valid address space
            // configuration. The caller initializes the arch aspace with `init*` before any other
            // use and drops the address space only once its mappings are gone, and `PinnedDrop`
            // calls `destroy` before the arch aspace is dropped.
            arch_aspace <- unsafe { ArchVmAspace::new(*base, *size, arch_flags) },
        })
    }

    #[inline]
    fn trace_new(&self) {
        ltracef!("{:p} '{}'\n", self, zr::from_utf8_lossy(self.name_ptr().to_bytes()));
    }

    /// Returns a pointer to the address space `this` refers to.
    #[inline]
    pub fn as_non_null(this: &RefPtr<Self>) -> NonNull<Self> {
        // SAFETY: The pointer a `RefPtr` holds is non-null.
        unsafe { NonNull::new_unchecked(RefPtr::as_ptr(this).cast_mut()) }
    }

    /// # Safety
    ///
    /// `this` points to a `VmAspace` to which at least one reference is held for the duration of
    /// the call.
    #[inline]
    pub unsafe fn clone_from_ptr(this: *const Self) -> RefPtr<Self> {
        // SAFETY: `this` points to a `VmAspace` to which a reference is held.
        RefPtr::add_ref(unsafe { this.as_ref_unchecked() });
        // SAFETY: `this` points to a `VmAspace`, and the reference just added is owned by the
        // result.
        unsafe { RefPtr::from_raw(this) }
    }

    #[inline]
    pub fn lock(&self) -> &VmAspaceMutex {
        &self.lock
    }

    #[inline]
    pub fn region_lock(&self) -> &VmAspaceRegionMutex {
        &self.region_lock
    }

    /// Creates an address space of the type specified in `type_` with name `name`.
    ///
    /// The returned aspace will start at `base` and span `size`.
    ///
    /// If `share_opt` is [`ShareOpt::Shared`], we're creating a shared address space, and the
    /// underlying [`ArchVmAspace`] will be initialized using the
    /// [`init_shared`](ArchVmAspace::init_shared) method instead of the normal
    /// [`init`](ArchVmAspace::init) method.
    ///
    /// If `share_opt` is [`ShareOpt::Restricted`], we're creating a restricted address space, and
    /// the underlying [`ArchVmAspace`] will be initialized using the
    /// [`init_restricted`](ArchVmAspace::init_restricted) method.
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy).
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    ///
    /// # Panics
    ///
    /// Panics if `share_opt` is not [`ShareOpt::None`] and `type_` is not [`Type::User`].
    pub fn create_with_opts(
        base: usize,
        size: usize,
        type_: Type,
        name: &[u8],
        share_opt: ShareOpt,
    ) -> Option<RefPtr<VmAspace>> {
        ltracef!("type {}, name '{}'\n", type_ as u32, zr::from_utf8_lossy(name));

        if !is_valid_for_type(base, size, type_) {
            hint::cold_path();
            return None;
        }
        // `init` requires a shared or restricted aspace to be a user aspace.
        assert!(share_opt == ShareOpt::None || type_ == Type::User);

        // SAFETY: `is_valid_for_type` accepted `base` and `size` for `type_`. `init` below
        // initializes the arch aspace before any other use. If `init` fails, `destroy` is called
        // below before `aspace` is dropped. Otherwise `init` gives it a root VMAR, which holds a
        // reference to it until `destroy` releases it, so it is destroyed before its last reference
        // is released. When its last reference is released, no VMAR or mapping of it is left, as
        // each holds a reference to it, so every mapping was unmapped when it was destroyed. A
        // thread is only assigned the aspace while a reference to it is held elsewhere
        // (`attach_to_thread`), so it is not active on any CPU, and the `create_unified` contract
        // keeps it alive while a unified aspace built from it is live.
        let aspace = unsafe { Self::new_ref(base, size, type_, name) }?;

        // initialize the arch specific component to our address space
        // SAFETY: `aspace` was just heap-allocated, so early architecture MMU initialization has
        // completed, and it has not been initialized and is not used by other threads.
        // `is_valid_for_type` accepted its non-empty, page-aligned range for `type_` and rejects
        // every kernel range, so it is not a kernel aspace, and the `assert!` above verified that
        // any shared or restricted aspace is a user aspace.
        if unsafe { Self::init(&aspace, share_opt) }.is_err() {
            hint::cold_path();
            let status = aspace.destroy();
            debug_assert!(status.is_ok());
            return None;
        }

        // add it to the global list
        // SAFETY: `aspace` is pinned in its `RefPtr`, and was just constructed, so it is not in the
        // aspace list. `init` initialized its arch aspace, and it is owned by `RefPtr`s, whose
        // `recycle` erases it from the list before dropping it, which destroys the arch aspace.
        unsafe { aspace.aspaces_list_push_back() };

        // return a ref pointer to the aspace
        Some(aspace)
    }

    /// Creates a unified address space that consists of the given constituent address spaces.
    ///
    /// The passed in address spaces must meet the following criteria:
    /// 1. They must manage non-overlapping regions.
    /// 2. The shared [`VmAspace`] must have been created with [`ShareOpt::Shared`].
    ///
    /// Although reference counted, the returned [`VmAspace`] must be explicitly destroyed via
    /// [`destroy`](Self::destroy). Note that it must be destroyed before the shared and
    /// restricted [`VmAspace`]s; destroying the constituent [`VmAspace`]s before destroying
    /// this one will trigger asserts.
    ///
    /// Returns `None` on failure (e.g. due to resource starvation).
    ///
    /// # Safety
    ///
    /// `shared` and `restricted` must have been created with [`ShareOpt::Shared`] and
    /// [`ShareOpt::Restricted`], with `restricted`'s range below `shared`'s and sharing no
    /// top-level page table entry. `restricted` must have no mappings and not be part of another
    /// unified address space.
    ///
    /// The returned address space must be destroyed before its last reference is dropped, and
    /// `shared` and `restricted` must outlive it: the last reference to each of them must be
    /// dropped after the last reference to the returned address space. Nothing enforces this
    /// order, as the returned address space holds no reference to `shared` or `restricted`. The
    /// caller keeps it by holding a strong reference ([`RefPtr`]) to each of `shared` and
    /// `restricted`, and releasing those references only once every reference to the returned
    /// address space has been dropped.
    pub unsafe fn create_unified(
        shared: &VmAspace,
        restricted: &VmAspace,
        name: &[u8],
    ) -> Option<RefPtr<VmAspace>> {
        let type_ = Type::User;
        // Unified aspaces are initialized with a base and size of 0 to signify that they do not
        // manage any mappings themselves. It also provides an extra layer of security in that any
        // operation on a unified aspace will fail to do a range check.
        // SAFETY: A user aspace with a base and size of 0 is a valid configuration.
        // `init_unified` below initializes the arch aspace before any other use. If `init_unified`
        // fails, `destroy` is called below before `aspace` is dropped, and otherwise this
        // function's `# Safety` contract requires the caller to destroy it before its last
        // reference is dropped. A unified aspace may keep mappings when it is dropped. A thread is
        // only assigned the aspace while a reference to it is held elsewhere (`attach_to_thread`),
        // so it is not active on any CPU once its last reference is released. No unified aspace is
        // built from it, as `create_unified` requires aspaces created with `ShareOpt::Shared` and
        // `ShareOpt::Restricted`.
        let aspace = unsafe { Self::new_ref(0, 0, type_, name) }?;

        // Initialize the arch specific component to our address space.
        // SAFETY: `aspace` was just constructed as a user aspace, is distinct from `shared` and
        // `restricted`, has not been initialized and is not used by other threads. By the
        // `# Safety` contract, the arch aspaces of `shared` and `restricted` were initialized by
        // `init_shared` and `init_restricted` with `restricted` below `shared` and no shared
        // top-level page table entry,
        // `restricted` is empty and not part of another unified aspace, and both aspaces outlive
        // `aspace`. An arch aspace is only destroyed when its `VmAspace` is dropped, so both arch
        // aspaces stay live and undestroyed until `aspace`'s is destroyed.
        let status = unsafe {
            aspace.arch_aspace().init_unified(shared.arch_aspace(), restricted.arch_aspace())
        };
        if status.is_err() {
            hint::cold_path();
            let status = aspace.destroy();
            debug_assert!(status.is_ok());
            return None;
        }

        // Add it to the global list.
        // SAFETY: `aspace` is pinned in its `RefPtr`, and was just constructed, so it is not in the
        // aspace list. `init_unified` initialized its arch aspace, and it is owned by `RefPtr`s,
        // whose `recycle` erases it from the list before dropping it, which destroys the arch
        // aspace.
        unsafe { aspace.aspaces_list_push_back() };

        Some(aspace)
    }

    /// Called once at boot to initialize the singleton kernel address space.
    ///
    /// # Safety
    ///
    /// Must be called at most once on the boot CPU before `heap_init` or any other thread runs,
    /// after `thread::init_early()`, `ASPACES_LIST` and PMM initialization, and `arch_prevm_init`,
    /// and before any other use of the kernel address space.
    pub(super) unsafe fn kernel_aspace_init_pre_heap() {
        // SAFETY: The kernel aspace range is a valid configuration for a kernel aspace. `init`
        // below initializes the arch aspace before any other use of it, and the kernel aspace is
        // in static storage and never dropped.
        let init =
            unsafe { Self::new(KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE, Type::Kernel, b"kernel") };
        // SAFETY: `KERNEL_ASPACE` is initialized at most once, during single-threaded early boot
        // before any other use.
        let Ok(()) = unsafe { Pin::static_ref(&KERNEL_ASPACE).init_pin(init) };

        // SAFETY: `KERNEL_ASPACE` is initialized, has not been adopted and nothing else accesses
        // it. The reference is held in `ManuallyDrop` and never released, so the kernel aspace is
        // never recycled.
        let kernel_aspace = ManuallyDrop::new(unsafe { RefPtr::adopt_raw(KERNEL_ASPACE.as_ptr()) });
        kernel_aspace.trace_new();

        // SAFETY: `kernel_aspace` is the kernel aspace, and this is its only root region. The
        // region is initialized in the static `KERNEL_ROOT_VMAR`, whose last reference is never
        // released: the reference adopted below is held in `ManuallyDrop` and never released.
        let init = unsafe { VmAddressRegion::new_kernel_root(&kernel_aspace) };
        // SAFETY: `KERNEL_ROOT_VMAR` is initialized at most once, before any other use.
        let Ok(()) = unsafe { Pin::static_ref(&KERNEL_ROOT_VMAR).init_pin(init) };
        // SAFETY: `KERNEL_ROOT_VMAR` is initialized and has not been adopted. The reference is held
        // in `ManuallyDrop` and never released, so the region is never recycled.
        let root_vmar = ManuallyDrop::new(unsafe { RefPtr::adopt_raw(KERNEL_ROOT_VMAR.as_ptr()) });
        root_vmar.trace_new();
        {
            ksync::lock!(let mut guard = kernel_aspace.lock_lock());
            *guard.as_mut().root_vmar_mut() = Some(RefPtr::clone(&root_vmar));
        }

        // SAFETY: `kernel_aspace` was just constructed and has not been initialized, nothing else
        // accesses it, it is the only kernel aspace, and early architecture MMU initialization
        // has completed.
        let status = unsafe { Self::init(&kernel_aspace, ShareOpt::None) };
        assert!(status.is_ok());

        // SAFETY: `kernel_aspace` is in static storage, so pinned and never dropped, and its arch
        // aspace is never destroyed. It was just constructed, so it is not in the aspace list, and
        // `init` initialized its arch aspace.
        unsafe { kernel_aspace.aspaces_list_push_front() };
    }

    /// Completes initialization; may fail in OOM cases.
    ///
    /// # Safety
    ///
    /// `this` was just constructed, has not been initialized and is not accessed by other threads
    /// during the call. Early architecture MMU initialization has completed, and if `this` is a
    /// kernel aspace, no other kernel aspace is initialized. If `share_opt` is not
    /// `ShareOpt::None`, `this` is a user aspace with a non-empty, page-aligned user range.
    unsafe fn init(this: &RefPtr<Self>, share_opt: ShareOpt) -> Result<(), Status> {
        this.canary.assert();

        ltracef!("{:p} '{}'\n", &**this, zr::from_utf8_lossy(this.name_ptr().to_bytes()));

        // initialize the architecturally specific part
        // SAFETY: The arch aspace was just constructed, has not been initialized and is not
        // accessed by other threads. Early architecture MMU initialization has completed, and if
        // this is a kernel aspace, no other kernel aspace is initialized. A shared or restricted
        // aspace is a user aspace with a non-empty, page-aligned user range.
        let status = unsafe {
            match share_opt {
                ShareOpt::Shared => this.arch_aspace().init_shared(),
                ShareOpt::Restricted => this.arch_aspace().init_restricted(),
                ShareOpt::None => this.arch_aspace().init(),
            }
        };
        status?;

        this.initialize_aslr();

        ksync::lock!(let mut region_guard = this.region_lock().lock());
        ksync::lock!(let mut guard = this.lock_lock());
        let region_token = region_guard.token_mut();

        if guard.root_vmar().is_none() {
            let root_vmar = VmAddressRegion::create_root_locked(
                this,
                region_token,
                guard.as_mut().token_mut(),
                vmar_flag::CAN_MAP_SPECIFIC,
            )?;
            *guard.as_mut().root_vmar_mut() = Some(root_vmar);
            return Ok(());
        }
        hint::cold_path();
        Ok(())
    }

    fn initialize_aslr(&self) {
        self.aslr_prng.add_entropy(&self.aslr_config.seed);
    }

    fn create_aslr_config(type_: Type) -> AslrConfig {
        // As documented in //docs/gen/boot-options.md.
        const MAX_ASLR_ENTROPY: u8 = 36;

        let mut config = AslrConfig {
            enabled: false,
            entropy_bits: 0,
            compact_entropy_bits: 0,
            seed: [0; Prng::MIN_ENTROPY],
        };

        config.enabled = type_ == Type::User && !BootOptions::get().aslr_disabled;
        if config.enabled {
            config.entropy_bits = BootOptions::get().aslr_entropy_bits.min(MAX_ASLR_ENTROPY);
            config.compact_entropy_bits = 0;
        }

        global_prng::draw(&mut config.seed);

        config
    }

    /// Destroys this address space.
    ///
    /// `destroy()` does not free this object, but rather allows it to be freed when the last
    /// retaining `RefPtr` is destroyed.
    ///
    /// # Errors
    ///
    /// Returns the error from destroying the root VMAR, unless it is [`Status::BAD_STATE`].
    ///
    /// # Panics
    ///
    /// Panics if called on the kernel address space.
    pub fn destroy(&self) -> Result<(), Status> {
        self.canary.assert();
        ltracef!("{:p} '{}'\n", self, zr::from_utf8_lossy(self.name_ptr().to_bytes()));
        // The root VMAR of `KERNEL_ASPACE` is the static `KERNEL_ROOT_VMAR`, which must never be
        // destroyed.
        assert!(!ptr::eq(self, KERNEL_ASPACE.as_ptr()));

        ksync::lock!(let mut region_guard = self.region_lock().lock());
        let region_token = region_guard.token_mut();
        ksync::lock!(let mut guard = self.lock_lock());
        let mut guard = guard.as_mut().as_token_guard_mut();

        // Don't let a vDSO mapping prevent destroying a VMAR
        // when the whole process is being destroyed.
        drop(guard.vdso_code_mapping_mut().take());

        // tear down and free all of the regions in our address space
        if let Some(root_vmar) = guard.root_vmar().clone() {
            let status =
                VmAddressRegion::destroy_locked(root_vmar, region_token, guard.token_mut());
            if let Err(status) = status
                && status != Status::BAD_STATE
            {
                hint::cold_path();
                return Err(status);
            }
        }
        *guard.aspace_destroyed_mut() = true;

        drop(guard.root_vmar_mut().take());

        // Now that we've removed all mappings we can put the arch aspace into a sort of read-only
        // mode.
        //
        // TODO(https://fxbug.dev/42159319): Once https://fxbug.dev/42159319 is resolved, this call
        // (and the DisableUpdates feature) can be removed.
        self.arch_aspace().disable_updates();

        Ok(())
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    pub fn name_ptr(&self) -> &CStr {
        // SAFETY: `name` holds a NUL within its bounds.
        unsafe { CStr::from_ptr(self.name.as_ptr().cast()) }
    }

    /// Returns a reference to the architecturally specific part of the address space
    /// (`ArchVmAspace`). This is internally locked and does not need to be guarded by the lock.
    pub fn arch_aspace(&self) -> &ArchVmAspace {
        &self.arch_aspace
    }

    pub fn is_user(&self) -> bool {
        self.type_ == Type::User
    }

    /// Returns whether this aspace is a guest physical address space.
    ///
    /// TODO(https://fxbug.dev/42054461): Rationalize usage of `is_user` and `is_guest_physical`.
    #[inline]
    pub fn is_guest_physical(&self) -> bool {
        self.type_ == Type::GuestPhysical
    }

    #[inline]
    pub(super) fn can_enlarge_arch_unmap(&self) -> bool {
        self.is_user() || self.is_guest_physical()
    }

    /// Encodes the idea that we can always unmap from user aspaces.
    pub(super) fn enlarge_arch_unmap(&self) -> ArchUnmapOptions {
        if self.can_enlarge_arch_unmap() {
            ARCH_UNMAP_OPTION_ENLARGE
        } else {
            ARCH_UNMAP_OPTION_NONE
        }
    }

    pub(super) fn last_fault_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> Option<&'a VmMapping> {
        // SAFETY: The aspace lock is held.
        let last_fault = *unsafe { self.last_fault.get(token) };
        // SAFETY: The aspace lock is held for `'a`, so by the `last_fault` invariant `last_fault`
        // points to an `Alive` `VmMapping` in the VMAR tree that stays allocated for `'a`.
        // `VmMapping` is only accessed through shared references.
        last_fault.map(|last_fault| unsafe { last_fault.as_ref() })
    }

    pub(super) fn clear_last_fault_locked(&self, token: &mut LockToken<'_, VmAspaceLockClass>) {
        // SAFETY: The aspace lock is held.
        *unsafe { self.last_fault.get_mut(token) } = None;
    }

    pub(super) fn vdso_code_mapping_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> Option<&'a VmMapping> {
        // SAFETY: The aspace lock is held.
        unsafe { self.vdso_code_mapping.get(token) }.as_deref()
    }

    pub(super) fn set_vdso_code_mapping_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        mapping: RefPtr<VmMapping>,
    ) {
        // SAFETY: The aspace lock is held.
        let vdso_code_mapping = unsafe { self.vdso_code_mapping.get_mut(token) };
        debug_assert!(vdso_code_mapping.is_none());
        *vdso_code_mapping = Some(mapping);
    }

    /// Helper function to test for collision with `vdso_code_mapping`.
    pub(super) fn intersects_vdso_code_locked(
        &self,
        token: &LockToken<'_, VmAspaceLockClass>,
        base: VAddr,
        size: usize,
    ) -> bool {
        if let Some(vdso_code_mapping) = self.vdso_code_mapping_locked(token) {
            return range_check::intersects_offset_len(
                vdso_code_mapping.base().0 as u64,
                vdso_code_mapping.size() as u64,
                base.0 as u64,
                size as u64,
            );
        }

        false
    }

    pub fn is_aslr_enabled(&self) -> bool {
        self.aslr_config.enabled
    }

    /// Expose the PRNG for ASLR to `VmAddressRegion`.
    #[inline]
    pub fn aslr_prng(&self) -> &Prng {
        debug_assert!(self.is_aslr_enabled());
        &self.aslr_prng
    }

    /// Providing entropy bits for ASLR.
    #[inline]
    pub(super) fn aslr_entropy_bits(&self, compact: bool) -> u8 {
        if compact { self.aslr_config.compact_entropy_bits } else { self.aslr_config.entropy_bits }
    }

    /// Returns true if this address space has been destroyed.
    pub fn is_destroyed(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        *guard.aspace_destroyed()
    }

    /// Returns the singleton kernel address space. May only be called after `vm_init_preheap`.
    #[inline]
    pub fn kernel_aspace() -> &'static VmAspace {
        // SAFETY: `KERNEL_ASPACE` is initialized during early boot in `vm_init_preheap` before any
        // caller invokes `kernel_aspace`.
        unsafe { KERNEL_ASPACE.get_unchecked() }
    }

    /// Returns a new reference to the singleton kernel address space. May only be called after
    /// `vm_init_preheap`.
    pub fn kernel_aspace_ref() -> RefPtr<VmAspace> {
        RefPtr::from_ref(Self::kernel_aspace())
    }

    /// Gets the root VMAR (briefly acquires the aspace lock).
    ///
    /// May return `None` if the aspace has been destroyed or is not yet initialized.
    pub fn root_vmar(&self) -> Option<RefPtr<VmAddressRegion>> {
        ksync::lock!(let guard = self.lock_lock());
        guard.root_vmar().clone()
    }

    /// Returns the root address region while holding the aspace lock.
    #[inline]
    pub fn root_vmar_locked<'a>(
        &'a self,
        token: &'a LockToken<'_, VmAspaceLockClass>,
    ) -> Option<&'a RefPtr<VmAddressRegion>> {
        // SAFETY: The aspace lock is held.
        unsafe { self.root_vmar.get(token) }.as_ref()
    }

    /// Sets the per-thread address space pointer to this address space.
    ///
    /// # Safety
    ///
    /// `this` points to a live `VmAspace`. `thread` points to a live `Thread` that has not started
    /// running, no chain lock transaction is active on this CPU, and while this aspace is assigned
    /// to the thread, it is statically allocated or a reference to it is held elsewhere.
    pub unsafe fn attach_to_thread(this: NonNull<Self>, thread: ThreadPtr) {
        // SAFETY: `this` points to a live `VmAspace`.
        unsafe { this.as_ref() }.canary.assert();
        let t = thread.as_raw();

        // Attach to thread is the one place where a different thread is allowed to
        // set a thread's address space.  This is only permitted because the thread
        // cannot be running yet.  Once the thread starts, only it will be allowed to
        // change its address space.
        thread_lock_guard!(
            guard,
            // SAFETY: `t` is non-null and points to a valid `Thread` that stays live past this
            // scope, this CPU is not in a chain lock transaction and none starts while the guard
            // lives, and we do not block.
            unsafe { ThreadLockGuard::target(t, clt_tag!("VmAspace::AttachToThread")) }
        );
        let mut proof = guard.proof();

        // not prepared to handle setting a new address space or one on a running thread
        debug_assert!(proof.get_aspace_ref_locked().is_none());
        debug_assert!(proof.state() != ThreadStateKind::Running);

        // SAFETY: The thread has not started running. Until this aspace is replaced, or the thread
        // is destroyed or has context-switched for the last time, it is statically allocated or a
        // reference to it is held elsewhere.
        unsafe { proof.switch_aspace(this) };
    }

    /// Dumps information about this address space to the kernel console.
    pub fn dump(&self) {
        ksync::lock!(let region_guard = self.region_lock().lock());
        ksync::lock!(let guard = self.lock_lock());
        self.canary.assert();
        kprintln!(
            "as {:p} [{:#x} {:#x}] sz {:#x} typ {:u} ref {:d} '{:cs}' destroyed {:d}",
            self,
            self.base(),
            self.base().wrapping_add(self.size()).wrapping_sub(1),
            self.size(),
            self.type_ as u32,
            self.ref_count().ref_count_debug(),
            self.name_ptr(),
            *guard.aspace_destroyed() as i32
        );

        if let Some(root_vmar) = guard.root_vmar().as_ref() {
            root_vmar.dump_locked(region_guard.token(), guard.token(), 1, true);
        }
    }

    /// Drops hardware page tables across all user address spaces.
    pub fn drop_all_user_page_tables() {
        ksync::lock!(let guard = AspaceList::get().lock_lock());

        for a in guard.list().forward_iter() {
            // SAFETY: `a` is in the aspace list, so its arch aspace is initialized and not
            // destroyed.
            unsafe { a.drop_user_page_tables() };
        }
    }

    /// Drops hardware page tables for this address space if it is a non-unified user address
    /// space.
    ///
    /// # Safety
    ///
    /// If this is a user aspace, its arch aspace is initialized and not destroyed.
    unsafe fn drop_user_page_tables(&self) {
        // Unified aspaces have a size of 0 and manage no mappings themselves.
        if !self.is_user() || self.size() == 0 {
            return;
        }
        ksync::lock!(let _guard = self.lock().lock());
        // SAFETY: The arch aspace is initialized, not destroyed, not unified (a unified aspace has
        // a size of 0) and was created with this aspace's range, so no large page extends past the
        // range. The range holds only user mappings, which no Rust reference points into and which
        // are faulted back in when accessed.
        let _ = unsafe {
            self.arch_aspace().unmap(
                self.base(),
                self.size() / kernel_page::SIZE,
                ARCH_UNMAP_OPTION_ENLARGE,
            )
        };
    }

    /// Dumps information about all address spaces in the system to the kernel console.
    pub fn dump_all_aspaces() {
        ksync::lock!(let guard = AspaceList::get().lock_lock());

        for a in guard.list().forward_iter() {
            a.dump();
        }
    }

    /// Harvests all accessed information across all user mappings and updates any page age
    /// information for terminal mappings, and potentially harvests page tables depending on the
    /// passed in action.
    ///
    /// This requires holding the aspace list lock over the entire duration and
    /// whilst not a commonly used lock this function should still only be called infrequently to
    /// avoid monopolizing the lock.
    pub fn harvest_all_user_accessed_bits(
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) {
        vm_ktrace_duration!(2, "VmAspace::HarvestAllUserAccessedBits");
        ksync::lock!(let guard = AspaceList::get().lock_lock());
        let token = guard.token();

        for a in guard.list().forward_iter() {
            if a.is_user() && a.size() > 0 {
                // Forbid PT reclamation and accessed bit harvesting on high priority aspaces.
                if a.is_high_memory_priority() {
                    continue;
                }
                let count = a.pt_harvest_since_active(token);
                // The arch_aspace is only destroyed in the VmAspace destructor *after* the aspace
                // is removed from the aspaces list. As we hold the aspace list lock we know that
                // this destructor has not completed, and so the arch_aspace has not been destroyed.
                // Even if the actual VmAspace has been destroyed, it is still completely safe to
                // walk to the hardware page tables, there just will not be anything there.
                // First we always check `accessed_since_last_check` (even if we could separately
                // infer that we have to do a harvest) in order to clear the state from it.
                let mut harvest = true;
                if a.arch_aspace().accessed_since_last_check(
                    terminal_action == TerminalAction::UpdateAgeAndHarvest,
                ) {
                    // The aspace has been accessed since some kind of harvest last happened, so we
                    // must do a new one. Reset our counter of how many pt reclamations we've done
                    // based on what kind scan this is.
                    if non_terminal_action == NonTerminalAction::FreeUnaccessed {
                        // This is set to one since we haven't yet performed the harvest, and so if
                        // next time the call to `accessed_since_last_check` returns false, then it
                        // will be true that one harvest has been done since last active.
                        // Alternatively, if next time `accessed_since_last_check` returns true,
                        // then we'll just re-set this back to 1 again.
                        count.set(1);
                    } else {
                        count.set(0);
                    }
                } else if non_terminal_action == NonTerminalAction::FreeUnaccessed
                    && count.get() < 2
                {
                    // The aspace hasn't been active, but we haven't yet performed two successive
                    // pt reclamations. Since the first pt reclamation only removes accessed
                    // information, the second is needed to actually do the reclamation.
                    count.set(count.get() + 1);
                } else {
                    // Either this is not a request to harvest pt information, or enough pt
                    // harvesting has been done, and so we can skip as the aspace should now be at
                    // a fixed point with no new information.
                    harvest = false;
                }
                if harvest {
                    // SAFETY: `a` is a user aspace with a nonzero size, so its arch aspace is
                    // initialized, not unified and covers only user mappings of PMM pages, whose
                    // unaccessed page tables may be freed. It is not destroyed while `a` is in the
                    // aspace list.
                    let result = unsafe {
                        a.arch_aspace().harvest_accessed(
                            a.base(),
                            a.size() / kernel_page::SIZE,
                            non_terminal_action,
                            terminal_action,
                        )
                    };
                    debug_assert!(result.is_ok());
                    VM_ASPACE_ACCESSED_HARVESTS_PERFORMED.add(1);
                } else {
                    VM_ASPACE_ACCESSED_HARVESTS_SKIPPED.add(1);
                }
            }
        }
    }

    fn pt_harvest_since_active<'a>(
        &'a self,
        token: &'a LockToken<'_, AspaceListLockClass>,
    ) -> &'a Cell<u8> {
        // SAFETY: `ASPACES_LIST` is the only `AspaceList`, so `token` is for its `lock`, the lock
        // that guards the field. That lock is an exclusive mutex, so while `token` is borrowed
        // only this thread holds it, and the returned `Cell`, which lives no longer than that
        // borrow, is accessed by no other thread.
        unsafe { self.pt_harvest_since_active.get(token) }
    }

    /// # Safety
    ///
    /// `self` is pinned, not in the aspace list and has an initialized arch aspace, and is erased
    /// from the list before its arch aspace is destroyed or it is freed: it is owned by `RefPtr`s,
    /// whose `recycle` erases it before dropping it, or is never dropped.
    unsafe fn aspaces_list_push_back(&self) {
        ksync::lock!(let mut guard = AspaceList::get().lock_lock());
        // SAFETY: `self` is not in any list, as its node is only used for the aspace list. It is
        // pinned, has an initialized arch aspace, and is erased from the list before its arch
        // aspace is destroyed or it is freed.
        unsafe { guard.as_mut().list_mut().push_back_raw(NonNull::from(self)) };
    }

    /// # Safety
    ///
    /// `self` is pinned, not in the aspace list and has an initialized arch aspace, and is erased
    /// from the list before its arch aspace is destroyed or it is freed: it is owned by `RefPtr`s,
    /// whose `recycle` erases it before dropping it, or is never dropped.
    unsafe fn aspaces_list_push_front(&self) {
        ksync::lock!(let mut guard = AspaceList::get().lock_lock());
        // SAFETY: `self` is not in any list, as its node is only used for the aspace list. It is
        // pinned, has an initialized arch aspace, and is erased from the list before its arch
        // aspace is destroyed or it is freed.
        unsafe { guard.as_mut().list_mut().push_front_raw(NonNull::from(self)) };
    }

    fn aspaces_list_erase(&self) {
        ksync::lock!(let mut guard = AspaceList::get().lock_lock());
        if self.get_node().in_container() {
            // SAFETY: `self` is in the aspace list, the only list its node is used for.
            unsafe { guard.as_mut().list_mut().erase(self) };
        }
    }

    /// Generates a soft fault against this address space.
    ///
    /// This is similar to `page_fault` except:
    /// * This address space may not currently be active and this does not have to be called from
    ///   the hardware exception handler.
    /// * May be invoked spuriously in situations where the hardware mappings would have prevented a
    ///   real `page_fault` from occurring.
    ///
    /// May block on page requests and must be called without locks held.
    ///
    /// # Safety
    ///
    /// If `flags` contains [`fault::flag::ACCESS`](super::fault::flag::ACCESS), the arch aspace is
    /// initialized and this aspace is not unified. Otherwise the calling thread holds no
    /// `VmObject` lock, and this is a user or guest-physical aspace.
    pub unsafe fn soft_fault(&self, va: usize, flags: u32) -> Result<(), Status> {
        // With the current implementation we can just reuse the internal `page_fault` mechanism.
        // SAFETY: If `flags` contains `pf_flag::ACCESS`, the arch aspace is initialized and this
        // aspace is not unified. Otherwise the calling thread holds no `VmObject` lock, and this is
        // a user or guest-physical aspace.
        unsafe { self.page_fault(va, flags | pf_flag::SW_FAULT) }
    }

    /// Similar to `soft_fault`, but additionally takes a length indicating that the range of
    /// `[va, va+len)` is expected to be accessed with `flags` after resolving this fault. The
    /// address space can take this range as a hint to attempt to preemptively avoid future faults.
    ///
    /// There are no alignment restrictions on `va` or `len`, although it is assumed that `len` is
    /// greater than zero.
    ///
    /// # Safety
    ///
    /// If `flags` contains [`fault::flag::ACCESS`](super::fault::flag::ACCESS), the arch aspace is
    /// initialized and this aspace is not unified. Otherwise the calling thread holds no
    /// `VmObject` lock, and this is a user or guest-physical aspace.
    pub unsafe fn soft_fault_in_range(
        &self,
        va: usize,
        flags: u32,
        len: usize,
    ) -> Result<(), Status> {
        // If the fault was actually an access fault, handle that and return.
        if flags & pf_flag::ACCESS != 0 {
            // Assert that the translation bit is not set.
            debug_assert!((flags & pf_flag::NOT_PRESENT) == 0);
            // SAFETY: The arch aspace is initialized and this aspace is not unified.
            return unsafe { self.accessed_fault(va) };
        }

        vm_ktrace_duration!(
            2,
            "VmAspace::SoftFaultInRange",
            "va" => va as u64,
            "flags" => flags,
            "len" => len as u64
        );

        debug_assert!(len > 0);
        let (range_end, overflow) = len.wrapping_sub(1).overflowing_add(va);
        if overflow {
            hint::cold_path();
            return Err(Status::OUT_OF_RANGE);
        }
        debug_assert!(va <= range_end);

        let va_page_base = kernel_page::round_down(va);
        let last_page_base = kernel_page::round_down(range_end);
        let extra_pages = last_page_base.wrapping_sub(va_page_base) / kernel_page::SIZE;
        // SAFETY: The calling thread holds no `VmObject` lock, `va_page_base` is page-aligned and
        // `extra_pages` is less than `usize::MAX`. This is a user or guest-physical aspace.
        unsafe { self.page_fault_internal(va_page_base, flags, extra_pages) }
    }

    /// Generates an accessed flag fault against this address space.
    ///
    /// This is a specialized version of `soft_fault` that will only resolve a potential missing
    /// access flag and nothing else.
    ///
    /// # Safety
    ///
    /// The arch aspace is initialized and this aspace is not unified.
    pub unsafe fn accessed_fault(&self, va: usize) -> Result<(), Status> {
        vm_ktrace_duration!(2, "VmAspace::AccessedFault", "va" => ArgValue::Pointer(va));
        // There are no permissions etc associated with accessed bits so we can skip any vmar
        // walking and just let the hardware aspace walk for the virtual address.
        let va = kernel_page::round_down(va);
        // SAFETY: The arch aspace is initialized and not unified. `destroy` is not called
        // concurrently: only the `VmAspace` destructor calls it, and `self` is live.
        unsafe { self.arch_aspace().mark_accessed(va, 1) }
    }

    /// Page fault routine.
    ///
    /// Should only be called by the hypervisor or by `Thread::Current::Fault`.
    ///
    /// # Safety
    ///
    /// If `flags` contains [`fault::flag::ACCESS`](super::fault::flag::ACCESS), the arch aspace is
    /// initialized and this aspace is not unified. Otherwise the calling thread holds no
    /// `VmObject` lock, and this is a user or guest-physical aspace.
    pub unsafe fn page_fault(&self, mut va: usize, flags: u32) -> Result<(), Status> {
        // If the fault was actually an access fault, handle that and return.
        if flags & pf_flag::ACCESS != 0 {
            // Assert that the translation bit is not set.
            debug_assert!((flags & pf_flag::NOT_PRESENT) == 0);
            // SAFETY: The arch aspace is initialized and this aspace is not unified.
            return unsafe { self.accessed_fault(va) };
        }

        vm_ktrace_duration!(2, "VmAspace::PageFault", "va" => va as u64, "flags" => flags);

        // With the original va logged in the traces can now convert to a page aligned address
        // suitable for passing to `page_fault_internal`.
        va = kernel_page::round_down(va);

        // SAFETY: The calling thread holds no `VmObject` lock, `va` is page-aligned and
        // `additional_pages` is 0. This is a user or guest-physical aspace.
        unsafe { self.page_fault_internal(va, flags, 0) }
    }

    /// Internal helper for resolving page faults. Takes an aligned va.
    ///
    /// # Safety
    ///
    /// The calling thread holds no `VmObject` lock, `va` is page-aligned and `additional_pages` is
    /// less than `usize::MAX`. This is a user or guest-physical aspace.
    #[inline(never)]
    unsafe fn page_fault_internal(
        &self,
        mut va: usize,
        mut flags: u32,
        mut additional_pages: usize,
    ) -> Result<(), Status> {
        // As page fault resolution may take arbitrary locks and wait on page requests, ensure our
        // caller is not holding any locks.
        lockdep::assert_no_locks_held();

        self.canary.assert();
        debug_assert!((flags & pf_flag::ACCESS) == 0);
        if self.is_guest_physical() {
            flags &= !pf_flag::USER;
            flags |= pf_flag::GUEST;
        }

        stack_pin_init!(let page_request = MultiPageRequest::new());
        loop {
            let locked = {
                ksync::lock!(let mut guard = self.lock().lock());
                let token = guard.token();
                // SAFETY: The aspace lock is held.
                debug_assert!(!*unsafe { self.aspace_destroyed.get(token) });
                // First check if we're faulting on the same mapping as last time to short-circuit
                // the vmar walk.
                let last_fault = match self.last_fault_locked(token) {
                    Some(last_fault) if last_fault.is_in_range(VAddr(va), 1) => {
                        VM_ASPACE_LAST_FAULT_HIT.add(1);
                        Some((last_fault, token))
                    }
                    _ => {
                        hint::cold_path();
                        VM_ASPACE_LAST_FAULT_MISS.add(1);
                        let root_vmar =
                            self.root_vmar_locked(guard.token()).expect("root vmar is null");
                        // Stash the mapping we found as the most recent fault. As we just found
                        // this mapping in the VMAR tree we know it's in the `Alive` state (or is
                        // null), satisfying that requirement that allows us to record this as a raw
                        // pointer.
                        let found_mapping = root_vmar
                            .find_mapping_locked(guard.token(), VAddr(va))
                            .map(NonNull::from);
                        // SAFETY: The aspace lock is held.
                        *unsafe { self.last_fault.get_mut(guard.as_mut().token_mut()) } =
                            found_mapping;
                        let token = guard.token();
                        self.last_fault_locked(token).map(|mapping| (mapping, token))
                    }
                };
                last_fault.map(|(last_fault, token)| {
                    let object = last_fault.vmo_locked(token).expect("mapping has no object");
                    (object, RefPtr::from_ref(last_fault))
                })
            };
            let Some((object, mapping)) = locked else {
                hint::cold_path();
                return Err(Status::NOT_FOUND);
            };

            // SAFETY: `object` is the mapping's object, or was until the mapping's object was
            // reset, and the object lock is not held. `va` is page-aligned and within the mapping,
            // `additional_pages` is less than `usize::MAX`, and `page_request` does not hold an
            // active request. The mapping's aspace, `self`, is a user or guest-physical aspace.
            let (status, mapped) = unsafe {
                mapping.page_fault(
                    VAddr(va),
                    flags,
                    additional_pages,
                    &object,
                    page_request.as_ref(),
                )
            };
            if status == Err(Status::SHOULD_WAIT) {
                hint::cold_path();
                // If the page fault originated in kernel mode (usercopy), we cannot safely suspend
                // the thread without potential data loss. See https://fxbug.dev/42084841 for
                // details.
                let suspendable = flags & pf_flag::USER != 0;
                let st = page_request.as_ref().wait(suspendable);
                if let Err(st) = st {
                    hint::cold_path();
                    if st == Status::TIMED_OUT {
                        self.dump_root_vmar_after_timeout();
                    }
                    return Err(st);
                }
                // Before retrying the page fault, take into account how many pages got mapped on
                // the previous attempt (if any).
                if mapped > 0 {
                    va += kernel_page::SIZE * mapped as usize;
                    // For mapped to be non-zero and we were able to have an error then we must have
                    // requested a non-zero amount of additional pages, and not all of them were
                    // able to be mapped.
                    debug_assert!(mapped as usize <= additional_pages);
                    additional_pages -= mapped as usize;
                }
            }
            // Need to retry the fault if we had to wait on a page request, or if our previous
            // attempt at resolving the fault raced with the mapping being deleted.
            if !(status == Err(Status::SHOULD_WAIT) || status == Err(Status::UNAVAILABLE)) {
                return status;
            }
            hint::cold_path();
        }
    }

    /// Dumps the root VMAR after a page request timed out. Outlined and marked cold so its two lock
    /// guards stay out of `page_fault_internal`'s frame.
    #[cold]
    #[inline(never)]
    fn dump_root_vmar_after_timeout(&self) {
        ksync::lock!(let region_guard = self.region_lock().lock());
        ksync::lock!(let guard = self.lock_lock());
        let root_vmar = guard.root_vmar().as_ref().expect("root vmar is null");
        root_vmar.dump_locked(region_guard.token(), guard.token(), 0, false);
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// The cache policy in `arch_mmu_flags` is appropriate for all mappings of
    /// `[paddr, paddr + size)` with `size` rounded up to a whole number of pages. Creating a
    /// mapping with the specified range and flags is sound.
    #[expect(clippy::too_many_arguments)]
    pub unsafe fn alloc_physical(
        &self,
        name: &[u8],
        size: usize,
        specific: Option<usize>,
        align_pow2: u8,
        paddr: PAddr,
        mut vmm_flags: u32,
        mut arch_mmu_flags: ArchMmuFlags,
    ) -> Result<usize, Status> {
        self.canary.assert();
        ltracef!(
            "aspace {:p} name '{}' size {:#x} ptr {:p} paddr {:#x} vmm_flags 0x{:x} \
             arch_mmu_flags 0x{:x}\n",
            self,
            zr::from_utf8_lossy(name),
            size,
            ptr::without_provenance::<c_void>(specific.unwrap_or(0)),
            paddr.0,
            vmm_flags,
            arch_mmu_flags
        );

        debug_assert!(kernel_page::is_aligned(paddr.0));

        if size == 0 {
            return Ok(0);
        }
        if !kernel_page::is_aligned(paddr.0) {
            return Err(Status::INVALID_ARGS);
        }

        let size = kernel_page::round_up(size);

        // create a vm object to back it
        let vmo = VmObjectPhysical::create(paddr, size)?;
        let _ = vmo.set_name(name);

        // force it to be mapped up front
        // TODO: add new flag to precisely mean pre-map
        vmm_flags |= vmm_flag::COMMIT;

        // Apply the cache policy
        // SAFETY: The cache policy is appropriate for all future mappings of `vmo`.
        if unsafe { vmo.set_mapping_cache_policy(arch_mmu_flags & ARCH_MMU_FLAG_CACHE_MASK) }
            .is_err()
        {
            return Err(Status::INVALID_ARGS);
        }

        arch_mmu_flags &= !ARCH_MMU_FLAG_CACHE_MASK;
        // SAFETY: Creating a mapping with the specified range and flags is sound.
        unsafe {
            self.map_object_internal_impl(
                VmObjectPhysical::into_vm_object(vmo),
                name,
                0,
                size,
                specific,
                align_pow2,
                vmm_flags,
                arch_mmu_flags,
            )
        }
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// Creating a mapping with the specified range and flags is sound.
    pub unsafe fn alloc_contiguous(
        &self,
        name: &[u8],
        size: usize,
        specific: Option<usize>,
        align_pow2: u8,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<usize, Status> {
        self.canary.assert();
        ltracef!(
            "aspace {:p} name '{}' size 0x{:x} ptr {:p} align {} vmm_flags 0x{:x} \
             arch_mmu_flags 0x{:x}\n",
            self,
            zr::from_utf8_lossy(name),
            size,
            ptr::without_provenance::<c_void>(specific.unwrap_or(0)),
            align_pow2,
            vmm_flags,
            arch_mmu_flags
        );

        let size = kernel_page::round_up(size);
        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // test for invalid flags
        if vmm_flags & vmm_flag::COMMIT == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // create a vm object to back it
        let vmo = VmObjectPaged::create_contiguous(pmm::ALLOC_FLAG_ANY, size as u64, align_pow2)?;
        let _ = vmo.set_name(name);

        // SAFETY: Creating a mapping with the specified range and flags is sound.
        unsafe {
            self.map_object_internal_impl(
                VmObjectPaged::into_vm_object(vmo),
                name,
                0,
                size,
                specific,
                align_pow2,
                vmm_flags,
                arch_mmu_flags,
            )
        }
    }

    /// Legacy function to assist in the transition to VMARs.
    ///
    /// Assumes a flat VMAR structure in which all VMOs are mapped as children of the root.
    /// Will assert if used on user address spaces.
    ///
    /// # Safety
    ///
    /// `va` is in a mapping of the root VMAR whose whole VMO range is pinned by a pin the caller
    /// owns, and nothing uses that mapping any more.
    pub unsafe fn free_region(&self, va: usize) -> Result<(), Status> {
        debug_assert!(!self.is_user());

        let Some(root_vmar) = self.root_vmar() else {
            return Err(Status::NOT_FOUND);
        };
        let Some(r) = root_vmar.find_region(VAddr(va)) else {
            return Err(Status::NOT_FOUND);
        };

        let Some(mapping) = r.into_vm_mapping() else {
            return Err(Status::BAD_STATE);
        };
        // Cache the VMO information for this mapping so that we can unpin. We must destroy the
        // mapping first though, otherwise we would be unpinning a live mapping.
        let vmo = mapping.vmo();
        let vmo_offset = mapping.object_offset();
        let unpin_size = mapping.size() as u64;
        let status = mapping.destroy();
        vmo.expect("mapping has no vmo").unpin(vmo_offset, unpin_size);
        status
    }

    /// Convenience method for traversing the tree of VMARs to find the deepest
    /// VMAR in the tree that includes `va`.
    ///
    /// Returns `None` if the aspace has been destroyed or is not yet initialized.
    pub fn find_region(&self, va: usize) -> Option<OwnedVmAddressRegionOrMapping> {
        let mut vmar = self.root_vmar()?;
        loop {
            let Some(next) = vmar.find_region(VAddr(va)) else {
                return Some(vmar.into());
            };

            vmar = match next.into_region_or_mapping() {
                Ok(region) => {
                    region.common_canary().assert();
                    region
                }
                Err(mapping) => return Some(mapping.into()),
            };
        }
    }

    /// Internal use function for mapping VMOs.  Do not use.  This is exposed in
    /// the public API purely for tests.
    ///
    /// # Safety
    ///
    /// The caller must ensure that creating a mapping with the specified range and flags is sound.
    #[expect(clippy::too_many_arguments)]
    pub unsafe fn map_object_internal(
        &self,
        vmo: RefPtr<VmObject>,
        name: &[u8],
        offset: u64,
        size: usize,
        align_pow2: u8,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<*mut c_void, Status> {
        // SAFETY: Creating a mapping with the specified range and flags is sound.
        unsafe {
            self.map_object_internal_impl(
                vmo,
                name,
                offset,
                size,
                None,
                align_pow2,
                vmm_flags,
                arch_mmu_flags,
            )
        }
        .map(ptr::with_exposed_provenance_mut)
    }

    /// Internal implementation for mapping a VMO into this address space.
    ///
    /// # Safety
    ///
    /// Creating a mapping with the specified range and flags is sound.
    #[expect(clippy::too_many_arguments)]
    unsafe fn map_object_internal_impl(
        &self,
        vmo: RefPtr<VmObject>,
        name: &[u8],
        offset: u64,
        size: usize,
        specific: Option<usize>,
        align_pow2: u8,
        vmm_flags: u32,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<usize, Status> {
        self.canary.assert();
        ltracef!(
            "aspace {:p} name '{}' vmo {:p}, offset {:#x} size {:#x} ptr {:p} align {} vmm_flags \
             {:#x} arch_mmu_flags {:#x}\n",
            self,
            zr::from_utf8_lossy(name),
            RefPtr::as_ptr(&vmo),
            offset,
            size,
            ptr::without_provenance::<c_void>(specific.unwrap_or(0)),
            align_pow2,
            vmm_flags,
            arch_mmu_flags
        );

        debug_assert!(!self.is_user());

        let size = kernel_page::round_up(size);
        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if !kernel_page::is_aligned(offset as usize) {
            return Err(Status::INVALID_ARGS);
        }

        let mut vmar_offset: usize = 0;
        // if they're asking for a specific spot or starting address, copy the address
        if vmm_flags & vmm_flag::VALLOC_SPECIFIC != 0 {
            // can't ask for a specific spot and then not provide one
            let Some(addr) = specific else {
                return Err(Status::INVALID_ARGS);
            };
            vmar_offset = addr;

            // check that it's page aligned
            if !kernel_page::is_aligned(vmar_offset) || vmar_offset < self.base() {
                return Err(Status::INVALID_ARGS);
            }

            vmar_offset -= self.base();
        }

        let mut vmar_flags = 0;
        if vmm_flags & vmm_flag::VALLOC_SPECIFIC != 0 {
            vmar_flags |= vmar_flag::SPECIFIC;
        }

        // Create the mappings with all of the CAN_* RWX flags, so that
        // `protect()` can transition them arbitrarily.  This is not desirable for the
        // long-term.
        vmar_flags |= vmar_flag::CAN_RWX_FLAGS;

        // TODO: Enforce all callers to be passing `vmm_flag::COMMIT`.
        vmo.commit_range_pinned(offset, size as u64, true)?;

        // allocate a region and put it in the aspace list
        let r = self.root_vmar().expect("root vmar is null").create_vm_mapping_no_overwrite(
            vmar_offset,
            size,
            align_pow2,
            vmar_flags,
            vmo,
            offset,
            arch_mmu_flags,
        )?;

        // if we're committing it, map the region now
        // TODO: Enforce all callers to be passing `vmm_flag::COMMIT`.
        if vmm_flags & vmm_flag::COMMIT != 0 {
            r.mapping.map_range(0, size, true, false)?;
        }

        // return the vaddr
        Ok(r.base)
    }

    /// Returns the vDSO base address for this address space.
    pub fn vdso_base_address(&self) -> usize {
        ksync::lock!(let guard = self.lock_lock());
        guard.vdso_code_mapping().as_ref().map_or(0, |m| VDso::base_address(m, guard.token()))
    }

    /// Returns the vDSO code address for this address space.
    pub fn vdso_code_address(&self) -> usize {
        ksync::lock!(let guard = self.lock_lock());
        guard.vdso_code_mapping().as_ref().map_or(0, |m| m.base().0)
    }

    /// Counts memory usage under the VmAspace.
    pub fn get_memory_usage(&self) -> Result<VmUsage, Status> {
        let mut vc = VmCounter::default();
        let root_vmar = self.root_vmar().ok_or(Status::INTERNAL)?;
        if root_vmar.enumerate_children(&mut vc).is_err() {
            return Err(Status::INTERNAL);
        }
        Ok(vc.usage)
    }

    /// Returns whether this address space is currently set to be a high memory priority.
    pub fn is_high_memory_priority(&self) -> bool {
        // Relaxed ordering is sufficient because this counter is only used as a standalone
        // heuristic and does not synchronize access to any other state.
        let val = self.high_priority_count.load(Ordering::Relaxed);
        debug_assert!(val >= 0);
        val != 0
    }

    /// Increments or decrements the priority count of this aspace. The high priority count is used
    /// to control active page table reclamation, and applies to the whole aspace. The count is
    /// never allowed to go negative and so callers must only subtract what they have already added.
    /// Further, callers are required to remove any additions before the aspace is destroyed.
    #[inline(always)]
    pub(super) fn change_high_priority_count_locked(
        &self,
        token: &mut LockToken<'_, VmAspaceLockClass>,
        delta: i64,
    ) {
        // SAFETY: The aspace lock is held.
        debug_assert!(!*unsafe { self.aspace_destroyed.get(token) });
        self.change_high_priority_count(delta);
    }

    #[inline(never)]
    fn change_high_priority_count(&self, delta: i64) {
        let old = self.high_priority_count.fetch_add(delta, Ordering::SeqCst);
        if old == 0 {
            VM_ASPACE_HIGH_PRIORITY.add(1);
        } else if delta + old == 0 {
            VM_ASPACE_HIGH_PRIORITY.add(-1);
        }
        debug_assert!(delta + old >= 0);
    }

    /// Finds the memory mapping for `vaddr` in this address space.
    pub fn find_mapping(&self, vaddr: usize) -> Option<RefPtr<VmMapping>> {
        self.find_region(vaddr)?.into_vm_mapping()
    }
}

/// Counts memory usage under a VmAspace.
#[derive(Default)]
struct VmCounter {
    usage: VmUsage,
}

impl VmEnumerator for VmCounter {
    fn on_vm_mapping(
        &mut self,
        map: &VmMapping,
        _vmar: &VmAddressRegion,
        _depth: u32,
        mut guard: Pin<&mut VmAspaceGuard<'_>>,
    ) -> Status {
        let map_size = map.size();
        let object_offset = map.object_offset();
        let vmo: RefPtr<VmObject> =
            map.vmo_locked(guard.token()).expect("mapping in aspace must have a VMO");

        let usage = &mut self.usage;
        guard.as_mut().call_unlocked(|| {
            usage.mapped_bytes += map_size;

            let counts: AttributionCounts =
                vmo.get_attributed_memory_in_range(object_offset, map_size as u64);
            let share_count = vmo.share_count();
            // Portions of the VMO itself may have sharing via copy-on-write and so, regardless of
            // how many aspaces it is mapped into (represented by share_count), it may have a mix of
            // reported private and non private bytes. At this point we can only perform
            // approximations as we only have an aggregate VMO aspace sharing factor, and aggregate
            // counts, with no ability to precisely know what portions of the private and shared
            // vmo bytes are actually part of what level of aspace sharing.
            // The approximation chosen here is to consider any shared bytes as shared, even if
            // this specific VMO does not have other mappings, and to assume that if the VMO has
            // multiple mappings that any private VMO bytes are actually shared.
            if share_count == 1 {
                usage.private_bytes += counts.private_uncompressed_bytes;
                usage.shared_bytes += counts.uncompressed_bytes - counts.private_uncompressed_bytes;
                usage.scaled_shared_bytes += (counts.scaled_uncompressed_bytes
                    - counts.private_uncompressed_bytes as u64)
                    / share_count as u64;
            } else {
                usage.shared_bytes += counts.uncompressed_bytes;
                usage.scaled_shared_bytes += counts.scaled_uncompressed_bytes / share_count as u64;
            }
        });
        Status::NEXT
    }
}

/// # Safety
///
/// `name` is null or points to a NUL-terminated string that is not modified during the call.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_aspace_create_with_opts(
    base: usize,
    size: usize,
    type_: Type,
    name: *const c_char,
    share_opt: ShareOpt,
) -> Option<RefPtr<VmAspace>> {
    // SAFETY: `name` is null or points to a NUL-terminated string that is not modified during the
    // call.
    let name = unsafe { name_or_unnamed(name) };
    VmAspace::create_with_opts(base, size, type_, name, share_opt)
}

/// # Safety
///
/// `this` points to a `VmAspace` other than the kernel aspace whose last reference was released:
/// it is in storage returned by `allocate_exposed`, has no references left and is not used
/// afterwards. As `VmAspace::new` requires, its arch aspace has no mappings left unless it is
/// unified, and by the invariant on its `arch_aspace` field, it is not active on any CPU and no
/// unified address space built from it is live.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_aspace_recycle(this: NonNull<VmAspace>) {
    // SAFETY: The caller upholds the requirements of `recycle`.
    unsafe { VmAspace::recycle(this) }
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_destroy(this: &VmAspace) -> Result<(), Status> {
    this.destroy()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_kernel_aspace() -> *mut VmAspace {
    KERNEL_ASPACE.as_ptr()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_is_user(this: &VmAspace) -> bool {
    this.is_user()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_arch_aspace(this: &VmAspace) -> &ArchVmAspace {
    this.arch_aspace()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_lock(this: &VmAspace) -> &VmAspaceMutex {
    this.lock()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_root_vmar(this: &VmAspace) -> Option<RefPtr<VmAddressRegion>> {
    this.root_vmar()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_aspace_is_destroyed(this: &VmAspace) -> bool {
    this.is_destroyed()
}

/// # Safety
///
/// `name` points to a null-terminated string that is not modified during the call. `ptr` is null
/// or aligned and valid for reads and writes, and `*ptr` is initialized if `vmm_flags` contains
/// `vmm_flag::VALLOC_SPECIFIC`. The cache policy in `arch_mmu_flags` is appropriate for all
/// mappings of `[paddr, paddr + size)` with `size` rounded up to a whole number of pages. Creating
/// a mapping with the specified range and flags is sound.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_vm_aspace_alloc_physical(
    this: &VmAspace,
    name: *const c_char,
    size: usize,
    ptr: *mut *mut c_void,
    align_pow2: u8,
    paddr: PAddr,
    vmm_flags: u32,
    arch_mmu_flags: ArchMmuFlags,
) -> Result<(), Status> {
    let specific = if vmm_flags & vmm_flag::VALLOC_SPECIFIC != 0 && !ptr.is_null() {
        // SAFETY: `ptr` is aligned and valid for reads, and `*ptr` is initialized.
        Some(unsafe { *ptr }.addr())
    } else {
        None
    };
    // SAFETY: `name` points to a null-terminated string that is not modified during the call.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    // SAFETY: The cache policy in `arch_mmu_flags` is appropriate for all mappings of
    // `[paddr, paddr + size)` with `size` rounded up to a whole number of pages. Creating a mapping
    // with the specified range and flags is sound.
    let base = unsafe {
        this.alloc_physical(name, size, specific, align_pow2, paddr, vmm_flags, arch_mmu_flags)
    }?;
    if size != 0 && !ptr.is_null() {
        // SAFETY: `ptr` is aligned and valid for writes.
        unsafe { *ptr = ptr::with_exposed_provenance_mut(base) };
    }
    Ok(())
}
