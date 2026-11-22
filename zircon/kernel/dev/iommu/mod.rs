// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Zircon IOMMU, Bus Transaction Initiator (`Bti`), and Pinned Memory Token (`Pmt`) subsystem.
//!
//! Dispatches across concrete hardware IOMMU backends (`ArmSmmuV2` on `aarch64`) and the
//! architecture-independent pass-through fallback (`StubIommu` / `StubBti` / `StubPmt`) via closed
//! enums.

mod bti;
mod escaped_name;
mod stub;

use fbl::RefPtr;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

use crate::vm::pinned_vm_object::PinnedVmObject;

#[cfg(ktest)]
pub(crate) use bti::set_suppress_quarantine_oops_for_test;
pub use bti::{BtiPageLeakReason, print_quarantine_warning};
#[cfg(all(ktest, target_arch = "aarch64"))]
pub(crate) use bti::{
    CapturedQuarantineWarning, start_capturing_quarantine_warnings_for_test,
    take_captured_quarantine_warning_for_test,
};
pub use escaped_name::EscapedName;
pub use stub::{StubBti, StubIommu, StubPmt};

/// Permission flag allowing device read access to a mapped region.
pub const IOMMU_FLAG_PERM_READ: u32 = 1 << 0;

/// Permission flag allowing device write access to a mapped region.
pub const IOMMU_FLAG_PERM_WRITE: u32 = 1 << 1;

/// Permission flag allowing device instruction-fetch/execute access to a mapped region.
pub const IOMMU_FLAG_PERM_EXECUTE: u32 = 1 << 2;

/// Bitmask of all valid IOMMU permission flags.
pub const ALL_PERM_FLAGS: u32 =
    IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE | IOMMU_FLAG_PERM_EXECUTE;

/// The sentinel value used to indicate that a physical address is invalid.
///
/// IOMMUs, just like CPU MMUs, work in memory units of pages which are at least 4KB in length and
/// 4KB aligned. Page sizes can go up from there (16KB, 64KB, etc) but are always "page aligned"
/// implying that a valid page address always has the lowest N bits cleared. This address is all
/// 1s, meaning that it will never be a valid physical page address for any system unless the page
/// size was a single byte (which just does not happen).
pub const INVALID_PADDR: u64 = u64::MAX;

/// A strongly typed enum-style-bool which determines whether a request for a PMT mapping needs to
/// be contiguous in device virtual address space.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequireContiguousMapping {
    /// The mapping does not need to be contiguous in device virtual address space.
    No = 0,
    /// The mapping must be contiguous in device virtual address space.
    Yes = 1,
}

/// Device virtual address type (`dev_vaddr_t`), matching `zx_paddr_t` in `zx_bti_pin`.
#[allow(non_camel_case_types)]
pub type dev_vaddr_t = u64;

/// Result of querying a device virtual address range from a pinned memory token.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryAddressResult {
    /// Base device virtual address (IOVA or physical address in bypass mode).
    pub device_vaddr: dev_vaddr_t,
    /// Contiguous byte length starting at `device_vaddr`.
    pub size: usize,
}

zr::static_assert!(core::mem::size_of::<QueryAddressResult>() == 16);
zr::static_assert!(core::mem::align_of::<QueryAddressResult>() == 8);

// `Iommu`, `Bti`, and `Pmt` are embedded inside `#[repr(C)]` `*DispatcherState` structs backed by
// C++ `OpaqueStorage`, so their layout must be explicitly specified rather than left to
// `repr(Rust)`. On non-`aarch64` targets `Stub` is the sole variant, so `repr(transparent)` keeps
// each enum pointer-sized (8 bytes) with `NonNull` niche optimization for `Option<Pmt>` rather
// than paying 8 bytes of tag and padding on every dispatcher. On `aarch64`, `repr(C, u8)` fixes a
// 16-byte `(u8 tag + pad + RefPtr)` layout matching `kIommuEnumTagSize` in `object-constants.h`.

/// Closed enum over supported kernel IOMMU controller implementations.
///
/// `Iommu` holds a `RefPtr` per variant rather than a `dyn` trait object, eliminating vtable
/// indirection and avoiding double heap allocation (`SoloDispatcher` + `RefPtr`).
#[cfg_attr(target_arch = "aarch64", repr(C, u8))]
#[cfg_attr(not(target_arch = "aarch64"), repr(transparent))]
#[derive(Clone)]
pub enum Iommu {
    /// Pass-through stub IOMMU.
    Stub(RefPtr<StubIommu>),
    /// ARM SMMUv2 controller.
    #[cfg(target_arch = "aarch64")]
    ArmSmmuV2(RefPtr<crate::arm_smmu::v2::SmmuV2>),
}

impl Iommu {
    /// Creates or fetches an [`Iommu`] instance for `type_param` and `desc`.
    pub fn create(type_param: u32, desc: &[u8]) -> Result<Self, Status> {
        match type_param {
            zx_types::ZX_IOMMU_TYPE_STUB => {
                // TODO(b/462772483): Remove this check (or convert it to check for empty slice)
                // once we have removed the need to pass a `zx_iommu_desc_stub_t` at all.
                if desc.len() != core::mem::size_of::<zx_types::zx_iommu_desc_stub_t>() {
                    Err(Status::INVALID_ARGS)
                } else {
                    StubIommu::create().map(Self::Stub)
                }
            }
            #[cfg(target_arch = "aarch64")]
            zx_types::ZX_IOMMU_TYPE_ARM_SMMU => match crate::arm_smmu::v2::SmmuV2::fetch(desc)? {
                Some(smmu) => Ok(Self::ArmSmmuV2(smmu)),
                None => StubIommu::create().map(Self::Stub),
            },
            _ => Err(Status::NOT_SUPPORTED),
        }
    }

    /// Gives back what [`Iommu::create`] took, for a caller which then failed to make the
    /// dispatcher user mode would have held it through.
    ///
    /// Fetching an SMMU binds it to user mode. The binding normally ends when the dispatcher
    /// is destroyed ([`Iommu::on_dispatcher_destroyed`]); when no dispatcher could be made,
    /// nothing else would end it, and the SMMU would be unusable until the next boot. A stub
    /// IOMMU is made afresh for each caller and needs nothing.
    pub(crate) fn abandon_create(&self) {
        self.unbind();
    }

    /// Ends the binding to user mode which [`Iommu::create`] made, when the dispatcher which
    /// held this object is destroyed: its last handle is closed, and nothing can reach the
    /// IOMMU through it any more. Another `zx_iommu_create` may then have it.
    ///
    /// Called exactly once for each dispatcher, and never together with
    /// [`Iommu::abandon_create`]: a binding is ended once. Ended twice, the second could end
    /// the binding of whoever had bound in between.
    ///
    /// BTIs made through the binding are not affected. They hold the driver's own objects,
    /// not the dispatcher, and carry on; a later binder which asks for a BTI on a stream ID
    /// one of them still has is told it exists.
    pub(crate) fn on_dispatcher_destroyed(&self) {
        self.unbind();
    }

    fn unbind(&self) {
        match self {
            Self::Stub(_) => {}
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(iommu) => iommu.unbind_user_mode(),
        }
    }

    /// Creates a new Bus Transaction Initiator ([`Bti`]) in this specific [`Iommu`] instance.
    ///
    /// `bti_id` encodes a packed form of the ID of the initiator indicated to the IOMMU (along
    /// with a device virtual address) during a HW-initiated transaction. The precise encoding of
    /// `bti_id` depends on the specific IOMMU hardware being used, or is an arbitrary placeholder
    /// in the case of the [`StubIommu`] implementation.
    ///
    /// Fails with `Status::INVALID_ARGS` if `bti_id` is invalid for this IOMMU or
    /// `Status::ALREADY_EXISTS` if `bti_id` intersects an already-active BTI.
    pub fn create_bti(&self, bti_id: u64) -> Result<Bti, Status> {
        match self {
            Self::Stub(iommu) => iommu.create_bti(bti_id).map(Bti::Stub),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(iommu) => iommu.create_bti(bti_id).map(Bti::ArmSmmuV2),
        }
    }
}

/// Closed enum over supported Bus Transaction Initiator (`Bti`) implementations.
///
/// `Bti` holds a `RefPtr` per variant rather than a `dyn` trait object, eliminating vtable
/// indirection and avoiding double heap allocation.
#[cfg_attr(target_arch = "aarch64", repr(C, u8))]
#[cfg_attr(not(target_arch = "aarch64"), repr(transparent))]
#[derive(Clone)]
pub enum Bti {
    /// Pass-through stub BTI.
    Stub(RefPtr<StubBti>),
    /// ARM SMMUv2 BTI.
    #[cfg(target_arch = "aarch64")]
    ArmSmmuV2(RefPtr<crate::arm_smmu::v2::SmmuV2Bti>),
}

impl Bti {
    /// Returns the 64-bit hardware BTI ID of this instance.
    #[inline]
    pub fn bti_id(&self) -> u64 {
        match self {
            Self::Stub(bti) => bti.bti_id(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.bti_id(),
        }
    }

    /// Copies the BTI's debug name into `out_name`.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) {
        match self {
            Self::Stub(bti) => bti.get_name(out_name),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.get_name(out_name),
        }
    }

    /// Sets the BTI's debug name from `name`.
    pub fn set_name(&self, name: &[u8]) {
        match self {
            Self::Stub(bti) => bti.set_name(name),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.set_name(name),
        }
    }

    /// Grants the device identified by `bti_id()` access to the range of pages given by
    /// `[offset, offset + size)` in `pinned_vmo`. An opaque [`Pmt`] token representing the mapping
    /// is returned, which can be used with [`Pmt::release_pinned_memory`] or
    /// [`Pmt::query_address`].
    ///
    /// The memory in the given range of `pinned_vmo` MUST have been pinned before calling this
    /// function, and if this function returns `Ok`, MUST NOT be unpinned until after
    /// [`Pmt::release_pinned_memory`] (or quarantine release) is called on the returned token.
    ///
    /// `perms` defines the access permissions using the `IOMMU_FLAG_PERM_*` flags.
    ///
    /// If `size` is no more than [`Bti::minimum_contiguity`], this will never return a partial
    /// mapping.
    ///
    /// If `req_contig` is [`RequireContiguousMapping::Yes`], then the mapping is guaranteed to be
    /// contiguous in the BTI device's address space, or an error will be returned.
    ///
    /// # Errors
    /// - `Status::INVALID_ARGS` if `size` is zero, `offset` is not page-aligned, or `perms` is
    ///   invalid.
    /// - `Status::OUT_OF_RANGE` if `[offset, offset + size)` is not a valid range in the VMO.
    /// - `Status::BAD_STATE` if the BTI is in a fault state or has Quarantined PMTs.
    /// - `Status::NO_MEMORY` or `Status::NO_RESOURCES` if the mapping could not be made due to
    ///   lack of memory or an available address range.
    /// - `Status::INTERNAL` if a required contiguous physical range cannot be satisfied in bypass
    ///   mode.
    pub fn map(
        &self,
        pinned_vmo: PinnedVmObject,
        perms: u32,
        req_contig: RequireContiguousMapping,
    ) -> Result<Pmt, Status> {
        match self {
            Self::Stub(bti) => bti.map(pinned_vmo, perms, req_contig).map(Pmt::Stub),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti
                .map(
                    crate::arm_smmu::common::SmmuPinnedVmo::from_kernel_pinned_vmo(pinned_vmo),
                    perms,
                    req_contig,
                )
                .map(Pmt::ArmSmmuV2),
        }
    }

    /// Releases all quarantined PMTs on this BTI, returning their pinned memory to the system.
    pub fn release_quarantine(&self) {
        match self {
            Self::Stub(bti) => bti.release_quarantine(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.release_quarantine(),
        }
    }

    /// Called when the `BusTransactionInitiatorDispatcher` which owns this BTI reaches the end of
    /// its user-mode life (`SoloDispatcher::on_zero_handles`), or when that dispatcher could not
    /// be created after [`Iommu::create_bti`] succeeded, after which no new PMTs can be created on
    /// this BTI. Lets the driver level know in case there is cleanup to be done.
    pub fn on_dispatcher_zero_handles(&self) {
        match self {
            Self::Stub(bti) => bti.on_dispatcher_zero_handles(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.on_dispatcher_zero_handles(),
        }
    }

    /// Returns the smallest physical memory monitoring granularity (in bytes) that [`Bti::map`]
    /// can guarantee, upon success, to find a contiguous address range for, guaranteed to be a
    /// power-of-two multiple of `PAGE_SIZE`.
    pub fn minimum_contiguity(&self) -> u64 {
        match self {
            Self::Stub(bti) => bti.minimum_contiguity(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.minimum_contiguity(),
        }
    }

    /// Returns the total size (in bytes) of the address space the addresses are mapped into
    /// (`u64::MAX` if full 64-bit).
    pub fn aspace_size(&self) -> u64 {
        match self {
            Self::Stub(bti) => bti.aspace_size(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.aspace_size(),
        }
    }

    /// Returns the current total number of pinned memory objects, including both active and
    /// quarantined objects.
    pub fn pmo_count(&self) -> u64 {
        match self {
            Self::Stub(bti) => bti.pmo_count(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.pmo_count(),
        }
    }

    /// For BTI drivers which use quarantines, reports the number of currently quarantined PMTs.
    pub fn quarantine_count(&self) -> usize {
        match self {
            Self::Stub(bti) => bti.quarantine_count(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.quarantine_count(),
        }
    }

    /// Returns `true` when the underlying BTI is in a "Fault" state and `false` otherwise. BTIs
    /// generally enter a fault state for one of two reasons.
    ///
    /// **Leaked PMTs**
    ///
    /// If user-mode closes all of the handles to a PMT without making a call to `zx_pmt_unpin`,
    /// the PMT has been "leaked". This is considered to be a violation of the IOMMU/BTI contract
    /// as drivers, when they are finished with pinned memory, are supposed to make absolutely
    /// certain that their hardware is no longer attempting to access the pinned memory before
    /// formally signaling to the kernel that they have stopped their HW access by calling
    /// `zx_pmt_unpin`.
    ///
    /// For things like the Stub IOMMU implementation, this is a big problem. They don't know if
    /// the driver's HW might still be attempting to access the memory, and have no ability to stop
    /// it (as a real HW IOMMU would). So, they are forced to put the memory into a quarantine pool
    /// (not returning it to the physical page pool) until the driver is restarted, shuts down its
    /// hardware, and calls `zx_bti_release_quarantine` to indicate that it is finally safe to
    /// return the memory to the physical page pool.
    ///
    /// Similarly, an ARM SMMU operating in passthru mode has limited tools to deal with a
    /// situation like this. It is able to grant or revoke access to/from a specific BTI, but only
    /// in an all or nothing fashion. The initiator can either access any memory, or no memory. In
    /// a situation like this, the leaked PMT memory _can_ be returned to the pool, but *only*
    /// after all ability to access RAM has been revoked from the initiator.
    ///
    /// In either of these cases, the driver level BTI has entered a "Fault" state. Requests to pin
    /// new memory will be denied with "Bad State", and some (or all) of the device's ability to
    /// access memory will have been revoked from it depending on the ability of the underlying HW
    /// to enforce policy.
    ///
    /// **HW Induced Faults**
    ///
    /// The other reason a BTI might enter into the fault state is because of a transaction fault
    /// caused by an invalid access from an initiator. The HW might have attempted to touch a page
    /// of memory which was never pinned (bad address), or it might have attempted to touch a page
    /// which was pinned, but with an invalid type of access (eg, it tried to write to a read only
    /// page).
    ///
    /// Stub IOMMUs cannot detect this as they have no hardware enforcement, but actual HW IOMMUs
    /// who have translation and enforcement capabilities have a problem in a situation like this.
    /// If they simply print a warning (or ignore the error entirely) and re-enable their fault
    /// interrupt, they are almost certainly going to end up in a situation where they are
    /// constantly taking interrupts from the disallowed HW accesses.
    ///
    /// Something has gone very wrong with the HW managed by the user-mode driver, and only
    /// user-mode can fix it. Part of the driver's behavior will be to put the BTI into a "Fault"
    /// state, revoking all of the initiator's access to memory and refusing to pin new memory
    /// until the driver has taken control of their hardware and released their quarantine.
    pub fn in_fault_state(&self) -> bool {
        match self {
            Self::Stub(bti) => bti.in_fault_state(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(bti) => bti.in_fault_state(),
        }
    }

    /// Reclaims an orphaned [`StubBti`] and its quarantined PMTs at the end of a kernel unit test
    /// so `ORPHAN_LIST` and the PMM do not retain leaked test state.
    #[cfg(ktest)]
    pub(crate) fn reclaim_orphaned_for_test(&self) {
        match self {
            Self::Stub(bti) => bti.reclaim_orphaned_for_test(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(_) => {}
        }
    }
}

/// Closed enum over supported Pinned Memory Token (`Pmt`) implementations.
///
/// `Pmt` holds a `RefPtr` per variant rather than a `dyn` trait object, eliminating vtable
/// indirection and avoiding double heap allocation.
#[cfg_attr(target_arch = "aarch64", repr(C, u8))]
#[cfg_attr(not(target_arch = "aarch64"), repr(transparent))]
#[derive(Clone)]
pub enum Pmt {
    /// Pass-through stub PMT.
    Stub(RefPtr<StubPmt>),
    /// ARM SMMUv2 PMT.
    #[cfg(target_arch = "aarch64")]
    ArmSmmuV2(RefPtr<crate::arm_smmu::v2::SmmuV2Pmt>),
}

impl Pmt {
    /// Queries the information of the pinned VMO. Attempts to find the (single) continuous range
    /// from `[query_offset, query_offset + query_size)` in the device's address space for the
    /// pinned VMO managed by this token.
    ///
    /// On success, the returned range might be less than, equal to, or greater than the queried
    /// size. In the case of being less than, additional contiguous ranges can be found by calling
    /// again with a new `query_offset`.
    ///
    /// # Errors
    /// - `Status::INVALID_ARGS` if `query_offset` is not page-aligned or `query_size` is zero.
    /// - `Status::OUT_OF_RANGE` if `[query_offset, query_offset + query_size)` is not a valid
    ///   range in the mapping.
    pub fn query_address(
        &self,
        query_offset: u64,
        query_size: usize,
    ) -> Result<QueryAddressResult, Status> {
        match self {
            Self::Stub(pmt) => pmt.query_address(query_offset, query_size),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(pmt) => pmt.query_address(query_offset, query_size),
        }
    }

    /// Unmaps the memory managed by the PMT from its device's address space, revoking device
    /// access in the process. Then releases the reference to the memory held by the internal
    /// `PinnedVmObject` instance, potentially unpinning the memory and returning it to the PMM in
    /// the process.
    pub fn release_pinned_memory(&self) {
        match self {
            Self::Stub(pmt) => pmt.release_pinned_memory(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(pmt) => pmt.release_pinned_memory(),
        }
    }

    /// Called when the `PinnedMemoryTokenDispatcher` which owns this PMT reaches the end of its
    /// user-mode life. Lets the driver level know in case something special needs to be done (such
    /// as quarantining the memory).
    pub fn on_dispatcher_zero_handles(&self) {
        match self {
            Self::Stub(pmt) => pmt.on_dispatcher_zero_handles(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(pmt) => pmt.on_dispatcher_zero_handles(),
        }
    }

    /// Returns the size in bytes of the pinned VMO region.
    #[inline]
    pub fn size(&self) -> u64 {
        match self {
            Self::Stub(pmt) => pmt.size(),
            #[cfg(target_arch = "aarch64")]
            Self::ArmSmmuV2(pmt) => pmt.size(),
        }
    }

    /// Returns the number of pinned pages in this PMT.
    #[inline]
    pub fn pages(&self) -> u64 {
        self.size() / (page::SIZE as u64)
    }
}

#[cfg(target_arch = "aarch64")]
const EXPECTED_IOMMU_ENUM_SIZE: usize = 16;
#[cfg(not(target_arch = "aarch64"))]
const EXPECTED_IOMMU_ENUM_SIZE: usize = 8;

zr::static_assert!(core::mem::size_of::<Iommu>() == EXPECTED_IOMMU_ENUM_SIZE);
zr::static_assert!(core::mem::align_of::<Iommu>() == 8);
zr::static_assert!(core::mem::size_of::<Bti>() == EXPECTED_IOMMU_ENUM_SIZE);
zr::static_assert!(core::mem::align_of::<Bti>() == 8);
zr::static_assert!(core::mem::size_of::<Pmt>() == EXPECTED_IOMMU_ENUM_SIZE);
zr::static_assert!(core::mem::align_of::<Pmt>() == 8);
// `PinnedMemoryTokenDispatcherState` embeds `Option<Pmt>`, relying on niche optimization.
zr::static_assert!(core::mem::size_of::<Option<Pmt>>() == EXPECTED_IOMMU_ENUM_SIZE);
zr::static_assert!(core::mem::align_of::<Option<Pmt>>() == 8);
