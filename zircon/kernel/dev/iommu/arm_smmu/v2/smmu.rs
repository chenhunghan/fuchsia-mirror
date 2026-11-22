// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 controller implementation.

#[cfg(is_kernel)]
use core::convert::Infallible;
use core::ffi::c_void;
use core::fmt::Write as _;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use debug::dprintf;
use fbl::{
    Array, DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    Recyclable, RefPtr, TrackingSize, pin_make_ref_counted,
};
#[cfg(is_kernel)]
use ksync::KMutexGuard;
use ksync::{KCell, KMutex, LockToken};
#[cfg(is_kernel)]
use pin_init::PinInit;
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;

#[cfg(ktest)]
use crate::arm_smmu::common::PAGE_SIZE_USIZE;
#[allow(unused_imports)]
pub(crate) use crate::arm_smmu::common::irq::{
    IrqDef, ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_HIGH, ZBI_ARM_SMMU_IRQ_FLAGS_ACTIVE_LOW,
    ZBI_ARM_SMMU_IRQ_FLAGS_FALLING_EDGE, ZBI_ARM_SMMU_IRQ_FLAGS_RISING_EDGE, ZbiDcfgArmSmmuIrq,
};
use crate::arm_smmu::common::{
    ArmSmmuMode, DRIVER_OOPS_BANNER, EscapedName, SmmuEnv, SmmuNoIrqSavePolicy, SmmuRawMutex,
    SmmuRawSpinLock, ZX_RSRC_KIND_MMIO,
};
use crate::arm_smmu::v2::context_bank::ContextBank;
use crate::arm_smmu::v2::registers::SmmuMmio;
use crate::arm_smmu::v2::registers::gr0::{
    ClientPd, Cr0, Cr2, ExidEnable, Gfar, Gfsr, Gfsynr0, Gfsynr1, Idr0, Idr1, Idr2, Idr7, S2cr,
    S2crTranslation, S2crType, Smr, Stalld, Tlbgstatus, Tlbgsync, Tlbiallnsnh,
};
use crate::arm_smmu::v2::registers::gr1::{Cbar, Cbfrsynra};
use crate::arm_smmu::v2::registers::s1cbr::{Far, Fsr, Fsynr0, Sctlr};
use crate::arm_smmu::v2::smmu_bti::{SmmuV2Bti, SmmuV2BtiLockGuard};
use crate::arm_smmu::v2::stream_match_reg::StreamMatchRegGroup;
use crate::arm_smmu::v2::utils::{
    Bitmask128, BtiMode, SmmuParams, SmrValue, arm_cbar_type_to_string, arm_s2cr_type_to_string,
};

/// Maximum number of IRQ descriptors in [`ZbiDcfgArmSmmuV2Driver::irqs`].
pub const ZBI_ARM_SMMU_MAX_IRQS: usize = 128;

/// Maximum number of handoff SMR entries in [`ZbiDcfgArmSmmuV2Driver::handoff_smrs`].
pub const ZBI_ARM_SMMU_MAX_HANDOFF_SMRS: usize = 16;

/// ZBI driver configuration payload for an ARM SMMUv2 instance.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ZbiDcfgArmSmmuV2Driver {
    /// Base physical address of the SMMU address space (Global Register Space 0).
    pub mmio_phys: u64,
    /// Number of context banks that a driver should limit itself to using (`0` for no override).
    pub num_context_banks_override: u32,
    /// Number of stream match registers that a driver should limit itself to using (`0` for no
    /// override).
    pub num_smr_override: u32,
    /// Total number of valid entries in `irqs`.
    pub irq_cnt: u32,
    /// Number of global fault interrupt entries at the start of `irqs`.
    pub global_irq_cnt: u32,
    /// Interrupt descriptors (`[0, global_irq_cnt)` global, `[global_irq_cnt, irq_cnt)` context).
    pub irqs: [ZbiDcfgArmSmmuIrq; ZBI_ARM_SMMU_MAX_IRQS],
    /// Number of valid entries in `handoff_smrs`.
    pub handoff_smr_cnt: u32,
    /// Stream ID ranges (`(MASK << 16) | ID`) handed off from the bootloader.
    pub handoff_smrs: [u32; ZBI_ARM_SMMU_MAX_HANDOFF_SMRS],
}

zr::static_assert!(core::mem::size_of::<ZbiDcfgArmSmmuV2Driver>() == 1120);
zr::static_assert!(core::mem::align_of::<ZbiDcfgArmSmmuV2Driver>() == 8);

impl Default for ZbiDcfgArmSmmuV2Driver {
    fn default() -> Self {
        Self {
            mmio_phys: 0,
            num_context_banks_override: 0,
            num_smr_override: 0,
            irq_cnt: 0,
            global_irq_cnt: 0,
            irqs: [ZbiDcfgArmSmmuIrq::default(); ZBI_ARM_SMMU_MAX_IRQS],
            handoff_smr_cnt: 0,
            handoff_smrs: [0; ZBI_ARM_SMMU_MAX_HANDOFF_SMRS],
        }
    }
}

/// IOMMU descriptor for fetching an ARM SMMUv2 instance via `zx_iommu_create`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ZxIommuDescArmSmmu {
    /// Physical base address of the SMMU instance to bind.
    pub register_base: u64,
}

zr::static_assert!(core::mem::size_of::<ZxIommuDescArmSmmu>() == 8);
zr::static_assert!(core::mem::align_of::<ZxIommuDescArmSmmu>() == 8);

// Kernel mutexes cannot be constructed in a `const` context, so in the kernel the singleton's
// mutex is constructed by an lk init hook.
#[cfg(is_kernel)]
#[ksync::guarded]
pub(crate) struct SmmuV2InstanceMutex {
    #[mutex]
    pub(crate) lock: ksync::KMutex<SmmuRawMutex>,
}

#[cfg(is_kernel)]
pub(crate) type SmmuV2InstanceLock = SmmuV2InstanceMutexLockClass;

#[cfg(is_kernel)]
static SMMU_V2_INSTANCE_LOCK: lazy_init::LazyInit<SmmuV2InstanceMutex, lazy_init::NoCheck> =
    lazy_init::LazyInit::uninit();

#[cfg(is_kernel)]
impl SmmuV2InstanceLock {
    #[inline]
    pub(crate) fn get() -> &'static ksync::KMutex<Self, SmmuRawMutex> {
        // SAFETY: `SMMU_V2_INSTANCE_LOCK` is initialized at `LK_INIT_LEVEL_EARLIEST` before
        // `SmmuV2InstanceLock` can be accessed.
        &unsafe { SMMU_V2_INSTANCE_LOCK.get_unchecked() }.lock
    }

    #[inline]
    pub(crate) fn lock() -> impl PinInit<KMutexGuard<'static, Self, SmmuRawMutex>, Infallible> {
        Self::get().lock()
    }
}

#[cfg(is_kernel)]
fn smmu_v2_instance_lock_init(_level: init::LkInitLevel) {
    // SAFETY: lk init hooks run once during single-threaded early boot before `SmmuV2InstanceLock`
    // can be accessed.
    unsafe {
        let Ok(()) = Pin::static_ref(&SMMU_V2_INSTANCE_LOCK)
            .init_pin(pin_init::pin_init!(SmmuV2InstanceMutex { lock <- ksync::KMutex::init() }));
    }
}
#[cfg(is_kernel)]
init::lk_init_hook!(
    smmu_v2_instance_lock,
    smmu_v2_instance_lock_init,
    init::LK_INIT_LEVEL_EARLIEST
);

pub(crate) struct GlobalInstanceList {
    initialized: core::cell::UnsafeCell<bool>,
    inner: core::cell::UnsafeCell<
        core::mem::MaybeUninit<DoublyLinkedList<RefPtr<SmmuV2>, DefaultObjectTag, TrackingSize>>,
    >,
}

// SAFETY: Access to `GlobalInstanceList` is synchronized via `SmmuV2InstanceLock`.
unsafe impl Sync for GlobalInstanceList {}

impl GlobalInstanceList {
    const fn new() -> Self {
        Self {
            initialized: core::cell::UnsafeCell::new(false),
            inner: core::cell::UnsafeCell::new(core::mem::MaybeUninit::uninit()),
        }
    }

    pub(crate) fn get_mut<'a>(
        &'a self,
        _token: &'a mut LockToken<'_, SmmuV2InstanceLock>,
    ) -> Pin<&'a mut DoublyLinkedList<RefPtr<SmmuV2>, DefaultObjectTag, TrackingSize>> {
        // SAFETY: Synchronized by the mutable `LockToken` for `SmmuV2InstanceLock`, which stays
        // borrowed for as long as the returned reference is live, so that two calls cannot hand
        // out two live `&mut` references to the list.
        unsafe {
            let init = &mut *self.initialized.get();
            let slot = &mut *self.inner.get();
            if !*init {
                let Ok(()) =
                    pin_init::PinInit::__pinned_init(DoublyLinkedList::new(), slot.as_mut_ptr());
                *init = true;
            }
            Pin::new_unchecked(slot.assume_init_mut())
        }
    }
}

pub(crate) static INSTANCES: GlobalInstanceList = GlobalInstanceList::new();

// Lock protecting `FAULT_BUFFER`. This is a spinlock (acquired at hard IRQ time, with a BTI's
// spinlock potentially held) in the kernel, and a plain mutex in host tests.
ksync::declare_singleton_lock!(FaultBufferLock, SmmuRawSpinLock);

/// Size in bytes of the global buffer used to render context fault reports.
const FAULT_BUFFER_LEN: usize = 1024;

/// Global buffer used to render a context fault report before printing it in one piece.
struct FaultBuffer {
    buf: core::cell::UnsafeCell<[u8; FAULT_BUFFER_LEN]>,
}

// SAFETY: Access to `FaultBuffer` is synchronized via `FaultBufferLock`.
unsafe impl Sync for FaultBuffer {}

impl FaultBuffer {
    const fn new() -> Self {
        Self { buf: core::cell::UnsafeCell::new([0u8; FAULT_BUFFER_LEN]) }
    }

    fn get_mut<'a>(
        &'a self,
        _token: &'a mut LockToken<'_, FaultBufferLock>,
    ) -> &'a mut [u8; FAULT_BUFFER_LEN] {
        // SAFETY: Synchronized by the mutable `LockToken` for `FaultBufferLock`, which is borrowed
        // for as long as the returned reference is live.
        unsafe { &mut *self.buf.get() }
    }
}

static FAULT_BUFFER: FaultBuffer = FaultBuffer::new();

/// `core::fmt::Write` adapter which renders into a fixed byte buffer, silently truncating (on a
/// character boundary) once the buffer is full.
struct FaultReportWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> FaultReportWriter<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    fn as_str(&self) -> &str {
        // SAFETY: `write_str` only ever appends whole UTF-8 characters to `self.buf[..self.len]`.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl core::fmt::Write for FaultReportWriter<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let avail = self.buf.len() - self.len;
        let mut to_copy = s.len().min(avail);
        // Never split a multi-byte character when truncating.
        while !s.is_char_boundary(to_copy) {
            to_copy -= 1;
        }
        self.buf[self.len..self.len + to_copy].copy_from_slice(&s.as_bytes()[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

/// The cookie registered with the interrupt controller for one global or context interrupt
/// vector: which SMMU the vector belongs to, and its index in that SMMU's vector table.
///
/// The interrupt thunks read these two members without holding `irq_lock`, which is sound
/// because they never change after initialization. They are kept here and not in the vector
/// structures (`GlobalIrqVector` / `ContextIrqVector`) because the vector tables are guarded by
/// `irq_lock` and are accessed through `&mut` references to the whole table, which an unlocked
/// read of one of their elements would alias.
#[derive(Default)]
pub(crate) struct IrqCookie {
    smmu: *const SmmuV2,
    ndx: u32,
}

/// Global fault interrupt vector state.
#[derive(Default)]
pub(crate) struct GlobalIrqVector {
    pub(crate) irq_def: IrqDef,
    pub(crate) registered: bool,
}

/// Context bank fault interrupt vector state.
#[derive(Default)]
pub(crate) struct ContextIrqVector {
    pub(crate) irq_def: IrqDef,
    pub(crate) enabled: bool,
    pub(crate) registered: bool,
    pub(crate) in_flight: bool,
    pub(crate) bti: Option<RefPtr<SmmuV2Bti>>,
}

/// Init-time immutable hardware configuration cached during `SmmuV2::init`.
#[derive(Debug, Default)]
pub(crate) struct SmmuInitState {
    /// Stashed debug name formatted during initialization.
    pub(crate) name_buf: [u8; 32],
    pub(crate) name_len: usize,
    /// Operating mode determined during init from boot options and hardware capabilities.
    pub(crate) op_mode: ArmSmmuMode,
    /// Cached copies of read-only ID registers fetched during initialization.
    pub(crate) idr0: Idr0,
    pub(crate) idr1: Idr1,
    pub(crate) idr2: Idr2,
    pub(crate) idr7: Idr7,
    /// Mask of valid Stream ID bits (`((1 << NUMSIDB) - 1) | (((1 << NUMSIDB) - 1) << 16)`).
    pub(crate) valid_stream_id_mask: u32,
    /// Number of context banks and stream match register groups actually used by this driver.
    /// `num_cbs` uses an atomic so it can be reduced to `context_irq_cnt` after IRQ registration
    /// and `lockdown` without creating an exclusive `&mut SmmuInitState` reference while IRQ
    /// handlers are registered.
    pub(crate) num_cbs: AtomicU32,
    pub(crate) num_smrgs: u32,
    /// Virtual address bases of the Global 0, Global 1, and Context Bank 0 register spaces.
    pub(in crate::arm_smmu) gr0_base: SmmuMmio,
    pub(in crate::arm_smmu) gr1_base: SmmuMmio,
    pub(in crate::arm_smmu) cb0_base: SmmuMmio,
    /// Owned fake MMIO register buffer backing `gr0_base`, `gr1_base`, and `cb0_base` for
    /// unregistered unit-test `Smmu` instances.
    #[cfg(ktest)]
    pub(super) unit_test_regs: Option<core::ptr::NonNull<[u64]>>,
}

/// An implementation of an `Iommu` based on the ARM SMMUv2 specification:
/// "ARM System Memory Management Unit Architecture Specification - SMMU architecture version 2.0"
/// Document ID: ARM IHI 0062D.c (ID070417).
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct SmmuV2 {
    #[pin]
    pub(crate) list_node: DoublyLinkedListNode<SmmuV2>,
    phys_reg_base: u64,
    init_state: core::cell::UnsafeCell<SmmuInitState>,
    /// The cookies registered with the interrupt controller, one per entry of `global_irqs` and
    /// `context_irqs`. Like `init_state`, these are written once in `init()`, before any handler
    /// is registered, and are immutable from then on. See `IrqCookie`.
    global_irq_cookies: core::cell::UnsafeCell<Array<IrqCookie>>,
    context_irq_cookies: core::cell::UnsafeCell<Array<IrqCookie>>,

    #[mutex]
    pub(crate) lock: KMutex<SmmuRawMutex>,
    /// Set while user mode holds this SMMU, given to it by `zx_iommu_create` (`fetch`); a second
    /// request meanwhile fails with `ALREADY_BOUND`.
    ///
    /// Cleared when the IOMMU dispatcher user mode held it through is destroyed, which is when
    /// its last handle is closed (or when `zx_iommu_create` fails after `fetch`):
    /// `unbind_user_mode`. So a component which held the handle and restarted can bind the SMMU
    /// again. BTIs made meanwhile are not undone by that; they hold this object themselves.
    #[guarded_by(lock)]
    pub(crate) user_mode_bound: bool,
    #[pin]
    #[guarded_by(lock)]
    pub(crate) bti_list: DoublyLinkedList<RefPtr<SmmuV2Bti>, DefaultObjectTag, TrackingSize>,
    /// Each SMMU may have up to 128 either stream match register groups, or
    /// context banks, for us to allocate and associate with BTIs. Keep a 128-bit
    /// bitmask for each resource type which tracks which registers/context banks
    /// are still available for allocation (1 means available, 0 means currently in
    /// use).
    #[guarded_by(lock)]
    pub(crate) available_smrgs: Bitmask128,
    #[guarded_by(lock)]
    pub(crate) available_cbs: Bitmask128,

    #[mutex]
    pub(crate) irq_lock: KMutex<SmmuRawSpinLock>,
    #[guarded_by(irq_lock)]
    pub(crate) global_irqs: Array<GlobalIrqVector>,
    #[guarded_by(irq_lock)]
    pub(crate) context_irqs: Array<ContextIrqVector>,
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for SmmuV2 {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: Mutable fields of `SmmuV2` are either initialized once in `init()` before publication or
// synchronized by `lock`, `irq_lock`, or `SmmuV2InstanceLock`.
unsafe impl Send for SmmuV2 {}
// SAFETY: Shared access to `SmmuV2` synchronizes mutable state via `lock`, `irq_lock`, or
// `SmmuV2InstanceLock`.
unsafe impl Sync for SmmuV2 {}

#[pinned_drop]
impl PinnedDrop for SmmuV2 {
    fn drop(mut self: Pin<&mut Self>) {
        // By the time that we destruct, all of our interrupts should be unregistered.
        // Assert this.
        {
            let this = self.as_ref().get_ref();
            ksync::lock!(let irq_guard = this.lock_irq_lock());
            for vec in irq_guard.global_irqs().iter() {
                debug_assert!(!vec.registered);
            }
            for vec in irq_guard.context_irqs().iter() {
                debug_assert!(!vec.registered);
            }
        }
        {
            let this = self.as_ref().get_ref();
            ksync::lock!(let guard = this.lock_lock());
            debug_assert!(guard.bti_list().is_empty());
        }
        // SAFETY: `bti_list` was verified empty above, and we are in `PinnedDrop::drop`.
        unsafe {
            let mut_ref = self.as_mut().get_unchecked_mut();
            Pin::new_unchecked(mut_ref.bti_list.get_inner_mut()).clear();
            #[cfg(ktest)]
            if let Some(raw_regs) = mut_ref.init_state.get_mut().unit_test_regs.take() {
                // SAFETY: `raw_regs` was allocated via `Array::<u64>::into_raw` in
                // `create_for_unit_test`, and `self` is in `PinnedDrop::drop` with `bti_list`
                // empty, so no `SmmuMmio` handle into `raw_regs` remains live.
                drop(Array::from_raw(raw_regs.as_ptr()));
            }
        }
    }
}

extern "C" fn global_irq_thunk(cookie: *mut c_void) {
    // SAFETY: `cookie` was registered in `register_global_irqs` pointing to an `IrqCookie` in
    // `SmmuV2::global_irq_cookies`, which is never written after `init()` and which lives, along
    // with the `SmmuV2` it points to, until after `unregister_interrupts` completes.
    unsafe {
        let cookie = &*(cookie as *const IrqCookie);
        (*cookie.smmu).handle_global_irq(cookie.ndx);
    }
}

extern "C" fn context_irq_thunk(cookie: *mut c_void) {
    // SAFETY: `cookie` was registered in `register_context_irqs` pointing to an `IrqCookie` in
    // `SmmuV2::context_irq_cookies`, which is never written after `init()` and which lives, along
    // with the `SmmuV2` it points to, until after `unregister_interrupts` completes.
    unsafe {
        let cookie = &*(cookie as *const IrqCookie);
        (*cookie.smmu).handle_context_irq(cookie.ndx);
    }
}

trait IrqVectorEntry: Sized {
    const KIND: &'static str;
    const THUNK: extern "C" fn(*mut c_void);

    fn irq_def(&self) -> IrqDef;
    fn registered(&self) -> bool;
    fn set_registered(&mut self, registered: bool);
    fn cookies(smmu: &SmmuV2) -> &core::cell::UnsafeCell<Array<IrqCookie>>;
    fn slice<'b>(guard: &'b SmmuV2IrqLockGuard<'_>) -> &'b [Self];
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuV2IrqLockGuard<'_>>) -> &'b mut [Self];
}

impl IrqVectorEntry for GlobalIrqVector {
    const KIND: &'static str = "Global";
    const THUNK: extern "C" fn(*mut c_void) = global_irq_thunk;

    #[inline]
    fn irq_def(&self) -> IrqDef {
        self.irq_def
    }

    #[inline]
    fn registered(&self) -> bool {
        self.registered
    }

    #[inline]
    fn set_registered(&mut self, registered: bool) {
        self.registered = registered;
    }

    #[inline]
    fn cookies(smmu: &SmmuV2) -> &core::cell::UnsafeCell<Array<IrqCookie>> {
        &smmu.global_irq_cookies
    }

    #[inline]
    fn slice<'b>(guard: &'b SmmuV2IrqLockGuard<'_>) -> &'b [Self] {
        guard.global_irqs()
    }

    #[inline]
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuV2IrqLockGuard<'_>>) -> &'b mut [Self] {
        guard.global_irqs_mut()
    }
}

impl IrqVectorEntry for ContextIrqVector {
    const KIND: &'static str = "Context";
    const THUNK: extern "C" fn(*mut c_void) = context_irq_thunk;

    #[inline]
    fn irq_def(&self) -> IrqDef {
        self.irq_def
    }

    #[inline]
    fn registered(&self) -> bool {
        self.registered
    }

    #[inline]
    fn set_registered(&mut self, registered: bool) {
        self.registered = registered;
    }

    #[inline]
    fn cookies(smmu: &SmmuV2) -> &core::cell::UnsafeCell<Array<IrqCookie>> {
        &smmu.context_irq_cookies
    }

    #[inline]
    fn slice<'b>(guard: &'b SmmuV2IrqLockGuard<'_>) -> &'b [Self] {
        guard.context_irqs()
    }

    #[inline]
    fn slice_mut<'b>(guard: Pin<&'b mut SmmuV2IrqLockGuard<'_>>) -> &'b mut [Self] {
        guard.context_irqs_mut()
    }
}

impl SmmuV2 {
    fn format_smmu_name(phys_reg_base: u64) -> ([u8; 32], usize) {
        let mut buf = [0u8; 32];
        let prefix = b"SMMU@0x";
        buf[..prefix.len()].copy_from_slice(prefix);
        let mut pos = prefix.len();
        const HEX: &[u8; 16] = b"0123456789abcdef";

        // Format "SMMU@0x<phys_reg_base>" with at least 8 lowercase hex digits, or
        // more if `phys_reg_base > 0xffff_ffff`.
        let top_nibble = if phys_reg_base > 0xffff_ffff {
            (64 - phys_reg_base.leading_zeros()).div_ceil(4) as usize
        } else {
            8usize
        };
        for i in (0..top_nibble).rev() {
            let nib = ((phys_reg_base >> (i * 4)) & 0xf) as usize;
            buf[pos] = HEX[nib];
            pos += 1;
        }
        (buf, pos)
    }

    #[inline]
    pub(crate) fn init_state(&self) -> &SmmuInitState {
        // SAFETY: Non-atomic fields of `init_state` are either initialized before `self` is
        // allocated or mutated inside `init()` while holding `self.lock` before any interrupt
        // handlers are registered or `self` is published to `INSTANCES`. Post-registration updates
        // to `num_cbs` use atomic operations.
        unsafe { &*self.init_state.get() }
    }

    /// Returns the human-readable instance name (e.g. `"SMMU@0x09000000"`).
    #[inline]
    pub fn name(&self) -> &str {
        let st = self.init_state();
        // SAFETY: `name_buf[..name_len]` is initialized with ASCII characters in
        // `format_smmu_name`.
        unsafe { core::str::from_utf8_unchecked(&st.name_buf[..st.name_len]) }
    }

    /// Returns the global operating mode of this SMMU instance.
    #[inline]
    pub fn op_mode(&self) -> ArmSmmuMode {
        self.init_state().op_mode
    }

    /// Returns the valid Stream ID mask (`(mask << 16) | id`) for this SMMU instance.
    #[inline]
    pub(crate) fn valid_stream_id_mask(&self) -> u32 {
        self.init_state().valid_stream_id_mask
    }

    /// Returns the size in bytes of each register page (`4096` or `65536`).
    #[inline]
    pub(crate) fn reg_page_size(&self) -> usize {
        if self.init_state().idr1.pagesize() != 0 { 1usize << 16 } else { 1usize << 12 }
    }

    /// Returns the number of register pages (`1 << (IDR1.NUMPAGENDXB + 1)`).
    ///
    /// See Section 9.6.1 : SMMU_IDR0 and SMMU_IDR1 for more details, in addition to Section 8.1.1.
    ///
    /// Please note that the `IDR1.NUMPAGENDXB` comment defining `reg_page_cnt` appears to be
    /// mis-formatted. It says that the number of pages is `2(NUMPAGENDXB + 1)`, but fails to place
    /// the parenthetical in a superscript, or use some other common notation used to indicate
    /// exponentiation as opposed to multiplication. In the absence of a superscript font, either
    /// `2^(NUMPAGENDXB + 1)` or `2**(NUMPAGENDXB + 1)` could be used as an acceptable substitute.
    /// Section 8.1.1 uses a proper superscript notation, and is more correct/clear.
    #[inline]
    pub(crate) fn reg_page_cnt(&self) -> usize {
        1usize << (self.init_state().idr1.numpagendxb() + 1)
    }

    /// Returns the effective number of Stream Match Register Groups managed by this SMMU.
    #[inline]
    pub(crate) fn num_smrgs(&self) -> u32 {
        self.init_state().num_smrgs
    }

    /// Returns the effective number of Context Banks managed by this SMMU.
    #[inline]
    pub(crate) fn num_cbs(&self) -> u32 {
        // `Relaxed` suffices: `num_cbs` is only written during `init()` before `self` is published
        // under `SmmuV2InstanceLock` (and before any context IRQ is unmasked), so lock
        // release/acquire already orders all post-init reads; `AtomicU32` only avoids
        // `&mut SmmuInitState` after IRQ handlers are registered.
        self.init_state().num_cbs.load(Ordering::Relaxed)
    }

    /// Returns the `GR0` MMIO bank handle.
    #[inline]
    pub(in crate::arm_smmu) fn gr0_base(&self) -> SmmuMmio {
        self.init_state().gr0_base
    }

    /// Returns the `GR1` MMIO bank handle.
    #[inline]
    pub(in crate::arm_smmu) fn gr1_base(&self) -> SmmuMmio {
        self.init_state().gr1_base
    }

    /// Returns the `CB0` MMIO bank handle.
    #[inline]
    pub(in crate::arm_smmu) fn cb0_base(&self) -> SmmuMmio {
        self.init_state().cb0_base
    }

    /// Returns the MMIO bank handle for Context Bank `ndx`.
    #[inline]
    pub(in crate::arm_smmu) fn get_cb_base(&self, ndx: u32) -> SmmuMmio {
        debug_assert!(ndx < self.num_cbs());
        let page_sz = self.reg_page_size();
        self.cb0_base().sub_bank(page_sz * (ndx as usize), page_sz)
    }

    /// Builds an [`SmmuParams`] view implementing `SmmuController` while holding `guard`.
    #[inline]
    pub(in crate::arm_smmu) fn controller_params<'a>(
        &'a self,
        guard: &'a SmmuV2LockGuard<'_>,
    ) -> SmmuParams<'a> {
        SmmuParams {
            name: self.name(),
            op_mode: self.op_mode(),
            num_smrgs: self.num_smrgs(),
            num_cbs: self.num_cbs(),
            available_smrgs: guard.available_smrgs(),
            available_cbs: guard.available_cbs(),
            gr0_base: self.gr0_base(),
            gr1_base: self.gr1_base(),
            cb0_base: self.cb0_base(),
            reg_page_size: self.reg_page_size(),
        }
    }

    /// Returns the [`IrqDef`] for context bank `ndx`, if within bounds.
    pub(crate) fn get_context_irq(&self, ndx: u32) -> Option<IrqDef> {
        ksync::lock!(let guard = self.lock_irq_lock());
        let idx = ndx as usize;
        if idx < guard.context_irqs().len() {
            Some(guard.context_irqs()[idx].irq_def)
        } else {
            None
        }
    }

    fn find_instance(
        token: &mut LockToken<'_, SmmuV2InstanceLock>,
        phys_reg_base: u64,
    ) -> Option<RefPtr<Self>> {
        for smmu in INSTANCES.get_mut(token).iter() {
            if smmu.phys_reg_base == phys_reg_base {
                return Some(RefPtr::from_ref(smmu));
            }
        }
        None
    }

    /// Creates, initializes, and registers a new `SmmuV2` instance from a ZBI driver config.
    pub fn create(config: &ZbiDcfgArmSmmuV2Driver) -> Result<RefPtr<Self>, Status> {
        // It is an error to attempt to create an SMMU instance with registers which
        // have already been used.
        ksync::lock!(let mut instance_guard = SmmuV2InstanceLock::lock());
        if Self::find_instance(instance_guard.as_mut().token_mut(), config.mmio_phys).is_some() {
            return Err(Status::ALREADY_BOUND);
        }

        // Looks like this is a new instance. Allocate an instance, then attempt to
        // initialize it. If we succeed, add the instance to the global list.
        let instance = Self::allocate_empty(config.mmio_phys, SmmuInitState::default())?;

        instance.init(config)?;
        INSTANCES.get_mut(instance_guard.as_mut().token_mut()).push_back(instance.clone());
        Ok(instance)
    }

    fn allocate_empty(
        phys_reg_base: u64,
        init_state: SmmuInitState,
    ) -> Result<RefPtr<Self>, Status> {
        pin_make_ref_counted!(SmmuV2 {
            list_node: DoublyLinkedListNode::new(),
            phys_reg_base,
            init_state: core::cell::UnsafeCell::new(init_state),
            global_irq_cookies: core::cell::UnsafeCell::new(Array::new()),
            context_irq_cookies: core::cell::UnsafeCell::new(Array::new()),
            lock <- KMutex::init(),
            user_mode_bound: KCell::new(false),
            bti_list <- KCell::pin_init(DoublyLinkedList::new()),
            available_smrgs: KCell::new(Bitmask128::new()),
            available_cbs: KCell::new(Bitmask128::new()),
            irq_lock <- KMutex::init(),
            global_irqs: KCell::new(Array::new()),
            context_irqs: KCell::new(Array::new()),
        })
        .map_err(|_| Status::NO_MEMORY)
    }

    /// Creates an unregistered `SmmuV2` backed by an owned fake MMIO register buffer (`GR0`,
    /// `GR1`, and 6 `CB` pages) for in-kernel unit tests without touching peripheral maps or
    /// hardware interrupt controllers.
    #[cfg(ktest)]
    pub(in crate::arm_smmu) fn create_for_unit_test(
        op_mode: ArmSmmuMode,
    ) -> Result<RefPtr<Self>, Status> {
        const TEST_PHYS_REG_BASE: u64 = 0x0900_0000;
        const TEST_NUM_SMRGS: u32 = 6;
        const TEST_NUM_CBS: u32 = 6;
        // `GR0` + `GR1` + `CB0..CB5` packed back-to-back as independent `SmmuMmio` banks.
        const TEST_REG_PAGES: usize = 2 + (TEST_NUM_CBS as usize);
        const UNIT_TEST_FAKE_REG_WORDS: usize =
            TEST_REG_PAGES * (PAGE_SIZE_USIZE / core::mem::size_of::<u64>());

        let fake_regs = Array::<u64>::try_new_zeroed_slice(UNIT_TEST_FAKE_REG_WORDS)
            .map_err(|_| Status::NO_MEMORY)?;
        let raw_regs = fake_regs.into_raw();
        let mut free_raw_regs = zr::defer(|| {
            // SAFETY: `raw_regs` came from `fake_regs.into_raw()` above and `allocate_empty`
            // failed before `SmmuV2` took ownership.
            unsafe { drop(Array::from_raw(raw_regs)) };
        });
        let base_vaddr = raw_regs.cast::<u64>().expose_provenance();
        // SAFETY: `raw_regs` is an 8-byte-aligned `TEST_REG_PAGES * PAGE_SIZE_USIZE` heap
        // allocation with exposed provenance, owned by `SmmuInitState::unit_test_regs` until
        // `SmmuV2`'s `PinnedDrop::drop` runs after all child BTIs/PMTs have been torn down, and
        // writes to this RAM-backed test buffer cannot cause DMA or hardware UB.
        let (gr0_base, gr1_base, cb0_base) = unsafe {
            (
                SmmuMmio::with_size(base_vaddr, PAGE_SIZE_USIZE),
                SmmuMmio::with_size(base_vaddr + PAGE_SIZE_USIZE, PAGE_SIZE_USIZE),
                SmmuMmio::with_size(
                    base_vaddr + 2 * PAGE_SIZE_USIZE,
                    (TEST_NUM_CBS as usize) * PAGE_SIZE_USIZE,
                ),
            )
        };

        let (name_buf, name_len) = Self::format_smmu_name(TEST_PHYS_REG_BASE);
        let mut idr0 = Idr0::from_value(0);
        // Stage-1 translation + stream matching, 15-bit StreamIDs, 1 global fault IRQ.
        idr0.set_s1ts(1).set_sms(1).set_numsmrg(TEST_NUM_SMRGS).set_numsidb(15).set_numirpt(1);
        let mut idr1 = Idr1::from_value(0);
        // 4 KiB register pages (`pagesize = 0`), `2^(1 + 1) = 4` global pages (`numpagendxb = 1`).
        idr1.set_pagesize(0).set_numpagendxb(1).set_numcb(TEST_NUM_CBS);
        let mut idr2 = Idr2::from_value(0);
        // 48-bit input/output address sizes (`5` per SMMUv2 Table 9-5) and VMSAv8-64 4 KiB granule.
        idr2.set_ias(5).set_oas(5).set_ptfsv8_4kb(1);
        let mut idr7 = Idr7::from_value(0);
        idr7.set_major(2).set_minor(0);

        let init_state = SmmuInitState {
            name_buf,
            name_len,
            op_mode,
            idr0,
            idr1,
            idr2,
            idr7,
            valid_stream_id_mask: SmrValue::FULL_VALID_MASK,
            num_cbs: AtomicU32::new(TEST_NUM_CBS),
            num_smrgs: TEST_NUM_SMRGS,
            gr0_base,
            gr1_base,
            cb0_base,
            unit_test_regs: core::ptr::NonNull::new(raw_regs),
        };
        let instance = Self::allocate_empty(TEST_PHYS_REG_BASE, init_state)?;
        free_raw_regs.cancel();
        {
            ksync::lock!(let mut guard = instance.lock_lock());
            guard.as_mut().available_smrgs_mut().set_lowest_n_bits(TEST_NUM_SMRGS);
            guard.as_mut().available_cbs_mut().set_lowest_n_bits(TEST_NUM_CBS);
            let params = instance.controller_params(&guard);
            for i in 0..TEST_NUM_SMRGS {
                StreamMatchRegGroup::disable_at(&params, i);
            }
            for i in 0..TEST_NUM_CBS {
                ContextBank::disable_at(&params, i);
            }
        }

        Ok(instance)
    }

    /// Looks up and binds an `SmmuV2` instance matching `desc` (a [`ZxIommuDescArmSmmu`]).
    ///
    /// Returns `Ok(Some(smmu))` when the SMMU is in `Passthru` or `Enforced` mode, or `Ok(None)`
    /// when the global or per-instance SMMU mode is `Disabled` (indicating the caller should
    /// substitute a `StubIommu`).
    pub fn fetch(desc: &[u8]) -> Result<Option<RefPtr<Self>>, Status> {
        // When SMMUs are disabled by default, a request we cannot match to an instance gets a
        // stub IOMMU rather than an error: the caller is on a system which, as far as it should
        // be able to tell, has no SMMU.
        //
        // The default does not decide the answer for an instance we do find, though. The mode of
        // a specific SMMU can be overridden by its base address, in either direction, and it is
        // the instance's own mode which says whether it is in use (below).
        let disabled_by_default = SmmuEnv::boot_option_smmu_mode(None) == ArmSmmuMode::Disabled;

        // Check our parameters, then go looking for an instance which was created
        // during kernel startup which has not already been bound by user-mode.
        let Ok(bytes): Result<[u8; 8], _> = desc.try_into() else {
            if disabled_by_default {
                return Ok(None);
            }
            dprintf!(INFO, "Bad desc when fetching SMMUv2 ({:p}, {})\n", desc.as_ptr(), desc.len());
            return Err(Status::INVALID_ARGS);
        };

        // TODO(johngro): Currently the user-mode driver framework is creating two
        // IOMMUs for systems with an SMMU. One of them is a StubIommu instance with
        // an ID equal to the SMMU HW's base address. The other is a real SMMU
        // instance, but it gets created using an address with the LSB set in it.
        //
        // This is not correct. They should simply be attempting to create a single
        // SMMU using the base address specified in the device tree. If the SMMU is
        // disabled in the kernel, we will automatically create a Stub for them.
        // Otherwise, we will give them a handle to the real SMMU hardware, provided
        // that no other handle is currently bound to it.
        let register_base = u64::from_ne_bytes(bytes) & !7u64;

        ksync::lock!(let mut instance_guard = SmmuV2InstanceLock::lock());
        let Some(instance) =
            Self::find_instance(instance_guard.as_mut().token_mut(), register_base)
        else {
            return if disabled_by_default { Ok(None) } else { Err(Status::NOT_FOUND) };
        };

        // If the instance requested by the user is disabled, whether by default or because its
        // mode has been overridden, do not return a reference to it. Give them a new stub IOMMU
        // instance instead. Any disabled SMMUs we have in our global list are only there so we
        // can dump registers using `k` commands.
        if instance.op_mode() == ArmSmmuMode::Disabled {
            return Ok(None);
        }

        ksync::lock!(let mut guard = instance.lock_lock());
        if !*guard.user_mode_bound() {
            *guard.as_mut().user_mode_bound_mut() = true;
            Ok(Some(instance.clone()))
        } else {
            Err(Status::ALREADY_BOUND)
        }
    }

    /// Ends the binding made by `fetch`: the IOMMU dispatcher user mode held this SMMU through
    /// has been destroyed, or could not be made at all.
    ///
    /// The caller ends each binding exactly once. Nothing else is undone: a BTI made through
    /// the binding stays in `bti_list` until its own last handle is closed, and whoever binds
    /// next gets `ALREADY_EXISTS` from `create_bti` for a stream ID such a BTI still has.
    pub(crate) fn unbind_user_mode(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        assert!(*guard.user_mode_bound());
        *guard.as_mut().user_mode_bound_mut() = false;
    }

    /// Cleanly tears down an `SmmuV2` instance between tests and removes it from the static
    /// `INSTANCES` list while verifying destructor invariants.
    ///
    /// In production kernel boots, `SmmuV2` instances registered in `INSTANCES` live for the
    /// lifetime of the kernel, whereas unit and model tests create and destroy multiple `SmmuV2`
    /// instances within a single process.
    #[cfg(any(ktest, not(is_kernel)))]
    pub fn destroy_for_test(smmu: RefPtr<Self>) {
        let mut target: Option<RefPtr<Self>> = None;
        {
            ksync::lock!(let mut instance_guard = SmmuV2InstanceLock::lock());
            let list = INSTANCES.get_mut(instance_guard.as_mut().token_mut());
            let found = list.iter().any(|s| core::ptr::eq(s, &*smmu));
            if found {
                // SAFETY: `smmu` was verified to be in `INSTANCES`.
                target = unsafe { list.erase(&*smmu) };
            }
        }
        drop(smmu);
        let Some(target) = target else {
            return;
        };

        target.teardown_btis_for_test();

        {
            ksync::lock!(let _guard = target.lock_lock());
            target.unregister_interrupts();
        }
        drop(target);
    }

    /// Tears down an unregistered `SmmuV2` created by [`Self::create_for_unit_test`], dismantling
    /// any remaining orphaned or adopted BTIs before dropping the instance.
    #[cfg(ktest)]
    pub(in crate::arm_smmu) fn shutdown_for_unit_test(smmu: RefPtr<Self>) {
        smmu.teardown_btis_for_test();
        drop(smmu);
    }

    #[cfg(ktest)]
    fn teardown_btis_for_test(&self) {
        // Shut down any remaining adopted or quarantined BTIs before unregistering
        // interrupts so all `ContextBank`, `StreamMatchRegGroup`, and `DeviceAspace`
        // resources are cleanly released.
        loop {
            let bti = {
                ksync::lock!(let guard = self.lock_lock());
                guard.bti_list().front().map(RefPtr::from_ref)
            };
            let Some(bti) = bti else {
                break;
            };

            // Unpin any PMTs left active by an early test failure before shutting down the BTI.
            loop {
                let pmt = {
                    ksync::lock!(let pmt_guard = bti.lock_pmt_lock());
                    pmt_guard.active_pmt_list().front().map(RefPtr::from_ref)
                };
                let Some(pmt) = pmt else {
                    break;
                };
                pmt.release_pinned_memory();
            }

            let mut handled_adopted_without_cb = false;
            {
                ksync::lock!(let mut pmt_guard = bti.lock_pmt_lock());
                ksync::lock!(let mut bti_guard = bti.lock_lock());
                *pmt_guard.as_mut().quarantined_pmt_count_mut() = 0;
                *pmt_guard.as_mut().quarantined_page_count_mut() = 0;
                *pmt_guard.as_mut().orphaned_mut() = false;
                if *bti_guard.mode() == BtiMode::Adopted {
                    if let Some(ref mut cb) = *bti_guard.as_mut().context_bank_mut() {
                        cb.force_mode_for_test(BtiMode::Fault);
                        *bti_guard.as_mut().mode_mut() = BtiMode::Fault;
                    } else {
                        // Adopted BTI in bypass/fault mode (no ContextBank): disable its SMRGs
                        // directly.
                        while let Some(mut smrg) = bti_guard.as_mut().smrg_list_mut().pop_front() {
                            smrg.disable();
                        }
                        *bti_guard.as_mut().mode_mut() = BtiMode::Shutdown;
                        handled_adopted_without_cb = true;
                    }
                }
            }

            if handled_adopted_without_cb {
                let removed = {
                    ksync::lock!(let mut guard = self.lock_lock());
                    assert!(bti.in_container());
                    // SAFETY: `bti` was obtained from `self.bti_list` (the only list it can be
                    // linked in) and verified `in_container()` under `self.lock`.
                    unsafe { guard.as_mut().bti_list_mut().erase(&*bti) }
                };
                {
                    ksync::lock!(let mut bti_guard = bti.lock_lock());
                    *bti_guard.as_mut().smmu_mut() = None;
                }
                drop(removed);
            } else {
                bti.on_end_of_life();
            }
        }
    }

    /// Creates a new [`SmmuV2Bti`] for `bus_txn_id`.
    pub fn create_bti(&self, bus_txn_id: u64) -> Result<RefPtr<SmmuV2Bti>, Status> {
        // If our SMMU driver is set to disabled, no one should be attempting to
        // allocate any BTIs here. They should be using StubIommus instead. Check
        // upfront before acquiring `self.lock` or touching hardware registers.
        let bti_mode = match self.op_mode() {
            ArmSmmuMode::Passthru => BtiMode::Bypass,
            ArmSmmuMode::Enforced => BtiMode::Translation,
            ArmSmmuMode::Disabled => return Err(Status::BAD_STATE),
        };

        // Start by making sure the mask/id being passed to us is supported.
        let stream_ids = SmrValue::from_bti_id(bus_txn_id, self.valid_stream_id_mask())?;

        // Next, make sure that this mask/id does not collide with any Stream IDs
        // owned by existing BTIs.
        ksync::lock!(let mut guard = self.lock_lock());

        for bti in guard.bti_list().iter() {
            if bti.smr_intersects(stream_ids) {
                return Err(Status::ALREADY_EXISTS);
            }
        }

        // Great! It looks like this is a new set of stream IDs. We'll need to
        // allocate an SMRG and a ContextBank in order to proceed. See if we have any
        // to allocate.
        let smrg_ndx = guard.available_smrgs().find_first_set_bit().ok_or(Status::NO_RESOURCES)?;
        let cb_ndx = guard.available_cbs().find_first_set_bit().ok_or(Status::NO_RESOURCES)?;

        // Looks like we have the resources available, construct objects to manage them.
        let params = self.controller_params(&guard);
        let smrg = StreamMatchRegGroup::create_and_lockdown(&params, smrg_ndx, stream_ids)?;
        let cb = match ContextBank::create_and_lockdown(&params, cb_ndx) {
            Ok(cb) => cb,
            Err(err) => {
                // An SMRG has to be disabled before it is dropped (its `Drop` asserts this).
                // The register writes repeat the values `create_and_lockdown` wrote a moment
                // ago with `lock` held throughout, so no register changes value and the SMRG
                // stays disabled.
                let mut smrg = smrg;
                smrg.disable();
                return Err(err);
            }
        };

        // Almost there. Now we just need to construct a BTI, and activate it
        // according to our current SMMU mode of operation (either passthru, or
        // enforced).
        let bti = SmmuV2Bti::create(self, &mut guard, smrg, Some(cb), bti_mode)?;

        // Victory! Add ourselves to the list of active BTIs, and return a reference
        // for the BTI dispatcher to hold on to.
        guard.as_mut().bti_list_mut().push_back(bti.clone());
        Ok(bti)
    }

    fn init(&self, config: &ZbiDcfgArmSmmuV2Driver) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.lock_lock());

        // First, find our register bank in the peripheral map and stash our virtual
        // address to the registers. Then initialize our MMIO base for our global
        // registers.
        //
        // TODO(johngro): this code currently assumes that if the first address
        // exists, that all of the SMMU pages exist in the peripheral map. Some day
        // we should:
        // 1) Extend the peripheral map API to allow us to translate an address while
        //    simultaneously confirming that at least X bytes exist in the map after
        //    the translated address.
        // 2) Start by asking for a single page. Use the ID registers to determine
        //    the total size of the SMMU register bank.
        // 3) Then compute how many context banks are available to us, and have not
        //    been hidden by the secure side of things.
        // 4) Finally, verify that we can access the subset of register pages that we
        //    are supposed to be able to access.
        let reg_vaddr = SmmuEnv::periph_paddr_to_vaddr(self.phys_reg_base);
        if reg_vaddr == 0 {
            return Err(Status::NO_RESOURCES);
        }
        // SAFETY: `reg_vaddr` is the peripheral-map virtual address of `self.phys_reg_base`, and
        // `Smmu` only issues read-only ID register probes through this initial `gr0_base`.
        let gr0_base = unsafe { SmmuMmio::new(reg_vaddr) };

        // Read and cache our ID registers. Check the version number to be sure we
        // recognize the hardware. Stash our initial count of context banks and
        // compute our friendly name as we go.
        let idr0 = Idr0::read_from(&gr0_base);
        let idr1 = Idr1::read_from(&gr0_base);
        let idr2 = Idr2::read_from(&gr0_base);
        let idr7 = Idr7::read_from(&gr0_base);
        let (name_buf, name_len) = Self::format_smmu_name(self.phys_reg_base);

        // SAFETY: `self` has not been published yet and we hold `self.lock`.
        unsafe {
            let st = &mut *self.init_state.get();
            st.name_buf = name_buf;
            st.name_len = name_len;
            st.gr0_base = gr0_base;
            st.idr0 = idr0;
            st.idr1 = idr1;
            st.idr2 = idr2;
            st.idr7 = idr7;
        }

        if idr7.major() != 2 {
            dprintf!(
                INFO,
                "{} (v{}.{}) is not supported\n",
                self.name(),
                idr7.major(),
                idr7.minor()
            );
            return Err(Status::INVALID_ARGS);
        } else {
            dprintf!(INFO, "Found {} (v{}.{})\n", self.name(), idr7.major(), idr7.minor());
        }

        // Figure out how many context banks and SMRs we are allowed to use. This is
        // based on how many are still available to us after the secure side of things
        // has taken theirs, as well as the artificial limit which might be passed to
        // us by configuration.
        let mut num_cbs = idr1.numcb();
        if config.num_context_banks_override != 0 && config.num_context_banks_override < num_cbs {
            num_cbs = config.num_context_banks_override;
        }

        let mut num_smrgs = idr0.numsmrg();
        if config.num_smr_override != 0 && config.num_smr_override < num_smrgs {
            num_smrgs = config.num_smr_override;
        }

        // Compute the base address of the other register banks we plan to use. See
        // Section 8.1 for the details of the register address space.
        let page_size = self.reg_page_size();
        let reg_size = page_size * self.reg_page_cnt();
        // SAFETY: `reg_vaddr` is the peripheral-map virtual address of `self.phys_reg_base`,
        // `[reg_vaddr, reg_vaddr + 2 * reg_size)` is the SMMUv2 global and context register space,
        // and `Smmu` retains exclusive ownership of these handles to uphold SMMU/DMA register
        // invariants across all writes.
        let (gr0_base, gr1_base, cb0_base) = unsafe {
            (
                SmmuMmio::with_size(reg_vaddr, page_size),
                SmmuMmio::with_size(reg_vaddr + page_size, page_size),
                SmmuMmio::with_size(reg_vaddr + reg_size, reg_size),
            )
        };

        // Figure out what our operating mode should be based on our address and the
        // Zircon boot option.
        let mut op_mode = SmmuEnv::boot_option_smmu_mode(Some(self.phys_reg_base));

        // If "Enforced" has been selected, but this SMMU instance does not support
        // translation, we will need to downgrade to Bypass mode.
        if op_mode == ArmSmmuMode::Enforced && idr0.s1ts() == 0 {
            dprintf!(
                INFO,
                "WARNING - {} does not support Stage 1 Translation, but fully enforced operation \
                 was requested.  Downgrading to passthru mode.\n",
                self.name()
            );
            op_mode = ArmSmmuMode::Passthru;
        }

        // This code currently depends on stream-matching support and does not support
        // the older stream indexing mode.
        if idr0.sms() == 0 {
            dprintf!(
                INFO,
                "ERROR - {} does not support stream-matching (IDR0 {:#010x})\n",
                self.name(),
                idr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // This code does not currently support extended stream IDs. When extended
        // stream IDs are supported and enabled, the match registers' ID and MASK
        // fields move from being 15 bits, to 16 bits, and the location of the "valid"
        // bit moves from the SMR (stream match register) to the S2CB (stream to
        // context bank) register. Currently, if the feature is enabled and supported
        // when we come out of the bootloader, we _assume_ that it is required to
        // support the stream ID space used by the SOC, so we refuse to load.
        //
        // Support for this feature is not a huge lift, it just requires abstracting
        // the SMR/S2CB interface just a little bit.
        //
        // See section 2.3.1 Transaction streams : StreamID size.
        let mut cr0 = Cr0::read_from(&gr0_base);
        if idr0.exids() != 0 && cr0.exidenable() == ExidEnable::Enabled {
            dprintf!(
                INFO,
                "ERROR - {} does not support extended StreamIDs (IDR0 {:#010x}, CR0 {:#010x})\n",
                self.name(),
                idr0.reg_value(),
                cr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // In SMMUv2, IDR0.NUMIRPT == 1 means that we have a dedicated interrupt pin
        // for every context bank. If the value is > 1, then there are a pool of
        // interrupts which need to be shared across the various context banks, and
        // the CBAR.IRPTNDX field will need to be programmed to map specific context
        // banks to specific interrupts. Currently, we only support the dedicated
        // interrupt model, so make sure that is how this SMMU is configured.
        //
        // See section 3.6.2 Context Bank Interrupts.
        if idr0.numirpt() != 1 {
            dprintf!(
                INFO,
                "ERROR - {} SMMU does not have a dedicated interrupt per context bank. \
                 (IDR0 {:#010x})\n",
                self.name(),
                idr0.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // Use the number of stream ID bits as reported in IDR0 to compute a mask
        // which can tell us if an encoded stream ID ((mask << 16 | id) is valid or
        // not.
        let mut valid_stream_id_mask = (1u32 << idr0.numsidb()) - 1;
        valid_stream_id_mask |= valid_stream_id_mask << 16;

        // Similar to extended StreamIDs, we do not currently support the "Extended
        // Stream Matching" extension, which increases the total number of stream
        // match registers from 128 up to as many as 1024. Adding support would
        // involve changes to the logic of finding and allocating stream match
        // contexts, as well as some additional save/restore code to support SMMU low
        // power states. See Chapter 14 : Extended Stream Matching Extension.
        let mut cr2 = Cr2::read_from(&gr0_base);
        if idr0.exsmrgs() != 0 && cr2.exsmrgenable() != 0 {
            dprintf!(
                INFO,
                "{} does not support extended stream matching (IDR0 {:#010x}, CR2 {:#010x})\n",
                self.name(),
                idr0.reg_value(),
                cr2.reg_value()
            );
            return Err(Status::NOT_SUPPORTED);
        }

        // SAFETY: `self` has not been published yet and we hold `self.lock`.
        unsafe {
            let st = &mut *self.init_state.get();
            st.op_mode = op_mode;
            st.valid_stream_id_mask = valid_stream_id_mask;
            st.num_cbs = AtomicU32::new(num_cbs);
            st.num_smrgs = num_smrgs;
            st.gr0_base = gr0_base;
            st.gr1_base = gr1_base;
            st.cb0_base = cb0_base;
        }

        // Allocate storage for our global and context interrupts based on the values
        // passed to us, and make copies of the configuration.
        //
        // A configuration whose IRQ counts are inconsistent is rejected. So far the SMMU has
        // only been read (IDR0, IDR1, IDR2, IDR7, CR0 and CR2): nothing has been written, no
        // interrupt is configured, and there is no lockdown or TLB invalidation, so the SMMU
        // stays exactly as the bootloader left it, as it does for every other configuration
        // rejected above (unsupported version, no stream matching, and so on). In particular,
        // stream mappings set up by the bootloader are left in place, not locked down.
        if (config.irq_cnt as usize) > config.irqs.len() || config.global_irq_cnt > config.irq_cnt {
            dprintf!(
                INFO,
                "ERROR - {} invalid IRQ counts (irq_cnt={}, global_irq_cnt={})\n",
                self.name(),
                config.irq_cnt,
                config.global_irq_cnt
            );
            return Err(Status::INVALID_ARGS);
        }
        let all_irqs = &config.irqs[..config.irq_cnt as usize];
        let global_irqs = &all_irqs[..config.global_irq_cnt as usize];
        let context_irqs = &all_irqs[config.global_irq_cnt as usize..];

        self.allocate_global_irqs(global_irqs)?;
        self.allocate_context_irqs(context_irqs, idr1.numcb())?;

        // Set up our "available" bit masks. At this point, all SMRGs and CBs in the
        // system are considered to be "available", however right after this we might
        // end up reserving some of them because they were flagged as "Early init"
        // contexts.
        guard.as_mut().available_smrgs_mut().set_lowest_n_bits(self.num_smrgs());
        guard.as_mut().available_cbs_mut().set_lowest_n_bits(self.num_cbs());

        // If our mode is "disabled", don't go any further. We don't want to actually
        // claim any hardware, or change any of the existing HW configuration.
        if op_mode == ArmSmmuMode::Disabled {
            return Ok(());
        }

        // Reserve our registers so that they cannot be accessed by user mode.
        SmmuEnv::root_resource_filter_add_deny_region(
            self.phys_reg_base,
            reg_size << 1,
            ZX_RSRC_KIND_MMIO,
        );

        // If we are not disabled, make sure our configuration registers are set up as
        // we want them to be.
        cr0.set_vmid16en(0) // 8-bit VMIDs
            .set_smcfcfg(1) // Raise a fault when there are stream-match conflicts.
            .set_fb(0) // Disable force broadcast of TLB/cache maintenance ops.
            .set_ptm(1) // Private TLB maintenance. Don't listen to system-wide broadcasts.
            .set_vmidpne(1) // Private VMID namespaces
            .set_usfcfg(1) // Raise a fault on unidentified stream access.
            .set_gse(0) // Disable global stalling across contexts.
            .set_stalld(Stalld::Disabled) // Disable per-context stalling.
            .set_gcfgfie(0) // Disable global config fault interrupts (enabled below)
            .set_gcfgfre(1) // Abort transactions during global config faults (not RAZ/WI).
            .set_exidenable(ExidEnable::Disabled) // Do not use extended IDs (we checked this above)
            .set_gfie(0) // Disable global fault interrupts (enabled below)
            .set_gfre(1) // Abort transactions during global faults (not RAZ/WI).
            // Accesses are subject to translation, access ctrl, and attr-gen.
            .set_clientpd(ClientPd::Enabled)
            .write_to(&gr0_base);

        cr2.set_exsmrgenable(0) // Extended SMRGs are disabled.
            .set_compindexenable(0) // Disabled compressed StreamID matching (eg; use the SMRs)
            .write_to(&gr0_base);

        // Register all of our interrupts. If anything goes wrong after this, be sure
        // to give them all back.
        let mut cleanup_irqs = zr::defer(|| self.unregister_interrupts());
        self.register_global_irqs()?;
        self.register_context_irqs()?;

        // Go over the list of "handoff stream IDs" passed to us and adopt all of the
        // associated stream match register groups and context banks so that they are
        // not automatically locked down, or claimed by user-mode.
        //
        // The count comes from the bootloader, so it is clamped to the size of the array: only
        // stream IDs which are really in the configuration can exempt an SMRG from lockdown.
        let handoff_cnt = (config.handoff_smr_cnt as usize).min(config.handoff_smrs.len());
        let handoff_stream_ids = &config.handoff_smrs[..handoff_cnt];
        for &sid in handoff_stream_ids {
            if (sid & !valid_stream_id_mask) != 0 {
                dprintf!(
                    INFO,
                    "WARNING: {} is ignoring invalid early init stream ID {:#010x}\n",
                    self.name(),
                    sid
                );
                continue;
            }

            let early_stream_ids = SmrValue::from_raw(sid, valid_stream_id_mask);
            for i in 0..self.num_smrgs() {
                // A group adopted for an earlier entry of the list stays valid in hardware, and
                // a later entry may name the same stream IDs, or others it also matches. It is
                // already ours; adopting it twice would claim it twice.
                if !guard.available_smrgs().test_bit(i) {
                    continue;
                }
                let smr = Smr::read_at(&gr0_base, i);

                // When attempting to decide whether or not to adopt an SMRG, we compute
                // the intersection of the SMR mask/value with each member of the
                // early-init vector passed to us by config. Theoretically, this lets a
                // configuration specify multiple SIDs to adopt using only a single entry.
                //
                // That said, there are many possible misconfigurations for which proper
                // behavior is not well defined. For example, if pre-existing hardware
                // has a set of SID `[0x1, 0x3]`, and the global set of SIDs to adopt
                // includes `[0x1, 0x4, 0x5]`, then we have encountered an SMRG where one
                // of its SIDs is supposed to be adopted, and another is not.
                //
                // For now, we do our best with our intersection test. There is no
                // particularly clear guidance on how this is formally supposed to work.
                // Furthermore, we assume that if there is something incorrect about this
                // approach, that it will become apparent early in platform bringup, and
                // someone can come back here to tweak the code to handle the newly
                // discovered quirk.
                let reg_stream_ids = SmrValue::from_smr_reg(smr, valid_stream_id_mask);
                if smr.valid() != 0 && early_stream_ids.intersects(reg_stream_ids) {
                    match self.adopt_smrg(&mut guard, i, reg_stream_ids) {
                        Ok(()) => {
                            dprintf!(
                                INFO,
                                "{} : Locked SMRG {} (MASK {:#06x}, ID {:#06x}) which intersects \
                                 early-init stream ID (MASK {:#06x}, ID {:#06x})\n",
                                self.name(),
                                i,
                                smr.mask(),
                                smr.id(),
                                early_stream_ids.mask(),
                                early_stream_ids.id()
                            );
                        }
                        Err(status) => {
                            dprintf!(
                                INFO,
                                "{} : ERROR - Failed to lock SMRG {} (MASK {:#06x}, ID {:#06x}) \
                                 (err = {})\n",
                                self.name(),
                                i,
                                smr.mask(),
                                smr.id(),
                                status.into_raw()
                            );
                        }
                    }
                }
            }
        }

        // Now, lock-down any contexts which were not adopted.
        self.lockdown(&guard);

        // If the number of context banks we have is larger than the number of
        // context bank interrupts we were given, artificially limit the number of
        // context banks we will make available and use. We don't want to use any
        // context banks that we don't have interrupts for.
        let context_irq_cnt = context_irqs.len() as u32;
        if self.num_cbs() > context_irq_cnt {
            for i in context_irq_cnt..self.num_cbs() {
                guard.as_mut().available_cbs_mut().clear_bit(i);
            }
            // `Relaxed` suffices: written while holding `self.lock` before `self` is published
            // under `SmmuV2InstanceLock` or any context IRQ is unmasked; `AtomicU32` only avoids
            // forming `&mut SmmuInitState` after IRQ handlers are registered.
            self.init_state().num_cbs.store(context_irq_cnt, Ordering::Relaxed);
        }

        // Now that everything has been either locked down or adopted, globally
        // invalidate all of the non-secure, non-hypervisor TLB entries, providing us
        // a well defined starting state for the TLB cache.
        self.invalidate_all_tlb_entries();

        // Finally, enable global interrupts.
        let mut unmask_err: Option<Status> = None;
        {
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            for vec in irq_guard.global_irqs().iter() {
                if vec.registered
                    && let Err(status) = SmmuEnv::unmask_interrupt(vec.irq_def.num)
                {
                    dprintf!(
                        INFO,
                        "{} : Failed to unmask global IRQ {:#04x}\n",
                        self.name(),
                        vec.irq_def.num
                    );
                    unmask_err = Some(status);
                    break;
                }
            }

            // Enable global fault interrupts while holding `irq_lock`.
            if unmask_err.is_none() {
                cr0.set_gcfgfie(1).set_gfie(1).write_to(&gr0_base);
            }
        }
        if let Some(status) = unmask_err {
            return Err(status);
        }

        // Success. Cancel any cleanup operations and we are done.
        cleanup_irqs.cancel();
        Ok(())
    }

    fn allocate_global_irqs(&self, src: &[ZbiDcfgArmSmmuIrq]) -> Result<(), Status> {
        if src.is_empty() {
            return Ok(());
        }
        let num_vectors = src.len();
        let mut arr = Array::<GlobalIrqVector>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Global IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        let mut cookies = Array::<IrqCookie>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Global IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        for (i, item) in src.iter().enumerate() {
            arr[i] = GlobalIrqVector { irq_def: IrqDef::from_zbi(*item), registered: false };
            cookies[i] = IrqCookie { smmu: self as *const SmmuV2, ndx: i as u32 };
        }

        // SAFETY: `self` has not been published yet, we hold `self.lock`, and no interrupt
        // handler has been registered, so nothing else can be reading the cookies.
        unsafe {
            *self.global_irq_cookies.get() = cookies;
        }

        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        *irq_guard.as_mut().global_irqs_mut() = arr;
        Ok(())
    }

    fn allocate_context_irqs(&self, src: &[ZbiDcfgArmSmmuIrq], numcb: u32) -> Result<(), Status> {
        if src.is_empty() {
            return Ok(());
        }
        let num_vectors = (src.len() as u32).min(numcb) as usize;
        let mut arr = Array::<ContextIrqVector>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Context IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        let mut cookies = Array::<IrqCookie>::try_new(num_vectors).map_err(|_| {
            dprintf!(
                INFO,
                "{} : Failed to allocate storage for {} Context IRQs\n",
                self.name(),
                num_vectors
            );
            Status::NO_MEMORY
        })?;

        for (i, item) in src.iter().take(num_vectors).enumerate() {
            arr[i] = ContextIrqVector {
                irq_def: IrqDef::from_zbi(*item),
                enabled: false,
                registered: false,
                in_flight: false,
                bti: None,
            };
            cookies[i] = IrqCookie { smmu: self as *const SmmuV2, ndx: i as u32 };
        }

        // SAFETY: `self` has not been published yet, we hold `self.lock`, and no interrupt
        // handler has been registered, so nothing else can be reading the cookies.
        unsafe {
            *self.context_irq_cookies.get() = cookies;
        }

        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        *irq_guard.as_mut().context_irqs_mut() = arr;
        Ok(())
    }

    fn register_irqs<V: IrqVectorEntry>(&self) -> Result<(), Status> {
        let cookies = V::cookies(self);
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        let len = V::slice(&irq_guard).len();
        // SAFETY: `cookies` was sized to `len` in `allocate_*_irqs`, `self` has not been published
        // yet, and no interrupt handler for `V` has been registered yet, so we have exclusive
        // access to `*cookies.get()` before entering the registration loop. Extracting `base_ptr`
        // via `as_mut_ptr()` once upfront preserves mutable provenance without creating any
        // reference once handlers begin registering.
        let base_ptr = unsafe { (&mut *cookies.get()).as_mut_ptr() };

        for i in 0..len {
            // Validate, configure, and mask the interrupt with `irq_lock` held (interrupts off),
            // dropping `irq_lock` only around `register_int_handler` below.
            let def = V::slice(&irq_guard)[i].irq_def();
            // SAFETY: `i < len` matches the `cookies` allocation length from `allocate_*_irqs`.
            let v_ptr = unsafe { base_ptr.add(i).cast::<c_void>() };

            if !def.valid() {
                dprintf!(
                    INFO,
                    "{} : Invalid IRQ flags ({:#04x}) for {} IRQ num {:#04x}\n",
                    self.name(),
                    def.flags,
                    V::KIND,
                    def.num
                );
                return Err(Status::INVALID_ARGS);
            }

            if let Err(status) =
                SmmuEnv::configure_interrupt(def.num, def.trigger_mode(), def.polarity())
            {
                dprintf!(
                    INFO,
                    "{} : Failed to configure {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            if let Err(status) = SmmuEnv::mask_interrupt(def.num) {
                dprintf!(
                    INFO,
                    "{} : Failed to mask {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            // `irq_lock` is dropped before registering our interrupt handler.
            //
            // The interrupt drivers have a central lock (call it `A`) which is held during calls
            // to register/unregister, and also during dispatch before calling the registered
            // callback, which acquires `irq_lock` (after `A`). Registering with `irq_lock` held
            // would acquire them in the opposite order. This cannot actually deadlock, since the
            // interrupt was masked above and cannot fire, but tools like lockdep would see the
            // potential for it.
            //
            // SAFETY: `v_ptr` points to `cookies[i]`, which remains valid until
            // `unregister_interrupts` completes.
            if let Err(status) = irq_guard.as_mut().call_unlocked(|| unsafe {
                SmmuEnv::register_int_handler(def.num, v_ptr, V::THUNK)
            }) {
                dprintf!(
                    INFO,
                    "{} : Failed to register {} IRQ num {:#04x} (flags {:#04x}, err {}).\n",
                    self.name(),
                    V::KIND,
                    def.num,
                    def.flags,
                    status.into_raw()
                );
                return Err(status);
            }

            V::slice_mut(irq_guard.as_mut())[i].set_registered(true);
        }

        Ok(())
    }

    fn register_global_irqs(&self) -> Result<(), Status> {
        self.register_irqs::<GlobalIrqVector>()
    }

    fn register_context_irqs(&self) -> Result<(), Status> {
        self.register_irqs::<ContextIrqVector>()
    }

    fn deactivate_irqs<V: IrqVectorEntry>(&self, irq_guard: &mut Pin<&mut SmmuV2IrqLockGuard<'_>>) {
        let len = V::slice(irq_guard).len();
        for i in 0..len {
            let maybe_def = {
                let v = &V::slice(irq_guard)[i];
                v.registered().then(|| v.irq_def())
            };
            if let Some(def) = maybe_def {
                if let Err(err) =
                    irq_guard.as_mut().call_unlocked(|| SmmuEnv::unregister_int_handler(def.num))
                {
                    panic!(
                        "{} : Failed to unregister {} IRQ num {:#04x} (flags {:#04x}, err {}) \
                         during shutdown.",
                        self.name(),
                        V::KIND,
                        def.num,
                        def.flags,
                        err.into_raw()
                    );
                }
                V::slice_mut(irq_guard.as_mut())[i].set_registered(false);
            }
        }
    }

    fn unregister_interrupts(&self) {
        // Unregister our interrupts with `irq_lock` dropped around each `unregister_int_handler`
        // call. Why do we do this and why is this safe?
        //
        // When interrupts are dispatched, there is currently a global lock (the `pdev_lock`)
        // which is held for the duration of the dispatch of any non-permanent interrupt handler.
        // This lock is also held during registration/unregistration of an interrupt.
        //
        // Holding the lock in these two places provides a guarantee that after calling
        // `unregister_int_handler(num)`, we can be certain that we are not racing with any
        // interrupts in flight, and it is now safe to destroy any resources that an IRQ handler
        // might be targeting.
        //
        // But, it also implies that we have lock ordering rules to follow. We hold `irq_lock`
        // both while we dispatch interrupts, as well as when we modify our internal interrupt
        // state (registering/un-registering interrupts). The IRQ handler is going to obtain the
        // `pdev_lock`, and then attempt to hold our `irq_lock`. If we hold `irq_lock` while
        // walking our table, and then call `unregister_int_handler` with the lock held, it will
        // attempt to hold `pdev_lock` and we will have set up the potential for deadlock.
        //
        // So, we drop `irq_lock` while we unregister. Why is this ok? We hold `irq_lock` only:
        // 1) During interrupt dispatch, where we hold only `irq_lock` and do not mutate our
        //    interrupt tables.
        // 2) When associating or disassociating a context interrupt and an `SmmuV2Bti`, where we
        //    also hold the top-level `lock` mutex.
        // 3) When re-enabling a context bank interrupt (`reenable_context_bank_irq_locked`),
        //    where we hold `bti.lock` -> `irq_lock` without `lock`, and return early whenever
        //    `vec.bti` is `None`.
        //
        // Dropping `irq_lock` around `unregister_int_handler` synchronizes with any in-flight
        // interrupt dispatch before we re-lock and mutate the table. And `unregister_interrupts`
        // runs either before the `SmmuV2` is published (`init` error cleanup, when no BTI has been
        // handed out) or during teardown under `lock` after every BTI has already been shut down
        // (so every `vec.bti` is `None` and re-enable returns early).
        //
        // TODO(johngro): consider modeling this more formally using a token and a two-lock
        // AssertHeld pattern. Gaining exclusive access to the token requires holding both `lock`
        // and `irq_lock`, while gaining shared access requires holding only one of the two.
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        self.deactivate_irqs::<GlobalIrqVector>(&mut irq_guard);
        self.deactivate_irqs::<ContextIrqVector>(&mut irq_guard);
    }

    fn handle_global_irq(&self, ndx: u32) {
        {
            // Mask the top level interrupt to prevent an unknown device from spamming
            // the global IRQ handler with interrupts.
            //
            // TODO(johngro): Figure out a better approach to this.
            //
            // 1) If there is a device we have never created a BTI for which is getting
            //    blocked by us, we'd like to know about it, but we need a way to stop
            //    the device from spamming the global IRQ handler with faults. Look
            //    into either force-stalling the device, or perhaps disabling global
            //    _fault_ interrupts (but not global config error interrupts) and
            //    re-enabling them later on using a timer or a DPC.
            // 2) If we receive a global config interrupt, consider panicking. There is
            //    no good reason we should ever have a bad configuration, and we should
            //    consider this to be a bug.
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            debug_assert!((ndx as usize) < irq_guard.global_irqs().len());
            let _ = SmmuEnv::mask_interrupt(irq_guard.global_irqs()[ndx as usize].irq_def.num);
        }

        // Start by reading the global fault status register to see what caused the
        // interrupt. A value of zero means that we have processed everything and
        // are finished.
        let gr0_base = self.gr0_base();
        let gfsr = Gfsr::read_from(&gr0_base);
        if gfsr.reg_value() == 0 {
            return;
        }

        // If the MULTI bit is set, it means that there was at least one more fault
        // which occurred before we were able to service the first fault. When this
        // happens, the MULTI bit is set, but no other details of the fault are
        // recorded in order to preserve the values in the fault registers which
        // describe the first fault.
        if gfsr.multi() != 0 {
            dprintf!(
                INFO,
                "{} : FAULT - At least one global fault occurred while an existing fault was \
                 pending.  Details of subsequent faults have been lost.\n",
                self.name()
            );
        }

        const FAULT_NAMES: [&str; 9] = [
            "Invalid context",                  // bit 0, ICF
            "Unidentified stream",              // bit 1, USF
            "Stream match conflict",            // bit 2, SMCF
            "Unimplemented context bank",       // bit 3, UCBF
            "Unimplemented context interrupt",  // bit 4, UCIF
            "Configuration access",             // bit 5, CAF
            "External",                         // bit 6, EF
            "Permission",                       // bit 7, PF
            "Unsupported upstream transaction", // bit 8, UUT
        ];

        let gfar = Gfar::read_from(&gr0_base);
        let gfsynr0 = Gfsynr0::read_from(&gr0_base);
        let gfsynr1 = Gfsynr1::read_from(&gr0_base);

        let fault_ndx = gfsr.reg_value().trailing_zeros() as usize;
        let fault_name = FAULT_NAMES.get(fault_ndx).copied().unwrap_or("Unknown");
        dprintf!(
            INFO,
            "{}: A global \"{}\" fault has occurred. (GFSR = {:#010x})\n",
            self.name(),
            fault_name,
            gfsr.reg_value()
        );
        dprintf!(INFO, "{}: Fault Address : {:#018x}\n", self.name(), gfar.faddr());
        dprintf!(INFO, "{}: Stream ID     : {:#06x}\n", self.name(), gfsynr1.stream_id());
        dprintf!(
            INFO,
            "{}: {}{} {} {} {}fault (GFSYNR0 = {:#010x})\n",
            self.name(),
            if gfsynr0.nested() != 0 { "Nested, " } else { "" },
            if gfsynr0.pnu() != 0 { "Privileged" } else { "Non-Privileged" },
            if gfsynr0.ind() != 0 { "instruction" } else { "data" },
            if gfsynr0.wnr() != 0 { "write" } else { "read" },
            if gfsynr0.ats() != 0 { "address translation " } else { "" },
            gfsynr0.reg_value()
        );

        // Zero all of the fault-detail registers, then ack the fault(s) in the status
        // register (GFSR) by writing back the defined W1C bits we read at the start of
        // processing (`Gfsr::CLEAR_ALL` leaves reserved UNK/SBZP bits zero; section 9.6.15).
        Gfar::from_value(0).write_to(&gr0_base);
        Gfsynr0::from_value(0).write_to(&gr0_base);
        Gfsynr1::from_value(0).write_to(&gr0_base);
        Gfsr::from_value(gfsr.reg_value() & Gfsr::CLEAR_ALL).write_to(&gr0_base);
    }

    fn handle_context_irq(&self, cb_ndx: u32) {
        debug_assert!(cb_ndx < self.num_cbs());

        let (target, irq_num) = {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            debug_assert!((cb_ndx as usize) < irq_guard.context_irqs().len());
            let vec = &mut irq_guard.as_mut().context_irqs_mut()[cb_ndx as usize];

            // Attempt to take a reference to our target BTI before we drop the IRQ lock
            // and dispatch the interrupt.
            let irq_num = vec.irq_def.num;
            let target = vec.bti.clone();
            let _ = SmmuEnv::mask_interrupt(irq_num);
            if target.is_some() {
                // `reenable_context_bank_irq_locked` does not return early on `vec.in_flight`, so
                // a quarantine release on another CPU can unmask `irq_num` once we release
                // `bti.lock` below and before we clear `in_flight`. `in_flight` can remain a
                // plain `bool` (avoiding re-entrant IRQ-time BTI destruction) because the GIC
                // interrupt controller drivers only EOI after this handler returns, preventing
                // the same SPI from being redelivered while we are still running.
                debug_assert!(!vec.in_flight);
                vec.in_flight = true;
                vec.enabled = false;
            }
            (target, irq_num)
        };

        let had_target = target.is_some();
        if let Some(bti) = target {
            // Enter our target's lock, print our fault details, then inform our BTI of
            // the fault so it can go into lockdown mode and (someday) raise a signal on
            // the kernel object indicating that there was a fault.
            //
            // The only way to restore functionality at this point is for the driver to
            // take control of its hardware, and release the quarantine state on the BTI.
            {
                ksync::lock!(let mut bti_guard = bti.lock_lock());
                let name_buf = *bti_guard.name();
                let bti_name = SmmuV2Bti::name_display(&name_buf);
                self.print_common_context_irq_fault_details(cb_ndx, Some(bti_name));
                bti.handle_fault_locked(&mut bti_guard);
            }
            // Give up our reference while `in_flight` is still set. A BTI being shut down waits
            // for `in_flight` to clear, holding references of its own until it has. Once the
            // flag is clear the shutdown can run to completion on another CPU, and a reference
            // still held here could then be the last one: the BTI would be destroyed, and its
            // memory freed, at interrupt time.
            drop(bti);
        } else {
            // This is very strange, but it is technically possible to receive a CB
            // interrupt with no target if we happen to be shutting down our target
            // right as one last fault interrupt arrives. In a situation like this, we
            // (the SMMU instance) _technically_ own these context bank fault syndrome
            // registers, so it should be OK to access the registers without holding a
            // BTI's lock.
            dprintf!(
                INFO,
                "{} : WARNING - received context IRQ ({:#04x}) for context bank {}, but no BTI is registered.\n",
                self.name(),
                irq_num,
                cb_ndx
            );
            self.print_common_context_irq_fault_details(cb_ndx, None);
        }

        // Clear our `in_flight` flag if we set it above. It is only set when a target BTI was
        // found; a context IRQ can arrive with no BTI registered, and then there is nothing to
        // clear.
        if had_target {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            let vec = &mut irq_guard.as_mut().context_irqs_mut()[cb_ndx as usize];
            debug_assert!(vec.in_flight);
            vec.in_flight = false;
        }
    }

    fn print_common_context_irq_fault_details(
        &self,
        cb_ndx: u32,
        bti_name: Option<EscapedName<'_>>,
    ) {
        // Unconditionally disable the context bank interrupt at the context bank level.
        let cb_base = self.get_cb_base(cb_ndx);
        let gr1_base = self.gr1_base();
        Sctlr::read_from(&cb_base).set_cfie(0).write_to(&cb_base);

        // Next attempt to log the relevant details of the fault, even if there is no
        // user-mode BTI associated with this context bank.
        let fsr = Fsr::read_from(&cb_base);
        let far = Far::read_from(&cb_base);
        let fsynr0 = Fsynr0::read_from(&cb_base);
        let cbfrsynra = Cbfrsynra::read_at(&gr1_base, cb_ndx);

        const FAULT_NAMES: [&str; 9] = [
            "Unknown (reserved)",               // bit 0, Reserved
            "Translation",                      // bit 1, TF
            "Access flag",                      // bit 2, AFF
            "Permission",                       // bit 3, PF
            "External",                         // bit 4, EF
            "TLB match conflict",               // bit 5, TLBMCF
            "TLB lock",                         // bit 6, TLBLKF
            "Address size",                     // bit 7, ASF
            "Unsupported upstream transaction", // bit 8, UUT
        ];
        let fault_ndx = fsr.reg_value().trailing_zeros() as usize;
        let fault_name = FAULT_NAMES.get(fault_ndx).copied().unwrap_or("Unknown");

        {
            // Render our report into the global fault buffer while holding the global fault buffer
            // lock, then emit it with a single console write so that the lines of one report
            // cannot be interleaved with other output. Sharing a global fault buffer allows us to
            // render the string without needing a large stack allocation at hard IRQ time.
            //
            // It is assumed that systems will not be taking large numbers of concurrent SMMU
            // faults, meaning that this lock should never be anything resembling a significant
            // choke point.
            ksync::lock!(let mut fault_guard = FaultBufferLock::lock());
            let mut f =
                FaultReportWriter::new(FAULT_BUFFER.get_mut(fault_guard.as_mut().token_mut()));

            let yes_no = |flag: u32| if flag != 0 { "yes" } else { "no" };
            let _ = writeln!(
                f,
                "{}{} \"{}\" fault has occurred in context bank {}{}. (FSR = {:#010x})",
                DRIVER_OOPS_BANNER,
                if fsr.multi() != 0 { "More than one" } else { "A" },
                fault_name,
                cb_ndx,
                if fsr.ss() != 0 { ", and the context is now stalled" } else { "" },
                fsr.reg_value()
            );
            let _ = writeln!(f, "SMMU              : {}", self.name());
            let _ = writeln!(
                f,
                "BTI Name          : {}",
                bti_name.unwrap_or(EscapedName::new(b"<unknown>"))
            );
            let _ = writeln!(f, "Stream ID         : {:#x}", cbfrsynra.stream_id());
            let _ = writeln!(f, "Fault Address     : {:#018x}", far.faddr());
            let _ = writeln!(f, "Stalled           : {}", yes_no(fsr.ss()));
            let _ = writeln!(f, "Permission Fault  : {}", yes_no(fsr.pf()));
            let _ = writeln!(f, "Access Fault      : {}", yes_no(fsr.aff()));
            let _ = writeln!(f, "Translation Fault : {}", yes_no(fsr.tf()));
            const OTHER_FAULT_MASK: u32 = 0x1f0;
            if (fsr.reg_value() & OTHER_FAULT_MASK) != 0 {
                let _ = writeln!(f, "Other faults present (FSR = {:#010x})", fsr.reg_value());
            }

            if fsr.pf() != 0 || fsr.aff() != 0 || fsr.tf() != 0 {
                let _ = writeln!(
                    f,
                    "{}{} {} {} fault{} at page table level {} (FSYNR0 = {:#010x})",
                    if fsynr0.afr() != 0 { "Asynchronous, " } else { "" },
                    if fsynr0.pnu() != 0 { "Privileged" } else { "Non-Privileged" },
                    if fsynr0.ind() != 0 { "instruction" } else { "data" },
                    if fsynr0.wnr() != 0 { "write" } else { "read" },
                    if fsynr0.ptwf() != 0 { ", during a page-table walk," } else { "" },
                    fsynr0.plvl(),
                    fsynr0.reg_value()
                );
            }

            SmmuEnv::print_driver_oops_report(f.as_str());
        }

        // Clear any of the fault bookkeeping so we are ready to receive a new context
        // fault when we are able to.
        //
        // Then ack the fault(s) in the status register (FSR) by writing back the defined
        // W1C fault bits we read at the start of processing (`Fsr::CLEAR_ALL` leaves
        // read-only `SS`/`Format` and reserved UNK/SBZP bits zero; section 16.5.9).
        Cbfrsynra::from_value(0).write_at(&gr1_base, cb_ndx);
        Far::from_value(0).write_to(&cb_base);
        Fsynr0::from_value(0).write_to(&cb_base);
        Fsr::from_value(fsr.reg_value() & Fsr::CLEAR_ALL).write_to(&cb_base);
    }

    fn lockdown(&self, guard: &SmmuV2LockGuard<'_>) {
        let gr0_base = self.gr0_base();
        let gr1_base = self.gr1_base();
        let params = self.controller_params(guard);

        // Go over all of the stream match register groups and make sure that they are
        // all disabled. If we find any enabled SMRs, print some details about the
        // fact that they were enabled as we disabled them.
        for i in 0..self.num_smrgs() {
            // Skip any SMRGs which were adopted during early-init.
            if !guard.available_smrgs().test_bit(i) {
                dprintf!(INFO, "{} : Lockdown skipping adopted SMRG {}\n", self.name(), i);
                continue;
            }

            let smr = Smr::read_at(&gr0_base, i);
            let s2cr = S2cr::read_at(&gr0_base, i);

            if smr.valid() != 0 {
                // Figure out what mode this group is operating in so we can report it.
                let mut tag1 = arm_s2cr_type_to_string(s2cr.s2cr_type());
                let mut tag2 = "";

                // If we are in translation mode, examine the configured context bank to
                // figure out how it was configured instead of simply saying "Translation".
                if s2cr.s2cr_type() == S2crType::Translation {
                    let cb_ndx = S2crTranslation::from_value(s2cr.reg_value()).cbndx();
                    if cb_ndx < self.num_cbs() {
                        tag1 =
                            arm_cbar_type_to_string(Cbar::read_at(&gr1_base, cb_ndx).cbar_type());
                        let cb_base = self.get_cb_base(cb_ndx);

                        // We know that this is the stage 1 version of SCTLR because we know
                        // that this context bank was not adopted, and because we never
                        // currently configure a context bank for stage 2 translation.
                        tag2 = if Sctlr::read_from(&cb_base).m() != 0 {
                            " - S1 Translate Enabled"
                        } else {
                            " - S1 Translate Disabled"
                        };
                    } else {
                        tag1 = "Translate (invalid context bank)";
                    }
                }

                dprintf!(INFO, "{} : Lockdown\n", self.name());
                dprintf!(INFO, "SMRG : {}\n", i);
                dprintf!(INFO, "SMR  : ID {:#06x} MASK {:#06x}\n", smr.id(), smr.mask());
                dprintf!(INFO, "Mode : {}{}\n", tag1, tag2);
            }

            StreamMatchRegGroup::disable_at(&params, i);
        }

        // Go over all of our context banks which have not been claimed (adopted) and
        // disable them so that any transaction which might happen to reach it will be
        // denied.
        for i in 0..self.num_cbs() {
            // Skip any Context Banks which were adopted during early-init.
            if !guard.available_cbs().test_bit(i) {
                dprintf!(INFO, "{} : Lockdown skipping locked CB {}\n", self.name(), i);
                continue;
            }

            ContextBank::disable_at(&params, i);
        }
    }

    fn invalidate_all_tlb_entries(&self) {
        let gr0_base = self.gr0_base();
        // Invalidate all TLB entries tagged as Non-secure, Non-hyp.
        Tlbiallnsnh::from_value(0).write_to(&gr0_base);
        Tlbgsync::from_value(0).write_to(&gr0_base);
        while Tlbgstatus::read_from(&gr0_base).gsactive() != 0 {
            SmmuEnv::yield_cpu();
        }

        // See Section 5.4.2 "TLB maintenance operation processing"'s SYNC assembly
        // example. We need a `dsb sy` instruction following this operation.
        SmmuEnv::device_memory_barrier();
    }

    /// Shuts down `bti` and removes it from this SMMU's BTI list.
    pub(crate) fn shutdown_bti(&self, bti: &SmmuV2Bti) {
        // Shutting a BTI down is a process which takes a bit of care. The sequence
        // is generally as follows:
        //
        // First, lock the SMMU for the duration of the process.
        //
        // Next, we need to deal with any context bank IRQ we may have registered, and
        // be certain that there cannot be any context bank IRQ in flight before we go
        // any further.
        //
        // So, lock the target BTI and peek at its current context bank, if any. If
        // it has a context bank, then make a note of the CB's index. Once a BTI has
        // a context bank assigned, the only way for the context bank (and its
        // associated IRQ) to be returned to the SMMU pool is to be shut down via the
        // SMMU itself. We know that a concurrent shutdown operation cannot take
        // place right now, because we are holding the main SMMU lock, so we can be
        // sure that the CB effectively belongs to us and cannot be reused until we
        // are done here.
        //
        // So, if we have a context bank index assigned to us, we can unregister the
        // IRQ associated with that index (obtaining the IRQ lock in the process) and
        // synchronize with any in-flight IRQ operations without needing to worry
        // about holding the BTI lock as we do.
        //
        // Once we are certain that there are no interrupts in flight targeting our
        // hardware, and that there will not be until we are finished, we are free to
        // call into the BTI instance itself in order to shut down the rest of the
        // hardware.
        let removed = {
            ksync::lock!(let mut guard = self.lock_lock());
            let maybe_cb_ndx = {
                ksync::lock!(let bti_guard = bti.lock_lock());
                assert!(bti_guard.smmu().as_deref().is_some_and(|s| core::ptr::eq(s, self)));
                bti_guard.context_bank().as_ref().map(|cb| cb.cb_ndx())
            };

            if let Some(cb_ndx) = maybe_cb_ndx {
                // Note: we know that there are at least two additional references to this
                // BTI which currently exist, independent of the reference which might be
                // held by the IRQ handler. They are the reference held by our main BTI
                // list, as well as the reference which this method's caller must be
                // holding.
                self.shutdown_context_bank_irq(cb_ndx);
            }

            // Go ahead and shut down the BTI hardware now, locking down all transactions
            // and returning all resources to the SMMU's pool in the process. Once we are
            // done with that, we can remove ourselves from the global BTI list and we
            // should be finished.
            assert!(bti.in_container());
            bti.shutdown(&mut guard);
            // SAFETY: `bti` was verified to belong to `self` and to be linked in `self.bti_list`
            // while holding `self.lock`.
            unsafe { guard.as_mut().bti_list_mut().erase(bti) }
        };
        drop(removed);
    }

    /// Associates `bti` with the context bank interrupt at `cb_ndx` and unmasks it.
    pub(crate) fn associate_bti_irq(&self, bti: &RefPtr<SmmuV2Bti>, cb_ndx: u32) {
        ksync::lock!(let mut irq_guard = self.lock_irq_lock());
        let idx = cb_ndx as usize;
        if idx >= irq_guard.context_irqs().len() {
            dprintf!(
                INFO,
                "{} : WARNING - Failed to associate context IRQ with BTI, invalid index ({} \
                 >= {})\n",
                self.name(),
                cb_ndx,
                irq_guard.context_irqs().len()
            );
            return;
        }

        let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
        debug_assert!(vec.bti.is_none());
        debug_assert!(vec.registered);

        vec.bti = Some(bti.clone());
        vec.enabled = true;
        let _ = SmmuEnv::unmask_interrupt(vec.irq_def.num);
    }

    /// Masks and disassociates the context bank interrupt at `cb_ndx`, synchronizing with any
    /// in-flight IRQ.
    pub(crate) fn shutdown_context_bank_irq(&self, cb_ndx: u32) {
        let bti_ref: Option<RefPtr<SmmuV2Bti>>;
        let mut in_flight: bool;
        let idx = cb_ndx as usize;
        {
            ksync::lock!(let mut irq_guard = self.lock_irq_lock());
            if idx >= irq_guard.context_irqs().len() {
                dprintf!(
                    INFO,
                    "{} : WARNING - Failed to disable context IRQ with BTI, invalid index ({} \
                     >= {})\n",
                    self.name(),
                    cb_ndx,
                    irq_guard.context_irqs().len()
                );
                return;
            }

            let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
            debug_assert!(vec.registered);

            in_flight = vec.in_flight;
            bti_ref = vec.bti.take();
            let _ = SmmuEnv::mask_interrupt(vec.irq_def.num);
        }
        drop(bti_ref);

        // Synchronize with any in-flight IRQs. In the unlikely event that an IRQ is
        // currently being processed on another CPU, poll until it completes before
        // allowing the BTI and its ContextBank to be torn down.
        while in_flight {
            SmmuEnv::sleep_usec(100);
            ksync::lock!(let irq_guard = self.lock_irq_lock());
            in_flight = irq_guard.context_irqs()[idx].in_flight;
        }
    }

    /// Re-enables the context bank fault interrupt of `bti` under `bti`'s lock, discarding any
    /// faults recorded while `bti` was in its fault state.
    pub(crate) fn reenable_context_bank_irq_locked(
        &self,
        bti: &SmmuV2Bti,
        bti_guard: &Pin<&mut SmmuV2BtiLockGuard<'_>>,
    ) {
        // `bti.lock` already disabled interrupts via `IrqSavePolicy`, so acquire `irq_lock`
        // with `SmmuNoIrqSavePolicy`.
        ksync::lock!(let mut irq_guard = self.lock_irq_lock_policy::<SmmuNoIrqSavePolicy>());

        let cb_ndx = SmmuV2Bti::cb_ndx_locked(bti_guard);
        if cb_ndx == u32::MAX {
            // The BTI has no context bank: it is being shut down, or was adopted without one.
            // Nothing to re-enable, and nothing wrong.
            return;
        }
        let idx = cb_ndx as usize;
        if idx >= irq_guard.context_irqs().len() {
            dprintf!(
                INFO,
                "{} : WARNING - Failed to re-enable context IRQ with BTI, invalid index ({} \
                 >= {})\n",
                self.name(),
                cb_ndx,
                irq_guard.context_irqs().len()
            );
            return;
        }

        // `vec.enabled` is not consulted: a leaked PMT rewrites a passthru context bank with
        // `SCTLR.CFIE` clear without going through the interrupt handler, leaving `vec.enabled`
        // set while the bank's enable bit is off. Our caller only reaches us when transitioning
        // `bti` out of `BtiMode::Fault` under the same hold of `bti.lock`.
        //
        // `vec.in_flight` is only consulted when `SCTLR.CFIE` is still set:
        // - Once `handle_context_irq` has cleared `SCTLR.CFIE`, put `bti` into `Fault` mode, and
        //   released `bti.lock`, `vec.in_flight` remains set briefly until the handler clears it
        //   under `irq_lock`; a release in that tail must still re-arm the interrupt.
        // - Conversely, if an enforced-mode BTI entered `Fault` mode via a leaked PMT (`CFIE`
        //   still set) while a context IRQ on another CPU masked the vector (`in_flight` set) and
        //   is spinning for `bti.lock`, `FSR` belongs to that waiting handler; leaving it intact
        //   and masked lets the handler report and quarantine that fault cleanly once we drop
        //   `bti.lock`.
        //
        // `vec.bti` is consulted: `shutdown_context_bank_irq` clears `vec.bti` under `SmmuV2::lock`
        // and `irq_lock` before `SmmuV2Bti::shutdown` takes `bti.lock` to return `cb_ndx` to the
        // pool, so `vec.bti` is either `None` (shutdown is underway and the interrupt must stay
        // disabled) or points to `bti`.
        let vec = &mut irq_guard.as_mut().context_irqs_mut()[idx];
        debug_assert!(vec.bti.as_deref().is_none_or(|v| core::ptr::eq(v, bti)));
        if !vec.registered || vec.bti.is_none() {
            return;
        }

        if *bti_guard.mode() != BtiMode::Bypass && *bti_guard.mode() != BtiMode::Translation {
            return;
        }

        let cb_base = self.get_cb_base(cb_ndx);
        if vec.in_flight && Sctlr::read_from(&cb_base).cfie() != 0 {
            return;
        }

        // Discard whatever faults were recorded while the BTI was in its fault state: the
        // device may have carried on issuing transactions, each of which sets `FSR` again, and
        // the interrupt is asserted for as long as `FSR` is non-zero. Without this, the
        // interrupt would fire the moment it is enabled and put the BTI straight back into the
        // fault state it was just released from. (`FSR` fault bits are write-one-to-clear.)
        Fsr::from_value(Fsr::CLEAR_ALL).write_to(&cb_base);
        Sctlr::read_from(&cb_base).set_cfie(1).write_to(&cb_base);
        let _ = SmmuEnv::unmask_interrupt(vec.irq_def.num);
        vec.enabled = true;
    }

    /// Adopts an already-enabled Stream Match Register Group and its associated Context Bank
    /// (if in translation mode) during early-init handoff.
    ///
    /// When adopting SMRG state, if the SMRG configuration references a context bank, one of two
    /// things may end up happening:
    ///
    /// 1) If the SMRG state references a context bank which has not yet been adopted, a new
    ///    `ContextBank` object will be created and adopted, and a new `SmmuV2Bti` object will be
    ///    created to hold the newly adopted SMRG and `ContextBank` objects. The existing SMRG and
    ///    `ContextBank` register state will be left as is. Internal bookkeeping will be updated to
    ///    match the pre-existing state.
    /// 2) If the SMRG state references a `ContextBank` which has already been adopted by a
    ///    different adopted SMRG, the newly adopted SMRG will be added to the existing `SmmuV2Bti`
    ///    which was created to contain the pre-existing SMRG(s)/`ContextBank` state.
    fn adopt_smrg(
        &self,
        guard: &mut Pin<&mut SmmuV2LockGuard<'_>>,
        smrg_ndx: u32,
        reg_stream_ids: SmrValue,
    ) -> Result<(), Status> {
        // The index we are being given must be valid, the group must be already
        // enabled, and not currently in use according to our bookkeeping. At this
        // point in initialization, we should also be able to ASSERT all of these
        // things.
        debug_assert!(smrg_ndx < self.num_smrgs());
        debug_assert!(guard.available_smrgs().test_bit(smrg_ndx));
        debug_assert_ne!(Smr::read_at(&self.gr0_base(), smrg_ndx).valid(), 0);

        // Go over our existing BTIs. None of the stream IDs we are attempting to
        // claim should currently be in use by any of our active BTIs. If they are,
        // we have a problem as it implies that there are at least two SMRGs
        // configured to match one or more of the same stream ID, which is undefined
        // behavior for SMMUv2 hardware.
        //
        // Return an error; we will disable and lock down this specific SMRG (at
        // `smrg_ndx`) after we have finished adoption of the rest of the early-init
        // SMRGs.
        for bti in guard.bti_list().iter() {
            if bti.smr_intersects(reg_stream_ids) {
                return Err(Status::ALREADY_BOUND);
            }
        }

        // Create a new SMRG, and adopt its initial state from existing register state.
        let mut smrg = StreamMatchRegGroup::create_and_adopt(
            &self.controller_params(guard),
            smrg_ndx,
            reg_stream_ids,
        )?;

        // If this SMRG is in translation mode, go looking for an existing BTI
        // which is already using the same context bank as this SMRG. If we find one,
        // just add this SMRG to it. Otherwise, we'll need to create a new BTI.
        if smrg.mode() == S2crType::Translation {
            let mut found_bti: Option<RefPtr<SmmuV2Bti>> = None;
            for bti in guard.bti_list().iter() {
                if bti.cb_ndx() == smrg.cb_ndx() {
                    found_bti = Some(RefPtr::from_ref(bti));
                    break;
                }
            }
            if let Some(existing_bti) = found_bti {
                existing_bti.add_smrg(guard, smrg);
                return Ok(());
            }
        }

        // We didn't find an existing BTI using this context bank (or the SMRG is not in
        // translation mode). If the SMRG is in translation mode, adopt its context bank
        // as well, then create a new BTI in the Adopted state to hold these resources.
        let context_bank = if smrg.mode() == S2crType::Translation {
            match ContextBank::create_and_adopt(&self.controller_params(guard), smrg.cb_ndx()) {
                Ok(cb) => Some(cb),
                Err(err) => {
                    // An SMRG has to be disabled before it is dropped (its `Drop` asserts
                    // this), so a stream match which cannot be adopted is turned off here, not
                    // left for `lockdown`. In practice this path is an allocation failure
                    // during early init. What follows from disabling it now:
                    //
                    // 1) The SMR is cleared (VALID, MASK and ID zero) and the S2CR set to
                    //    Fault, so the stream stops translating through a context bank we
                    //    failed to adopt. The order is the one `lockdown` uses (stream matches
                    //    first, context banks after), and CR0 has already been written, so the
                    //    now-unmatched stream is handled as it will be after lockdown.
                    // 2) `lockdown` still finds this SMRG available and disables it again,
                    //    repeating the values written here. As the SMR no longer reads as
                    //    valid, `lockdown` skips its diagnostic reads (CBAR and SCTLR of the
                    //    context bank) and its log lines for this SMRG; our caller logs the
                    //    failure.
                    // 3) A later handoff stream ID which intersects this same SMR no longer
                    //    sees it as valid, so adoption is not attempted a second time. A failed
                    //    adoption always ends locked down, whatever the order of the handoff
                    //    list.
                    smrg.disable();
                    return Err(err);
                }
            }
        } else {
            None
        };

        let bti = SmmuV2Bti::create(self, guard, smrg, context_bank, BtiMode::Adopted)?;
        guard.as_mut().bti_list_mut().push_back(bti);
        Ok(())
    }
}

/// Stack-allocated buffer for formatting register dump tags and fault lines in `no_std`.
pub(crate) struct TagBuf {
    buf: [u8; 160],
    len: usize,
}

impl TagBuf {
    pub(crate) const fn new() -> Self {
        Self { buf: [0u8; 160], len: 0 }
    }

    pub(crate) fn clear(&mut self) {
        self.len = 0;
    }

    pub(crate) fn as_str(&self) -> &str {
        // SAFETY: `write_str` only appends valid UTF-8 bytes into `self.buf[..self.len]`.
        unsafe { core::str::from_utf8_unchecked(&self.buf[..self.len]) }
    }
}

impl core::fmt::Write for TagBuf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let avail = self.buf.len().saturating_sub(self.len);
        let mut to_copy = bytes.len().min(avail);
        while !s.is_char_boundary(to_copy) {
            to_copy -= 1;
        }
        self.buf[self.len..self.len + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}
