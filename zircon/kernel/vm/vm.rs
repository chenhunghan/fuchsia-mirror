// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_WRITE, ARCH_MMU_FLAG_UNCACHED_DEVICE, ArchMmuFlags,
};
use super::page::VmPagePtr;
use super::page_state::VmPageState;
use super::physmap::{is_physmap_addr, paddr_to_physmap, physmap_to_paddr};
use super::pmm;
use super::vm_address_region::{VmAddressRegion, flag as vmar_flag};
use super::vm_aspace::VmAspace;
use crate::arch_rs::vm::is_kernel_address;
use crate::kernel::types::{PAddr, VAddr};
use boot_options::BootOptions;
use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use core::hint;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use debug::ltrace_entry;
use fbl::RefPtr;
use handoff::{PhysHandoff, PhysMapping, PhysMapping_Permissions, PhysMapping_Type, PhysVmar};
use lazy_init::{LazyInit, NoCheck};
use page_bindings::vm_page_t;

const LOCAL_TRACE: u32 = 0;

// SAFETY: All symbols below are linker-defined boundary addresses provided by `kernel.ld` and
// typed as 1-byte-aligned zero-sized `[c_char; 0]` statics so taking their addresses cannot
// cause alignment or out-of-bounds UB.
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    /// Start of the kernel executable image, provided by the linker script.
    pub static __executable_start: [c_char; 0];
}

// SAFETY: All symbols below are linker-defined boundary addresses provided by `kernel.ld` and
// typed as 1-byte-aligned zero-sized `[c_char; 0]` statics so taking their addresses cannot
// cause alignment or out-of-bounds UB.
#[cfg(ktest)]
#[allow(non_upper_case_globals)]
unsafe extern "C" {
    /// Start of the kernel code segment, provided by the linker script.
    pub static __code_start: [c_char; 0];
    /// End of the kernel code segment, provided by the linker script.
    pub static __code_end: [c_char; 0];
    /// Start of the kernel read-only data segment, provided by the linker script.
    pub static __rodata_start: [c_char; 0];
    /// End of the kernel read-only data segment, provided by the linker script.
    pub static __rodata_end: [c_char; 0];
    /// Start of the kernel RELRO segment, provided by the linker script.
    pub static __relro_start: [c_char; 0];
    /// End of the kernel RELRO segment, provided by the linker script.
    pub static __relro_end: [c_char; 0];
    /// Start of the kernel data segment, provided by the linker script.
    pub static __data_start: [c_char; 0];
    /// End of the kernel image, provided by the linker script.
    pub static _end: [c_char; 0];
}

#[repr(transparent)]
struct ZeroPage(VmPagePtr);

// SAFETY: `ZeroPage` wraps a pointer to the wired zero page in the PMM arena, which is initialized
// once during single-threaded `vm_init` and never freed or mutated afterward.
unsafe impl Sync for ZeroPage {}

/// Boot time allocated page full of zeros.
static ZERO_PAGE: LazyInit<ZeroPage, NoCheck> = LazyInit::uninit();
/// Physical address of the boot time allocated zero page.
static ZERO_PAGE_PADDR: LazyInit<PAddr, NoCheck> = LazyInit::uninit();

#[repr(transparent)]
struct RelocatedBasePtr(*const c_char);

// SAFETY: `RelocatedBasePtr` holds an immutable linker-relocated address (`__executable_start`)
// and is never dereferenced to mutate shared state.
unsafe impl Sync for RelocatedBasePtr {}

/// This exact symbol name is referenced by `scripts/vmzircon-gdb.py`. It will be
/// relocated by physboot before the kernel starts.
#[used]
#[allow(non_upper_case_globals)]
#[unsafe(no_mangle)]
static kernel_relocated_base: RelocatedBasePtr =
    RelocatedBasePtr((&raw const __executable_start).cast::<c_char>());

struct HandoffVmars(#[expect(dead_code)] fbl::Vector<RefPtr<VmAddressRegion>>);

// SAFETY: `RefPtr<VmAddressRegion>` and `DefaultAllocator` are `Sync`, and `HandoffVmars` is
// initialized once during single-threaded `vm_init` and never mutated afterward.
unsafe impl Sync for HandoffVmars {}

/// The initialized VMARs described in the phys hand-off.
// TODO(https://fxbug.dev/42164859): Consider moving these to a stack- or heap-allocated object.
static HANDOFF_VMARS: LazyInit<HandoffVmars, NoCheck> = LazyInit::uninit();

struct TemporaryHandoffVmar(UnsafeCell<Option<RefPtr<VmAddressRegion>>>);

// SAFETY: `TEMPORARY_HANDOFF_VMAR` is written once in `vm_init` and taken once in
// `vm_end_handoff`, which execute sequentially without concurrent access.
unsafe impl Sync for TemporaryHandoffVmar {}

/// Temporary hand-off VMAR initialized in `vm_init` and destroyed in `vm_end_handoff`.
static TEMPORARY_HANDOFF_VMAR: TemporaryHandoffVmar = TemporaryHandoffVmar(UnsafeCell::new(None));

/// Declare storage for the kernel's heap VMAR. Maybe unused if virtual heap is disabled.
pub(super) static KERNEL_HEAP_VMAR: LazyInit<VmAddressRegion, NoCheck> = LazyInit::uninit();

/// Whether the virtual heap is enabled. Initialized once during single-threaded `vm_init_preheap`.
static USING_VIRTUAL_HEAP: LazyInit<bool, NoCheck> = LazyInit::uninit();

fn to_vmar_flags(perms: PhysMapping_Permissions) -> u32 {
    let mut flags = vmar_flag::SPECIFIC | vmar_flag::CAN_MAP_SPECIFIC;
    if perms.readable() {
        flags |= vmar_flag::CAN_MAP_READ;
    }
    if perms.writable() {
        flags |= vmar_flag::CAN_MAP_WRITE;
    }
    if perms.executable() {
        flags |= vmar_flag::CAN_MAP_EXECUTE;
    }
    flags
}

fn to_arch_mmu_flags(perms: PhysMapping_Permissions, type_: PhysMapping_Type) -> ArchMmuFlags {
    let mut flags: ArchMmuFlags = 0;
    match type_ {
        PhysMapping_Type::Normal => {
            flags |= ARCH_MMU_FLAG_CACHED;
        }
        PhysMapping_Type::Mmio => {
            flags |= ARCH_MMU_FLAG_UNCACHED_DEVICE;
        }
    }
    if perms.readable() {
        flags |= ARCH_MMU_FLAG_PERM_READ;
    }
    if perms.writable() {
        flags |= ARCH_MMU_FLAG_PERM_WRITE;
    }
    if perms.executable() {
        flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
    }
    flags
}

/// Reserves each mapping in `mappings` within `vmar` and maps its KASAN shadow when required.
///
/// # Safety
///
/// `vmar` is in the kernel aspace, only the current CPU is online, and each entry in `mappings`
/// has valid kernel MMU permissions and cache type for its range. Nothing may access the range of
/// any entry of `mappings` in a way that its permissions do not permit, no Rust reference into a
/// permissionless range (or mutable reference into a read-only range) may be live, and if
/// splitting a partly covered large page may perform break-before-make, nothing may rely on the
/// rest of that large page staying mapped or hold a live Rust reference into it.
///
/// For KASAN shadow mapping (`#[cfg(sanitize = "address")]`), the caller must ensure this is only
/// called during single-threaded early boot (`vm_init`) with valid `physboot`-initialized kernel
/// `PhysMapping` entries within the KASAN shadow-mapped kernel address space.
unsafe fn register_mappings(mappings: &[PhysMapping], vmar: &VmAddressRegion) {
    for mapping in mappings {
        // SAFETY: Caller upholds `reserve_space`'s preconditions for `vmar`, `mapping`, its
        // permissions and cache type, and any partly covered large page.
        let status = unsafe {
            vmar.reserve_space(
                mapping.name(),
                VAddr(mapping.vaddr),
                mapping.size,
                to_arch_mmu_flags(mapping.perms, mapping.type_),
            )
        };
        assert!(status.is_ok());

        #[cfg(sanitize = "address")]
        if mapping.kasan_shadow {
            // SAFETY: `[mapping.vaddr, mapping.vaddr + mapping.size)` is a valid hand-off kernel
            // virtual mapping just reserved in `vmar` during single-threaded `vm_init`.
            unsafe {
                crate::instrumentation::asan::asan_map_shadow_for(mapping.vaddr, mapping.size);
            }
        }
    }
}

/// Registers a hand-off VMAR and all of its mappings under the kernel root VMAR.
///
/// # Safety
///
/// Must only be called during single-threaded `vm_init` while temporary hand-off memory backing
/// `phys_vmar` and its mappings remains mapped, with a valid `physboot`-initialized `phys_vmar`.
/// Nothing may access the range of any mapping of `phys_vmar` in a way that its permissions do
/// not permit, no Rust reference into a permissionless mapping (or mutable reference into a
/// read-only mapping) may be live, and if splitting a partly covered large page may perform
/// break-before-make, nothing may rely on the rest of that large page staying mapped or hold a
/// live Rust reference into it.
unsafe fn register_vmar(phys_vmar: &PhysVmar) -> RefPtr<VmAddressRegion> {
    let root_vmar = VmAspace::kernel_aspace().root_vmar().expect("kernel root VMAR initialized");

    // SAFETY: Caller guarantees `phys_vmar` is valid and temporary hand-off memory is mapped.
    unsafe { phys_vmar.log(c"VM") };
    // SAFETY: Caller guarantees `phys_vmar` is valid and temporary hand-off memory is mapped.
    let perms = unsafe { phys_vmar.permissions() };
    let vmar = root_vmar
        .create_sub_vmar(
            phys_vmar.base.wrapping_sub(root_vmar.base().0),
            phys_vmar.size,
            0,
            to_vmar_flags(perms),
            phys_vmar.name().to_bytes(),
        )
        .unwrap();
    // SAFETY: Caller guarantees `phys_vmar` is valid and temporary hand-off memory is mapped
    // during single-threaded `vm_init`. `vmar` is in the kernel aspace, only the current CPU is
    // online, and caller upholds the mapping permission, live-reference, and partly-covered
    // large-page requirements for `phys_vmar`.
    unsafe { register_mappings(phys_vmar.mappings(), &vmar) };

    vmar
}

/// Returns the physical address of `va`, a virtual address in the kernel image.
///
/// # Safety
///
/// `va` must be in `[__executable_start, __data_end)`, the physically contiguous part of the image.
/// The bss pages are not necessarily contiguous with it.
pub unsafe fn kernel_physical_address_of(va: usize) -> PAddr {
    PAddr(crate::top::handoff::KernelPhysicalAddressOf(va))
}

/// Whether the virtual heap is enabled.
///
/// Must not be called before `vm_init_preheap` has completed.
#[inline]
pub fn vm_using_virtual_heap() -> bool {
    // SAFETY: `USING_VIRTUAL_HEAP` is initialized unconditionally during single-threaded early boot
    // in `vm_init_preheap` before `heap_init` or any VM/heap operations, and is never mutated
    // afterward.
    unsafe { *USING_VIRTUAL_HEAP.get_unchecked() }
}

/// C++ FFI entry point for [`vm_using_virtual_heap`].
#[unsafe(export_name = "vm_using_virtual_heap")]
extern "C" fn rust_vm_using_virtual_heap() -> bool {
    vm_using_virtual_heap()
}

/// Request the heap dimensions.
///
/// Must not be called before `vm_init_preheap` has completed. Panics if the virtual heap is not
/// enabled.
#[inline]
pub fn vm_get_kernel_heap_base() -> usize {
    assert!(vm_using_virtual_heap());
    // SAFETY: `USING_VIRTUAL_HEAP` is only initialized to `true` in `vm_init_preheap` after
    // `KERNEL_HEAP_VMAR` has been initialized, and `KERNEL_HEAP_VMAR` is never destroyed.
    unsafe { KERNEL_HEAP_VMAR.get_unchecked() }.base().0
}

/// C++ FFI entry point for [`vm_get_kernel_heap_base`].
#[unsafe(export_name = "vm_get_kernel_heap_base")]
extern "C" fn rust_vm_get_kernel_heap_base() -> usize {
    vm_get_kernel_heap_base()
}

/// Request the heap dimensions.
///
/// Must not be called before `vm_init_preheap` has completed. Panics if the virtual heap is not
/// enabled.
#[inline]
pub fn vm_get_kernel_heap_size() -> usize {
    assert!(vm_using_virtual_heap());
    // SAFETY: `USING_VIRTUAL_HEAP` is only initialized to `true` in `vm_init_preheap` after
    // `KERNEL_HEAP_VMAR` has been initialized, and `KERNEL_HEAP_VMAR` is never destroyed.
    unsafe { KERNEL_HEAP_VMAR.get_unchecked() }.size()
}

/// C++ FFI entry point for [`vm_get_kernel_heap_size`].
#[unsafe(export_name = "vm_get_kernel_heap_size")]
extern "C" fn rust_vm_get_kernel_heap_size() -> usize {
    vm_get_kernel_heap_size()
}

/// Initializes basic VM data structures and reserves the kernel virtual heap VMAR prior to heap
/// initialization.
///
/// # Safety
///
/// Must be called at most once on the boot CPU before `heap_init`, `vm_init`, or any other thread
/// runs, after `thread::init_early()`, `ASPACES_LIST` and PMM initialization, and
/// `arch_prevm_init`, and before any other use of the kernel address space, while `gPhysHandoff`
/// is valid.
pub(crate) unsafe fn vm_init_preheap() {
    // Initialize VMM data structures.
    // SAFETY: The caller upholds the requirements of `kernel_aspace_init_pre_heap`.
    unsafe { VmAspace::kernel_aspace_init_pre_heap() };

    let root_vmar = VmAspace::kernel_aspace().root_vmar().expect("kernel root VMAR initialized");

    let vmar: ManuallyDrop<RefPtr<VmAddressRegion>>;

    // SAFETY: The caller guarantees `gPhysHandoff` is valid throughout `vm_init_preheap`.
    let heap_vmar = unsafe { PhysHandoff::get() }.heap_vmar();
    if let Some(heap_vmar) = heap_vmar {
        // SAFETY: Temporary hand-off memory backing `heap_vmar` is mapped throughout
        // `vm_init_preheap`.
        unsafe { heap_vmar.log(c"VM") };
        // Reserve the range for the heap.
        let kernel_heap_base = heap_vmar.base;
        let heap_bytes = heap_vmar.size;
        // The heap has nothing to initialize later and we can create this from the beginning with
        // only read and write and no execute.
        // SAFETY: The region is initialized in the static `KERNEL_HEAP_VMAR`, whose last
        // reference is never released: the reference adopted below is held in `ManuallyDrop` and
        // never released.
        let init = unsafe {
            VmAddressRegion::new_child(
                &root_vmar,
                kernel_heap_base,
                heap_bytes,
                vmar_flag::CAN_MAP_SPECIFIC | vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                b"kernel heap",
            )
        };
        // SAFETY: `KERNEL_HEAP_VMAR` is initialized at most once during single-threaded early boot
        // before any access.
        let Ok(()) = unsafe { Pin::static_ref(&KERNEL_HEAP_VMAR).init_pin(init) };
        // SAFETY: `KERNEL_HEAP_VMAR` was just initialized and has not been adopted yet. The
        // reference is held in `ManuallyDrop` and never released, so `VmAddressRegion::recycle` is
        // never invoked on static storage.
        vmar = ManuallyDrop::new(unsafe { RefPtr::adopt_raw(KERNEL_HEAP_VMAR.as_ptr()) });
        vmar.trace_new();
        {
            ksync::lock!(let mut region_guard = vmar.region_lock().lock());
            ksync::lock!(let mut guard = vmar.lock().lock());
            // SAFETY: `vmar` is in `LifeCycleState::NotReady` with a live parent (`root_vmar`), is
            // kept alive in static `LazyInit` storage (and `vmar`) for the duration of the call,
            // and is the first child in the freshly initialized kernel root VMAR so no child has
            // the same base.
            let status = unsafe { vmar.activate(region_guard.token_mut(), guard.token_mut()) };
            assert!(status.is_ok());
        }
    }
    // SAFETY: `USING_VIRTUAL_HEAP` is initialized at most once during single-threaded early boot
    // in `vm_init_preheap` before any access (and after `KERNEL_HEAP_VMAR` is initialized).
    unsafe { USING_VIRTUAL_HEAP.init(heap_vmar.is_some()) };
}

/// Initializes the VM subsystem, allocating the zero page and registering hand-off VMARs.
///
/// # Safety
///
/// Must be called at most once during single-threaded early boot after `vm_init_preheap` and
/// `heap_init` (prior to enabling break-before-make at `LK_INIT_LEVEL_VM`), while `gPhysHandoff`
/// is a valid `physboot` hand-off whose `PhysVmar` mappings match active page-table permissions
/// with no conflicting live Rust references.
pub(crate) unsafe fn vm_init() {
    ltrace_entry!();

    // grab a page and mark it as the zero page
    let (zero_page, zero_page_paddr) = pmm::alloc_page(0).unwrap();

    // consider the zero page a wired page part of the kernel.
    // SAFETY: `zero_page` was just allocated by `pmm::alloc_page` and is exclusively owned here.
    unsafe { zero_page.set_state(VmPageState::WIRED) };

    let ptr = paddr_to_physmap(zero_page_paddr);
    debug_assert!(ptr.0 != 0);

    // SAFETY: `ptr` is the valid kernel physmap virtual address of the page-aligned `zero_page`
    // exclusively owned here before any concurrent access.
    unsafe { crate::arch_rs::ops::zero_page(ptr) };

    // SAFETY: `ZERO_PAGE` and `ZERO_PAGE_PADDR` are initialized at most once during
    // single-threaded early boot in `vm_init` after the page is wired and zeroed.
    unsafe {
        ZERO_PAGE.init(ZeroPage(zero_page));
        ZERO_PAGE_PADDR.init(zero_page_paddr);
    }

    // Register the permanent and temporary hand-off VMARs.
    // SAFETY: The caller guarantees `gPhysHandoff` is valid throughout `vm_init`.
    let handoff = unsafe { PhysHandoff::get() };
    let mut handoff_vmars = fbl::Vector::new();
    // SAFETY: Temporary hand-off memory is mapped throughout `vm_init`.
    let phys_vmars = unsafe { handoff.vmars() };
    let res = handoff_vmars.reserve(phys_vmars.len());
    assert!(res.is_ok());
    for phys_vmar in phys_vmars {
        // SAFETY: Called during single-threaded `vm_init` prior to enabling break-before-make,
        // while valid `physboot` hand-off mappings are mapped in place with matching
        // `PhysMapping` permissions and no conflicting live Rust references.
        let vmar = unsafe { register_vmar(phys_vmar) };
        let res = handoff_vmars.push_back(vmar);
        assert!(res.is_ok());
    }
    // SAFETY: `HANDOFF_VMARS` is initialized at most once during single-threaded early boot in
    // `vm_init` and is never accessed concurrently.
    unsafe { HANDOFF_VMARS.init(HandoffVmars(handoff_vmars)) };

    // SAFETY: Called during single-threaded `vm_init` prior to enabling break-before-make, while
    // valid `physboot` temporary hand-off memory is mapped read-write with matching `PhysMapping`
    // permissions and no conflicting live Rust references.
    let temporary_vmar = unsafe { register_vmar(handoff.temporary_vmar()) };
    // SAFETY: `vm_init` runs once during single-threaded early boot before `vm_end_handoff`, so no
    // concurrent access to `TEMPORARY_HANDOFF_VMAR` can occur.
    unsafe { *TEMPORARY_HANDOFF_VMAR.0.get() = Some(temporary_vmar) };

    super::heap::cmpct_set_fill_on_alloc_threshold(
        BootOptions::get().heap_alloc_fill_threshold as usize,
    );
}

/// Ends the VM's role within the context of phys handoff: it destroys the VMAR
/// containing the mappings backing temporary hand-off data.
///
/// # Safety
///
/// Must be called at most once sequentially after `vm_init` (with no concurrent access), when
/// ending hand-off from physboot.
pub(crate) unsafe fn vm_end_handoff() {
    // SAFETY: The caller guarantees `vm_end_handoff` is called at most once sequentially after
    // `vm_init` with no concurrent access to `TEMPORARY_HANDOFF_VMAR`.
    let temporary_handoff_vmar = unsafe { (*TEMPORARY_HANDOFF_VMAR.0.get()).take() };
    debug_assert!(temporary_handoff_vmar.is_some());
    drop(temporary_handoff_vmar);
}

/// Virtual to physical for kernel addresses.
#[unsafe(no_mangle)]
pub extern "C" fn vaddr_to_paddr(va: *const c_void) -> PAddr {
    if is_physmap_addr(VAddr(va.addr())) {
        return physmap_to_paddr(VAddr(va.addr()));
    }

    // It doesn't make sense to be calling this on a non-kernel address, since we would otherwise
    // be querying some 'random' active user address space, which is unlikely to be what the caller
    // wants.
    if !is_kernel_address(va.addr()) {
        hint::cold_path();
        return PAddr(0);
    }

    let Ok((pa, _)) = VmAspace::kernel_aspace().arch_aspace().query(va.addr()) else {
        hint::cold_path();
        return PAddr(0);
    };

    pa
}

/// Return a pointer to the zero page.
///
/// Must not be called before `vm_init` has completed.
#[inline]
pub fn vm_get_zero_page() -> VmPagePtr {
    // SAFETY: `ZERO_PAGE` is initialized unconditionally during single-threaded early boot in
    // `vm_init` before any VM objects or mappings can reference the zero page, and is never
    // mutated afterward.
    unsafe { ZERO_PAGE.get_unchecked().0 }
}

/// C++ FFI entry point for [`vm_get_zero_page`].
#[unsafe(export_name = "vm_get_zero_page")]
extern "C" fn rust_vm_get_zero_page() -> *mut vm_page_t {
    vm_get_zero_page().as_ffi()
}

/// Return the physical address of the zero page.
///
/// Must not be called before `vm_init` has completed.
#[inline]
pub fn vm_get_zero_page_paddr() -> PAddr {
    // SAFETY: `ZERO_PAGE_PADDR` is initialized unconditionally during single-threaded early boot
    // in `vm_init` before any VM objects or mappings can reference the zero page, and is never
    // mutated afterward.
    unsafe { *ZERO_PAGE_PADDR.get_unchecked() }
}

/// Evaluates whether VM ktracing is enabled at the requested level.
#[macro_export]
macro_rules! vm_ktrace_level_enabled {
    (1) => {
        cfg!(vm_tracing_level_at_least_1)
    };
    (2) => {
        cfg!(vm_tracing_level_at_least_2)
    };
    (3) => {
        cfg!(vm_tracing_level_at_least_3)
    };
}
pub use vm_ktrace_level_enabled;

/// Creates a scoped VM ktrace duration if tracing is enabled at `$level`.
#[macro_export]
macro_rules! vm_ktrace_duration {
    (1, $($rest:tt)*) => {
        $crate::vm::vm::vm_ktrace_duration_if!(vm_tracing_level_at_least_1, $($rest)*);
    };
    (2, $($rest:tt)*) => {
        $crate::vm::vm::vm_ktrace_duration_if!(vm_tracing_level_at_least_2, $($rest)*);
    };
    (3, $($rest:tt)*) => {
        $crate::vm::vm::vm_ktrace_duration_if!(vm_tracing_level_at_least_3, $($rest)*);
    };
}
pub use vm_ktrace_duration;

/// Implements `vm_ktrace_duration!` for the level whose cfg is `$cfg`.
#[doc(hidden)]
#[macro_export]
macro_rules! vm_ktrace_duration_if {
    ($cfg:ident, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        #[cfg($cfg)]
        let _duration =
            $crate::ktrace_rs::begin_scope_cond!(true, "kernel:vm", $label $(, $key => $val)*);
        #[cfg(not($cfg))]
        if false {
            // Type-checks the arguments without evaluating them.
            let _: $crate::ktrace_rs::StringRef<'_> = ($label).into();
            $(
                let _: $crate::ktrace_rs::StringRef<'_> = ($key).into();
                let _: $crate::ktrace_rs::ArgValue<'_> = ($val).into();
            )*
        }
    };
}
pub use vm_ktrace_duration_if;

#[macro_export]
macro_rules! vm_ktrace_duration_begin {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::duration_begin!("kernel:vm", $label $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_duration_begin;

#[macro_export]
macro_rules! vm_ktrace_duration_end {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::duration_end!("kernel:vm", $label $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_duration_end;

/// Emits a VM ktrace flow begin event if tracing is enabled at `$level`.
#[macro_export]
macro_rules! vm_ktrace_flow_begin {
    ($level:tt, $label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::flow_begin!("kernel:vm", $label, $flow_id $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_flow_begin;

/// Emits a VM ktrace flow end event if tracing is enabled at `$level`.
#[macro_export]
macro_rules! vm_ktrace_flow_end {
    ($level:tt, $label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if $crate::vm::vm::vm_ktrace_level_enabled!($level) {
            $crate::ktrace_rs::flow_end!("kernel:vm", $label, $flow_id $(, $key => $val)*);
        }
    };
}
pub use vm_ktrace_flow_end;

// In builds with verbose VM tracing enabled (VM_TRACING_LEVEL >= 1), we emit these events under
// the "kernel:vm" category at the specified VM tracing level. In standard/production builds
// (VM_TRACING_LEVEL = 0), we fallback to emitting under the lightweight "kernel:oom" category
// instead.
//
// The general expectation is that users will enable "kernel:oom" in cases where "kernel:vm" is
// too expensive. Using a compile-time selection here prevents duplicate overlapping slices in
// the trace visualizer when both categories are enabled at runtime, while still providing a
// single category for developers opting for more verbose VM tracing.
#[cfg(vm_tracing_level_at_least_1)]
pub use vm_ktrace_duration as oom_ktrace_duration;
#[cfg(not(vm_tracing_level_at_least_1))]
#[macro_export]
macro_rules! oom_ktrace_duration {
    ($level:tt, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        let _duration = $crate::ktrace_rs::begin_scope!("kernel:oom", $label $(, $key => $val)*);
    };
}
#[cfg(not(vm_tracing_level_at_least_1))]
pub use oom_ktrace_duration;

/// Kernel console `vm` command implementation.
#[cfg(console_enabled)]
mod console {
    use crate::arch_rs::vm::is_kernel_address;
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::types::PAddr;
    use crate::vm::arch_vm_aspace::{ARCH_UNMAP_OPTION_NONE, ArchMmuFlags};
    use crate::vm::physmap::{is_physmap_phys_addr, paddr_to_physmap};
    use crate::vm::vm_aspace::VmAspace;
    use core::ffi::{c_int, c_void};
    use kprint::kprintln;
    use zx_status::Status;

    /// Console command handler for `k vm`.
    ///
    /// # Safety
    ///
    /// - `argc` must be `>= 1` and `argv` must be non-null and valid for reads of `argc`
    ///   initialized `CmdArgs` entries for the duration of the call.
    /// - For the `map` and `unmap` subcommands, the caller-supplied physical and kernel virtual
    ///   address ranges must be valid to map or unmap in the kernel address space without
    ///   corrupting active kernel mappings.
    unsafe extern "C" fn cmd_vm(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INTERNAL.into_raw();
        }
        // SAFETY: `argv` is non-null and points to `argc` initialized `CmdArgs` entries from the
        // console dispatcher.
        let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let cmd_name = args[0].as_str();
        let usage = || -> c_int {
            kprintln!("usage:");
            kprintln!("{:s} phys2virt <address>", cmd_name);
            kprintln!("{:s} virt2phys <address>", cmd_name);
            kprintln!("{:s} map <phys> <virt> <count> <flags>", cmd_name);
            kprintln!("{:s} unmap <virt> <count>", cmd_name);
            Status::INTERNAL.into_raw()
        };
        let not_enough_args = || -> c_int {
            kprintln!("not enough arguments");
            usage()
        };

        if args.len() < 2 {
            return not_enough_args();
        }

        let subcmd = args[1].as_str();
        if subcmd == "phys2virt" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if !is_physmap_phys_addr(PAddr(args[2].arg_uint as usize)) {
                kprintln!("address isn't in physmap");
                return -1;
            }

            let ptr = core::ptr::with_exposed_provenance::<c_void>(
                paddr_to_physmap(PAddr(args[2].arg_uint as usize)).0,
            );
            kprintln!("paddr_to_physmap returns {:p}", ptr);
        } else if subcmd == "virt2phys" {
            if args.len() < 3 {
                return not_enough_args();
            }

            if !is_kernel_address(args[2].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            match VmAspace::kernel_aspace().arch_aspace().query(args[2].arg_uint as usize) {
                Ok((pa, mmu_flags)) => {
                    kprintln!("arch_mmu_query returns {}", zx_status::sys::ZX_OK);
                    kprintln!("\tpa {:#x}, flags {:#x}", pa.0, mmu_flags);
                }
                Err(s) => {
                    kprintln!("arch_mmu_query returns {}", s.into_raw());
                }
            }
        } else if subcmd == "map" {
            if args.len() < 6 {
                return not_enough_args();
            }

            if !is_kernel_address(args[3].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            // SAFETY: Console debugging command mapping user-specified physical pages into
            // kernel virtual address space.
            let err = Status::result_into_raw(unsafe {
                VmAspace::kernel_aspace().arch_aspace().map_contiguous(
                    args[3].arg_uint as usize,
                    PAddr(args[2].arg_uint as usize),
                    args[4].arg_uint as u32 as usize,
                    args[5].arg_uint as ArchMmuFlags,
                )
            });
            kprintln!("arch_mmu_map returns {}", err);
        } else if subcmd == "unmap" {
            if args.len() < 4 {
                return not_enough_args();
            }

            if !is_kernel_address(args[2].arg_uint as usize) {
                kprintln!("ERROR: outside of kernel address space");
                return -1;
            }

            // Strictly only attempt to unmap exactly what the user requested, they can deal with
            // any failure that might result.
            // SAFETY: Console debugging command unmapping user-specified kernel virtual
            // address range.
            let err = Status::result_into_raw(unsafe {
                VmAspace::kernel_aspace().arch_aspace().unmap(
                    args[2].arg_uint as usize,
                    args[3].arg_uint as u32 as usize,
                    ARCH_UNMAP_OPTION_NONE,
                )
            });
            kprintln!("arch_mmu_unmap returns {}", err);
        } else {
            kprintln!("unknown command");
            return usage();
        }

        zx_status::sys::ZX_OK
    }

    static_command!(VM, c"vm".as_ptr(), c"vm commands".as_ptr(), cmd_vm, CMD_AVAIL_NORMAL);
}
