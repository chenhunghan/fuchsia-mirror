// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! MMIO register bank handle, register bitfield metadata, and the declarative
//! [`define_smmu_reg!`] macro shared by the SMMUv2 and SMMUv3 register definitions.
//!
//! Defines [`SmmuMmio`], [`FieldDesc`], and [`define_smmu_reg!`], which stamps out zero-cost
//! `#[repr(transparent)]` register types backed by [`MmioBank`] and [`Offset`].

use core::mem::align_of;
use debug::dprintf;
#[cfg(is_kernel)]
use regio::{MmioBank, MmioPtr, Ro};
use regio::{Offset, RwUnsafe};

use super::env::SmmuEnv;

/// Default size in bytes of an SMMUv2 MMIO register bank (64 KiB, the maximum SMMUv2 page size).
pub(in crate::arm_smmu) const SMMU_MMIO_DEFAULT_BANK_SIZE: usize = 0x10000;

/// MMIO register bank handle for an SMMU register page (SMMUv2 `GR0`, `GR1`, or `CBn`; SMMUv3
/// Page 0 or Page 1).
///
/// Wraps a mapped virtual base address (`0` when uninitialized or disabled) and bank size in
/// bytes. Delegates to [`MmioBank`] (`MmioLe`) and [`Offset`] volatile loads and stores.
///
/// # Invariants
/// Either `base == 0 && size == 0` (the null/disabled handle), or `[base, base + size)` is a
/// valid, 8-byte-aligned mapped SMMU MMIO region (or a valid host test MMIO region / buffer with
/// exposed provenance) that remains valid for volatile 32-bit and 64-bit register reads and writes,
/// **and** the constructor caller attests that all register writes issued through this handle (and
/// any sub-banks derived from it) uphold SMMU hardware/DMA invariants so that no write causes
/// undefined behavior.
///
/// # Safety Design Note (Capability-Token Trust Boundary vs. Per-Write `unsafe fn`)
/// SMMU register writes use `regio::RwUnsafe` because misconfiguring DMA/translation registers can
/// violate Rust's memory model. We intentionally place the `unsafe` trust boundary at
/// [`SmmuMmio::new`] / [`SmmuMmio::with_size`] / [`SmmuMmio::from_mut_ptr`] - treating `SmmuMmio`
/// as a capability token whose holder is trusted to uphold SMMU register invariants - rather than
/// marking `write32`, `write64`, `write_to`, and `write_at` as `unsafe fn` at every register write
/// site.
///
/// **Tradeoff / Alternative**: Per-write `unsafe fn` (as in `arm_gicv3.rs` for `RwUnsafe`
/// registers) is the stricter Rust safety boundary because it requires each individual write site
/// to justify its register value locally instead of relying on handle-level trust. That stricter
/// alternative can be adopted in the future by marking `write32`, `write64`, `write_to`, and
/// `write_at` `unsafe fn` and moving the write-soundness precondition from `SmmuMmio` construction
/// onto each call site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::arm_smmu) struct SmmuMmio {
    base: usize,
    size: usize,
}

impl SmmuMmio {
    /// Returns a null (uninitialized/disabled) [`SmmuMmio`] handle (`base == 0`).
    #[inline]
    pub(in crate::arm_smmu) const fn null() -> Self {
        Self { base: 0, size: 0 }
    }

    /// Constructs an [`SmmuMmio`] handle for `base` with [`SMMU_MMIO_DEFAULT_BANK_SIZE`] bytes
    /// (or `0` bytes if `base == 0`).
    ///
    /// # Safety
    /// If `base != 0`:
    /// - `[base, base + SMMU_MMIO_DEFAULT_BANK_SIZE)` must be a valid, 8-byte-aligned mapped MMIO
    ///   region (or valid host test MMIO region / buffer with exposed provenance) for as long as
    ///   the returned handle or any sub-bank derived from it is used.
    /// - The caller must ensure that all register writes performed through the returned handle (or
    ///   any sub-bank derived from it) uphold SMMU hardware/DMA invariants and cannot cause
    ///   undefined behavior. (See the `# Safety Design Note` on [`SmmuMmio`].)
    #[inline]
    pub(in crate::arm_smmu) const unsafe fn new(base: usize) -> Self {
        assert!(base.is_multiple_of(align_of::<u64>()), "SmmuMmio base must be 8-byte aligned");
        let size = if base == 0 { 0 } else { SMMU_MMIO_DEFAULT_BANK_SIZE };
        Self { base, size }
    }

    /// Constructs an [`SmmuMmio`] handle for `base` with an explicit `size` in bytes (or `0` bytes
    /// if `base == 0`).
    ///
    /// # Safety
    /// If `base != 0`:
    /// - `[base, base + size)` must be a valid, 8-byte-aligned mapped MMIO region (or valid host
    ///   test MMIO region / buffer with exposed provenance) for as long as the returned handle or
    ///   any sub-bank derived from it is used.
    /// - The caller must ensure that all register writes performed through the returned handle (or
    ///   any sub-bank derived from it) uphold SMMU hardware/DMA invariants and cannot cause
    ///   undefined behavior. (See the `# Safety Design Note` on [`SmmuMmio`].)
    #[inline]
    pub(in crate::arm_smmu) const unsafe fn with_size(base: usize, size: usize) -> Self {
        assert!(base.is_multiple_of(align_of::<u64>()), "SmmuMmio base must be 8-byte aligned");
        let size = if base == 0 { 0 } else { size };
        Self { base, size }
    }

    /// Constructs an [`SmmuMmio`] handle backed by a mutable memory buffer `(buf, size)` (or
    /// returns [`Self::null()`] if `buf` is null), exposing `buf`'s pointer provenance for
    /// subsequent MMIO/buffer accesses.
    ///
    /// # Safety
    /// If `buf` is non-null:
    /// - `buf` must be 8-byte aligned and valid for reads and writes of `size` bytes (or a
    ///   registered host test MMIO region) with no aliasing violations for as long as the returned
    ///   handle or any sub-bank derived from it is used.
    /// - The caller must ensure that all register writes performed through the returned handle (or
    ///   any sub-bank derived from it) cannot cause undefined behavior. (See the `# Safety Design
    ///   Note` on [`SmmuMmio`].)
    #[allow(dead_code)]
    #[inline]
    pub(in crate::arm_smmu) unsafe fn from_mut_ptr<T>(buf: *mut T, size: usize) -> Self {
        // SAFETY: Caller upholds `Self::with_size`'s alignment, validity, lifetime, and
        // write-soundness requirements; `expose_provenance` exposes `buf`'s provenance for
        // `with_exposed_provenance*`.
        unsafe { Self::with_size(buf.expose_provenance(), size) }
    }

    /// Derives a bounds-checked sub-bank at `[offset, offset + size)` within `self`, or returns
    /// [`Self::null()`] if `self` is null.
    ///
    /// Panics if `self` is non-null and `[offset, offset + size)` exceeds `self.size` or is not
    /// 8-byte aligned.
    #[inline]
    pub(in crate::arm_smmu) const fn sub_bank(self, offset: usize, size: usize) -> Self {
        if self.base == 0 {
            Self::null()
        } else {
            assert!(offset <= self.size && size <= self.size - offset);
            let sub_base = self.base + offset;
            assert!(
                sub_base.is_multiple_of(align_of::<u64>()),
                "SmmuMmio sub-bank base must be 8-byte aligned"
            );
            Self { base: sub_base, size }
        }
    }

    /// Returns the virtual base address of this MMIO register bank (`0` if null).
    #[inline]
    pub(in crate::arm_smmu) const fn base(&self) -> usize {
        self.base
    }

    /// Returns the size in bytes of this MMIO register bank.
    #[inline]
    pub(in crate::arm_smmu) const fn size(&self) -> usize {
        self.size
    }

    /// Returns `true` if this MMIO handle is null (`base == 0`).
    #[inline]
    pub(in crate::arm_smmu) const fn is_null(&self) -> bool {
        self.base == 0
    }

    /// Reads a 32-bit register at `offset` within this MMIO bank.
    #[inline]
    pub(in crate::arm_smmu) fn read32(&self, offset: Offset<u32, RwUnsafe>) -> u32 {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u32>()),
            "MMIO read32 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, `Ro`
            // carries no `SafeWrite` claim, and `MmioBank::at` verifies alignment and bounds of
            // `offset`.
            unsafe {
                let ptr =
                    MmioPtr::<u32, Ro>::new(core::ptr::with_exposed_provenance::<u32>(self.base));
                let bank = MmioBank::new(ptr, self.size);
                bank.at(Offset::<u32, Ro>::new(offset.value)).read()
            }
        }
    }

    /// Writes a 32-bit register value `val` at `offset` within this MMIO bank.
    #[inline]
    pub(in crate::arm_smmu) fn write32(&self, offset: Offset<u32, RwUnsafe>, val: u32) {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u32>()),
            "MMIO write32 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant (established at
            // `SmmuMmio::{new, with_size, from_mut_ptr}`) and the assertion above, `self.base` is
            // a non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes,
            // `MmioPtr::<u32, RwUnsafe>` does not claim `SafeWrite`, `MmioBank::at` verifies
            // alignment and bounds of `offset`, and the constructor's `# Safety` contract
            // guarantees that writes issued through this handle uphold SMMU hardware/DMA
            // invariants and will not cause undefined behavior.
            unsafe {
                let ptr = MmioPtr::<u32, RwUnsafe>::new(core::ptr::with_exposed_provenance_mut::<
                    u32,
                >(self.base));
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).write(val);
            }
        }
    }

    /// Reads a 64-bit register at `offset` within this MMIO bank.
    #[inline]
    pub(in crate::arm_smmu) fn read64(&self, offset: Offset<u64, RwUnsafe>) -> u64 {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u64>()),
            "MMIO read64 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, `Ro`
            // carries no `SafeWrite` claim, and `MmioBank::at` verifies alignment and bounds of
            // `offset`.
            unsafe {
                let ptr =
                    MmioPtr::<u64, Ro>::new(core::ptr::with_exposed_provenance::<u64>(self.base));
                let bank = MmioBank::new(ptr, self.size);
                bank.at(Offset::<u64, Ro>::new(offset.value)).read()
            }
        }
    }

    /// Writes a 64-bit register value `val` at `offset` within this MMIO bank.
    #[inline]
    pub(in crate::arm_smmu) fn write64(&self, offset: Offset<u64, RwUnsafe>, val: u64) {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u64>()),
            "MMIO write64 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant (established at
            // `SmmuMmio::{new, with_size, from_mut_ptr}`) and the assertion above, `self.base` is
            // a non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes,
            // `MmioPtr::<u64, RwUnsafe>` does not claim `SafeWrite`, `MmioBank::at` verifies
            // alignment and bounds of `offset`, and the constructor's `# Safety` contract
            // guarantees that writes issued through this handle uphold SMMU hardware/DMA
            // invariants and will not cause undefined behavior.
            unsafe {
                let ptr = MmioPtr::<u64, RwUnsafe>::new(core::ptr::with_exposed_provenance_mut::<
                    u64,
                >(self.base));
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).write(val);
            }
        }
    }
}

/// Trait implemented by underlying register integer widths (`u32` and `u64`) for [`SmmuMmio`] I/O.
pub(in crate::arm_smmu) trait SmmuRegValue: Copy {
    /// Reads a raw register value at byte `offset` from `mmio`.
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self;

    /// Writes a raw register value `val` at byte `offset` to `mmio`.
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self);
}

impl SmmuRegValue for u32 {
    #[inline]
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self {
        mmio.read32(Offset::new(offset as usize))
    }

    #[inline]
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self) {
        mmio.write32(Offset::new(offset as usize), val);
    }
}

impl SmmuRegValue for u64 {
    #[inline]
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self {
        mmio.read64(Offset::new(offset as usize))
    }

    #[inline]
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self) {
        mmio.write64(Offset::new(offset as usize), val);
    }
}

/// Metadata describing a single register bitfield for debug console introspection (`DumpReg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    /// Display name of the bitfield (empty `""` for `rsvdz` fields, `"__res..."` for non-zeroed
    /// reserved fields).
    pub name: &'static str,
    /// Most significant bit index (inclusive).
    pub msb: u32,
    /// Least significant bit index (inclusive).
    pub lsb: u32,
}

impl FieldDesc {
    /// Extracts this field's shifted unsigned value from `raw_reg_val`.
    #[inline]
    pub const fn extract(&self, raw_reg_val: u64) -> u64 {
        let width = (self.msb - self.lsb) + 1;
        let mask = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
        (raw_reg_val >> self.lsb) & mask
    }
}

/// Declarative macro for defining ARM SMMU MMIO registers, bitfields, RSVDZ masks, and console
/// field metadata tables.
///
/// # Overview
///
/// `define_smmu_reg!` generates a `#[repr(transparent)]` wrapper struct over `u32` or `u64` that
/// implements `LayoutOver` (`Copy + From<Base> + Into<Base>`), typed bitfield getters and
/// chainable `&mut Self` setters, reserved-as-zero (`[rsvdz]`) write masking, and a static
/// `FIELDS: &'static [FieldDesc]` slice used by the `smmuv2 dumpregs` / `smmuv2 show -v` kernel
/// console commands.
///
/// Three definition forms are supported:
/// 1. **Fixed-offset MMIO registers**: `pub struct RegName($val_ty) @ $offset_expr => { ... }`
/// 2. **Indexed MMIO register arrays**:
///    `pub struct RegName($val_ty) @ [$ndx:ident : u32] $offset_expr => { ... }`
/// 3. **Non-MMIO bitfield values**: `pub struct Name($val_ty) => { ... }` (for doublewords of
///    in-memory SMMUv3 structures; omits offsets and MMIO read/write methods).
///
/// Four field kinds are supported inside the `{ ... }` body:
/// 1. **Integer bit or multi-bit field**:
///    `msb:lsb => getter_name, set_name ["DISPLAY_NAME"]: u32;` (or `u64;`).
/// 2. **Boolean single-bit flag**:
///    `bit:bit => getter_name, set_name ["DISPLAY_NAME"]: bool;`.
/// 3. **Typed enum field**:
///    `msb:lsb => getter_name, set_name ["DISPLAY_NAME"]: enum EnumType;` (where `EnumType`
///    provides `const fn from_raw(u32) -> EnumType` and `#[repr(u32)]`).
/// 4. **Reserved-as-zero (`[rsvdz]`) field**:
///    `msb:lsb => [rsvdz];`. No getter or setter is emitted; the bit range is accumulated into
///    `Self::RSVDZ_MASK` and automatically cleared before every `write_to` / `write_at` MMIO
///    store, and its [`FieldDesc`] entry carries an empty `name: ""` so `DumpReg` omits it.
///
/// # Concrete Examples
///
/// ## Example 1: Fixed-offset register with integer, boolean, enum, and `[rsvdz]` fields
///
/// ```ignore
/// define_smmu_reg! {
///     /// Global Configuration Register 2 (`SMMU_sCR2`).
///     pub struct Cr2(u32) @ 0x008 => {
///         31:31 => exsmrgenable, set_exsmrgenable ["EXSMRGENABLE"]: u32;
///         30:30 => [rsvdz];
///         29:29 => compindexenable, set_compindexenable ["COMPINDEXENABLE"]: u32;
///         26:16 => [rsvdz];
///         7:0   => bpvmid, set_bpvmid ["BPVMID"]: u32;
///     }
/// }
/// ```
///
/// This expands to:
/// - `#[repr(transparent)] pub struct Cr2(u32);` implementing `Clone`, `Copy`, `Debug`, `Default`,
///   `PartialEq`, `Eq`, `From<u32>`, and `From<Cr2> for u32`.
/// - `pub const OFFSET: u32 = 0x008;` and
///   `pub const REGIO_OFFSET: Offset<Self, RwUnsafe> = Offset::new(0x008);`
/// - `pub const RSVDZ_MASK: u32 = (1 << 30) | (0x7ff << 16);`
/// - `pub const FIELDS: &'static [FieldDesc] = &[ ... ];`
/// - `pub const fn from_value(val: u32) -> Self` and `pub const fn reg_value(&self) -> u32`
/// - `pub(in crate::arm_smmu) fn read_from(mmio: &SmmuMmio) -> Self` and
///   `pub(in crate::arm_smmu) fn write_to(&mut self, mmio: &SmmuMmio) -> &mut Self` (clearing
///   `RSVDZ_MASK` before writing to `mmio`).
/// - `pub const fn exsmrgenable(&self) -> u32`,
///   `pub fn set_exsmrgenable(&mut self, val: u32) -> &mut Self`, etc.
///
/// ## Example 2: Indexed register array (`SMRn`, `S2CRn`, `CBARn`) with typed enum field
///
/// ```ignore
/// define_smmu_reg! {
///     /// Stream-to-Context Register (`SMMU_S2CRn`).
///     pub struct S2cr(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
///         17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
///     }
/// }
/// ```
///
/// This expands to:
/// - `pub const fn offset(ndx: u32) -> u32 { 0xc00 + (ndx << 2) }` and
///   `pub const fn regio_offset(ndx: u32) -> Offset<Self, RwUnsafe>`
/// - `pub(in crate::arm_smmu) fn read_at(mmio: &SmmuMmio, ndx: u32) -> Self` and
///   `pub(in crate::arm_smmu) fn write_at(&mut self, mmio: &SmmuMmio, ndx: u32) -> &mut Self`
/// - `pub const fn s2cr_type(&self) -> S2crType` and
///   `pub fn set_s2cr_type(&mut self, val: S2crType) -> &mut Self`.
macro_rules! define_smmu_reg {
    // =========================================================================
    // Arm 1: Indexed register array definition (`@ [$ndx:ident : u32] $offset:expr`)
    // =========================================================================
    // Matches a register family indexed by `ndx: u32` (such as `SMRn`, `S2CRn`, `CBARn`,
    // `CBFRSYNRAn`, `CBA2Rn`). Generates `offset(ndx)`, `regio_offset(ndx)`, `read_at(mmio, ndx)`,
    // and `write_at(mmio, ndx)` in place of a fixed `OFFSET`.
    (
        $(#[$doc:meta])*
        $vis:vis struct $name:ident ($val_ty:ty) @ [$ndx:ident : u32] $offset:expr => {
            $(
                $msb:literal : $lsb:literal =>
                    $( [$rsvdz:ident] )?
                    $( $get:ident, $set:ident [$disp:literal] : $($field_ty:ident)+ )?
            );* $(;)?
        }
    ) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name($val_ty);

        impl From<$val_ty> for $name {
            #[inline]
            fn from(val: $val_ty) -> Self {
                Self(val)
            }
        }

        impl From<$name> for $val_ty {
            #[inline]
            fn from(reg: $name) -> Self {
                reg.0
            }
        }

        impl $name {
            /// Computes the byte offset of element `$ndx` within its MMIO bank.
            #[inline]
            pub const fn offset($ndx: u32) -> u32 {
                $offset
            }

            /// Computes the typed [`Offset`] of element `ndx` within its MMIO bank.
            #[inline]
            pub const fn regio_offset(ndx: u32) -> Offset<Self, RwUnsafe> {
                Offset::new(Self::offset(ndx) as usize)
            }

            /// Bitmask of reserved-as-zero (`[rsvdz]`) bit positions that are automatically
            /// cleared before writing this register to MMIO.
            pub const RSVDZ_MASK: $val_ty = 0 $(
                | define_smmu_reg!(
                    @rsvdz_mask $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                )
            )*;

            /// Static bitfield descriptor table used by `DumpReg` in the kernel debug console.
            pub const FIELDS: &'static [FieldDesc] = &[
                $(
                    define_smmu_reg!(
                        @field_desc $msb, $lsb,
                        $( [$rsvdz] )?
                        $( $get, $set [$disp] : $($field_ty)+ )?
                    ),
                )*
            ];

            /// Constructs a register value wrapper from a raw integer `val`.
            #[inline]
            pub const fn from_value(val: $val_ty) -> Self {
                Self(val)
            }

            /// Returns the raw integer value stored in this register snapshot.
            #[inline]
            pub const fn reg_value(&self) -> $val_ty {
                self.0
            }

            /// Replaces the raw integer value stored in this register snapshot.
            #[inline]
            pub fn set_reg_value(&mut self, val: $val_ty) -> &mut Self {
                self.0 = val;
                self
            }

            /// Reads element `ndx` of this register array from `mmio`.
            #[inline]
            pub(in crate::arm_smmu) fn read_at(mmio: &SmmuMmio, ndx: u32) -> Self {
                Self(<$val_ty as SmmuRegValue>::read_mmio(mmio, Self::offset(ndx)))
            }

            /// Clears any [`Self::RSVDZ_MASK`] bits and writes element `ndx` of this register
            /// array to `mmio`.
            #[inline]
            pub(in crate::arm_smmu) fn write_at(&mut self, mmio: &SmmuMmio, ndx: u32) -> &mut Self {
                let masked = self.0 & !Self::RSVDZ_MASK;
                <$val_ty as SmmuRegValue>::write_mmio(mmio, Self::offset(ndx), masked);
                self
            }

            $(
                define_smmu_reg!(
                    @field_methods $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                );
            )*
        }
    };

    // =========================================================================
    // Arm 2: Fixed-offset register definition (`@ $offset:expr`)
    // =========================================================================
    // Generates the `#[repr(transparent)]` wrapper type, `From`/`Into` conversions required by
    // `LayoutOver`, `OFFSET` and `REGIO_OFFSET` constants, `RSVDZ_MASK`, `FIELDS` metadata
    // slice, `read_from` / `write_to` MMIO methods, and all bitfield getters/setters.
    (
        $(#[$doc:meta])*
        $vis:vis struct $name:ident ($val_ty:ty) @ $offset:expr => {
            $(
                $msb:literal : $lsb:literal =>
                    $( [$rsvdz:ident] )?
                    $( $get:ident, $set:ident [$disp:literal] : $($field_ty:ident)+ )?
            );* $(;)?
        }
    ) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name($val_ty);

        impl From<$val_ty> for $name {
            #[inline]
            fn from(val: $val_ty) -> Self {
                Self(val)
            }
        }

        impl From<$name> for $val_ty {
            #[inline]
            fn from(reg: $name) -> Self {
                reg.0
            }
        }

        impl $name {
            /// Byte offset of this register within its MMIO bank.
            pub const OFFSET: u32 = $offset;

            /// Typed [`Offset`] of this register within its MMIO bank.
            pub const REGIO_OFFSET: Offset<Self, RwUnsafe> = Offset::new(Self::OFFSET as usize);

            /// Bitmask of reserved-as-zero (`[rsvdz]`) bit positions that are automatically
            /// cleared before writing this register to MMIO.
            pub const RSVDZ_MASK: $val_ty = 0 $(
                | define_smmu_reg!(
                    @rsvdz_mask $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                )
            )*;

            /// Static bitfield descriptor table used by `DumpReg` in the kernel debug console.
            pub const FIELDS: &'static [FieldDesc] = &[
                $(
                    define_smmu_reg!(
                        @field_desc $msb, $lsb,
                        $( [$rsvdz] )?
                        $( $get, $set [$disp] : $($field_ty)+ )?
                    ),
                )*
            ];

            /// Constructs a register value wrapper from a raw integer `val`.
            #[inline]
            pub const fn from_value(val: $val_ty) -> Self {
                Self(val)
            }

            /// Returns the raw integer value stored in this register snapshot.
            #[inline]
            pub const fn reg_value(&self) -> $val_ty {
                self.0
            }

            /// Replaces the raw integer value stored in this register snapshot.
            #[inline]
            pub fn set_reg_value(&mut self, val: $val_ty) -> &mut Self {
                self.0 = val;
                self
            }

            /// Returns the byte offset (`reg_addr`) of this register within its MMIO bank.
            #[inline]
            pub const fn reg_addr(&self) -> u32 {
                Self::OFFSET
            }

            /// Reads this register from `mmio` at [`Self::OFFSET`].
            #[inline]
            pub(in crate::arm_smmu) fn read_from(mmio: &SmmuMmio) -> Self {
                Self(<$val_ty as SmmuRegValue>::read_mmio(mmio, Self::OFFSET))
            }

            /// Clears any [`Self::RSVDZ_MASK`] bits and writes this register to `mmio` at
            /// [`Self::OFFSET`].
            #[inline]
            pub(in crate::arm_smmu) fn write_to(&mut self, mmio: &SmmuMmio) -> &mut Self {
                let masked = self.0 & !Self::RSVDZ_MASK;
                <$val_ty as SmmuRegValue>::write_mmio(mmio, Self::OFFSET, masked);
                self
            }

            $(
                define_smmu_reg!(
                    @field_methods $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                );
            )*
        }
    };

    // =========================================================================
    // Arm 3: Bitfield value with no MMIO location (`struct Name(u64) => { ... }`)
    // =========================================================================
    // Matches a doubleword of an in-memory structure (SMMUv3 stream table entries, context
    // descriptors, queue entries). Generates the same value type, `RSVDZ_MASK`, `FIELDS`, and
    // field accessors as a register, but no offset and no MMIO read/write methods: such values
    // are stored through `v3::structures::store_dw`.
    (
        $(#[$doc:meta])*
        $vis:vis struct $name:ident ($val_ty:ty) => {
            $(
                $msb:literal : $lsb:literal =>
                    $( [$rsvdz:ident] )?
                    $( $get:ident, $set:ident [$disp:literal] : $($field_ty:ident)+ )?
            );* $(;)?
        }
    ) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name($val_ty);

        impl From<$val_ty> for $name {
            #[inline]
            fn from(val: $val_ty) -> Self {
                Self(val)
            }
        }

        impl From<$name> for $val_ty {
            #[inline]
            fn from(reg: $name) -> Self {
                reg.0
            }
        }

        impl $name {
            pub const RSVDZ_MASK: $val_ty = 0 $(
                | $crate::arm_smmu::common::mmio::define_smmu_reg!(
                    @rsvdz_mask $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                )
            )*;

            pub const FIELDS: &'static [$crate::arm_smmu::common::mmio::FieldDesc] = &[
                $(
                    $crate::arm_smmu::common::mmio::define_smmu_reg!(
                        @field_desc $msb, $lsb,
                        $( [$rsvdz] )?
                        $( $get, $set [$disp] : $($field_ty)+ )?
                    ),
                )*
            ];

            #[inline]
            pub const fn from_value(val: $val_ty) -> Self {
                Self(val)
            }

            #[inline]
            pub const fn reg_value(&self) -> $val_ty {
                self.0
            }

            #[inline]
            pub fn set_reg_value(&mut self, val: $val_ty) -> &mut Self {
                self.0 = val;
                self
            }

            $(
                $crate::arm_smmu::common::mmio::define_smmu_reg!(
                    @field_methods $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                );
            )*
        }
    };

    // =========================================================================
    // Internal Helper Rules: `@bit_mask`
    // =========================================================================
    // Computes the unshifted bitmask `((1 << (msb - lsb + 1)) - 1)` for an inclusive bit range
    // `[msb:lsb]` in `$val_ty` (`u32` or `u64`) at compile time without overflow when the field
    // spans the full register width (`width == $val_ty::BITS`).
    (@bit_mask $val_ty:ty, $msb:literal, $lsb:literal) => {{
        assert!($msb >= $lsb, "Upper bit must be >= lower bit");
        assert!($msb < <$val_ty>::BITS, "Upper bit exceeds register width");
        let width: u32 = ($msb - $lsb) + 1;
        if width == <$val_ty>::BITS {
            <$val_ty>::MAX
        } else {
            ((1 as $val_ty) << width) - 1
        }
    }};

    // =========================================================================
    // Internal Helper Rules: `@rsvdz_mask`
    // =========================================================================
    // Evaluates to the shifted bitmask `MASK << lsb` for `[rsvdz]` fields, and `0` for all
    // normal readable/writable fields.
    (@rsvdz_mask $val_ty:ty, $msb:literal, $lsb:literal, [rsvdz]) => {
        $crate::arm_smmu::common::mmio::define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb) << $lsb
    };
    (@rsvdz_mask $val_ty:ty, $msb:literal, $lsb:literal, $($other:tt)+) => {
        0 as $val_ty
    };

    // =========================================================================
    // Internal Helper Rules: `@field_desc`
    // =========================================================================
    // Builds a `FieldDesc` entry for `Self::FIELDS`. Reserved-zero (`[rsvdz]`) fields receive an
    // empty `name: ""` so `DumpReg` skips them during both width calculation and field printing,
    // matching `hwreg`'s `nullptr` name behavior.
    (@field_desc $msb:literal, $lsb:literal, [rsvdz]) => {
        $crate::arm_smmu::common::mmio::FieldDesc { name: "", msb: $msb, lsb: $lsb }
    };
    (
        @field_desc
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : $($rest:tt)+
    ) => {
        $crate::arm_smmu::common::mmio::FieldDesc { name: $disp, msb: $msb, lsb: $lsb }
    };

    // =========================================================================
    // Internal Helper Rules: `@field_methods`
    // =========================================================================
    // Generates typed getter and setter methods for each field kind:
    // 1. `[rsvdz]`: no getter/setter (`RSVDZ_MASK` already validates the bit range).
    // 2. `: bool`: emits `pub const fn $get(&self) -> bool` and
    //    `pub fn $set(&mut self, val: bool) -> &mut Self`.
    // 3. `: enum $enum_ty`: emits `pub const fn $get(&self) -> $enum_ty` (via `$enum_ty::from_raw`)
    //    and `pub fn $set(&mut self, val: $enum_ty) -> &mut Self`.
    // 4. `: $field_ty` (`u32` or `u64`): emits `pub const fn $get(&self) -> $val_ty` and
    //    `pub fn $set(&mut self, val: $val_ty) -> &mut Self` with a `debug_assert!` verifying that
    //    `val` fits within the field width.
    (@field_methods $val_ty:ty, $msb:literal, $lsb:literal, [rsvdz]) => {};
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : bool
    ) => {
        /// Extracts boolean bitfield `$disp`.
        #[inline]
        pub const fn $get(&self) -> bool {
            const _ASSERT_SINGLE_BIT: () =
                assert!($msb == $lsb, "Boolean register field must be a single bit");
            const _ASSERT_IN_BOUNDS: () =
                assert!($msb < <$val_ty>::BITS, "Bit index exceeds register width");
            ((self.0 >> $lsb) & 1) != 0
        }

        /// Updates boolean bitfield `$disp` in place.
        #[inline]
        pub fn $set(&mut self, val: bool) -> &mut Self {
            let bit: $val_ty = (1 as $val_ty) << $lsb;
            if val {
                self.0 |= bit;
            } else {
                self.0 &= !bit;
            }
            self
        }
    };
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : enum $enum_ty:ty
    ) => {
        /// Extracts typed enum bitfield `$disp`.
        #[inline]
        pub const fn $get(&self) -> $enum_ty {
            const MASK: $val_ty =
                $crate::arm_smmu::common::mmio::define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            let raw = (self.0 >> $lsb) & MASK;
            <$enum_ty>::from_raw(raw as u32)
        }

        /// Updates typed enum bitfield `$disp` in place.
        #[inline]
        pub fn $set(&mut self, val: $enum_ty) -> &mut Self {
            const MASK: $val_ty =
                $crate::arm_smmu::common::mmio::define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            let raw = (val as u32) as $val_ty;
            // A value too wide for the field is a caller bug, caught here in builds with debug
            // assertions. In other builds it is masked to the width of the field, so the wrong
            // value stays in this field and cannot spill into the fields above it.
            debug_assert!(raw.checked_shr(($msb - $lsb) + 1).unwrap_or(0) == 0);
            self.0 = (self.0 & !(MASK << $lsb)) | ((raw & MASK) << $lsb);
            self
        }
    };
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : $field_ty:ty
    ) => {
        /// Extracts integer bitfield `$disp` (`bits [$msb:$lsb]`).
        #[inline]
        pub const fn $get(&self) -> $val_ty {
            const MASK: $val_ty =
                $crate::arm_smmu::common::mmio::define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            (self.0 >> $lsb) & MASK
        }

        /// Updates integer bitfield `$disp` (`bits [$msb:$lsb]`) in place.
        #[inline]
        pub fn $set(&mut self, val: $val_ty) -> &mut Self {
            const MASK: $val_ty =
                $crate::arm_smmu::common::mmio::define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            // A value too wide for the field is a caller bug, caught here in builds with debug
            // assertions. In other builds it is masked to the width of the field, so the wrong
            // value stays in this field and cannot spill into the fields above it.
            debug_assert!(val.checked_shr(($msb - $lsb) + 1).unwrap_or(0) == 0);
            self.0 = (self.0 & !(MASK << $lsb)) | ((val & MASK) << $lsb);
            self
        }
    };
}

pub(crate) use define_smmu_reg;

/// Formats and prints a register and its bitfields to the kernel console.
pub(in crate::arm_smmu) fn dump_reg(
    tag: &str,
    raw_val: u64,
    byte_width: usize,
    reg_offset: u32,
    fields: &[FieldDesc],
    base: &SmmuMmio,
    ilvl: usize,
) {
    let paddr = SmmuEnv::periph_vaddr_to_paddr(base.base()).wrapping_add(reg_offset as u64);
    dump_bitfields(tag, paddr, raw_val, byte_width, fields, ilvl);
}

/// Formats and prints a value of `byte_width` bytes found at physical address `paddr`, and its
/// bitfields, to the kernel console. The value may be a register or a doubleword of an in-memory
/// structure.
pub(crate) fn dump_bitfields(
    tag: &str,
    paddr: u64,
    raw_val: u64,
    byte_width: usize,
    fields: &[FieldDesc],
    ilvl: usize,
) {
    let indent = ilvl * 2;
    dprintf!(ALWAYS, "{:indent$}{} : [{:#x}]\n", "", tag, paddr, indent = indent);

    // Size the name column from the fields which are printed below. RSVDZ fields (which have no
    // name) and the `__resN` reserved fields are not printed, so they do not widen the column.
    let mut max_name_width = "raw".len();
    for f in fields {
        if !f.name.is_empty() && !f.name.starts_with("__res") {
            max_name_width = max_name_width.max(f.name.len());
        }
    }

    let msb = ((byte_width * 8) - 1) as u32;
    if byte_width == 8 {
        dprintf!(
            ALWAYS,
            "{:indent$}[{:2}:{:2}] : {:>width$} : {:#018x} ({})\n",
            "",
            msb,
            0,
            "raw",
            raw_val,
            raw_val,
            indent = indent,
            width = max_name_width
        );
    } else {
        dprintf!(
            ALWAYS,
            "{:indent$}[{:2}:{:2}] : {:>width$} : {:#010x} ({})\n",
            "",
            msb,
            0,
            "raw",
            raw_val as u32,
            raw_val as u32,
            indent = indent,
            width = max_name_width
        );
    }

    for f in fields {
        if !f.name.is_empty() && !f.name.starts_with("__res") {
            let val = f.extract(raw_val);
            if byte_width == 8 {
                dprintf!(
                    ALWAYS,
                    "{:indent$}[{:2}:{:2}] : {:>width$} : {:#018x} ({})\n",
                    "",
                    f.msb,
                    f.lsb,
                    f.name,
                    val,
                    val,
                    indent = indent,
                    width = max_name_width
                );
            } else {
                dprintf!(
                    ALWAYS,
                    "{:indent$}[{:2}:{:2}] : {:>width$} : {:#010x} ({})\n",
                    "",
                    f.msb,
                    f.lsb,
                    f.name,
                    val as u32,
                    val as u32,
                    indent = indent,
                    width = max_name_width
                );
            }
        }
    }
    dprintf!(ALWAYS, "\n");
}
