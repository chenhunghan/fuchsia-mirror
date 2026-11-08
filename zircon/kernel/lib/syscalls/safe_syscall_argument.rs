// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Function arguments of types narrower than a 64-bit register are passed in a
//! full 64-bit register or a full 64-bit stack slot. The machine calling
//! conventions either say that the high bits are unspecified, or that the
//! narrow integer value is zero-extended or sign-extended as appropriate to its
//! type or thereabouts, or doesn't specify it clearly.
//!
//! * The aarch64 psABI says high bits are unspecified so the compiler is
//!   obliged to ignore them.
//!
//! * The x86-64 psABI doesn't clearly specify it (except that bool values are
//!   zero-extended from 1 bit to 8 bits); in observed fact, compilers do
//!   sometimes assume incoming register values have no excess high bits set.
//!
//! * The RISC-V psABI says that values of types narrower than 32 bits are
//!   extended as appropriate for their type to 32 bits, and then (they and
//!   original 32-bit values) are zero-extended to 64 bits.
//!
//! Even in a case like aarch64 where the compiler is unambiguously obliged to
//! ignore the high bits rather than assume they have been correctly zeroed or
//! sign-extended, the kernel should not trust the compiler not to slip up here,
//! nor otherwise allow unintended input noise from the user to have any effect
//! whatsoever on the kernel (ideally, not even littering the spilled values in
//! kernel stack frames differently in case those are accessible via exploits).
//!
//! So this trait handles sanitizing the user argument values into Rust values
//! that are safe to trust the compiler with.

use crate::object::HandleValue;
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use zx_types::zx_handle_t;

/// A syscall argument type that can be sanitized from the raw 64-bit register value the
/// generated entry-point wrappers receive from user mode.
pub(super) trait SafeSyscallArgument {
    /// This returns a safely valid (if not trustworthy) value of type `Self` by
    /// sanitizing the incoming value from the user of any excess high bits.
    /// This is a direct user value of type `Self` and not to be considered "safe" as
    /// an input value, but it is safely actually of type `Self` by Rust semantics
    /// rather than potentially having undefined behavior at the language level.
    fn sanitize(value: u64) -> Self;
}

/// Implements [`SafeSyscallArgument`] for integer types narrower than the 64-bit register.
macro_rules! impl_narrow_integer_argument {
    ($($ty:ty),* $(,)?) => {
        $(
            impl SafeSyscallArgument for $ty {
                #[inline(always)]
                fn sanitize(value: u64) -> Self {
                    // This will just truncate the high bits so they are sign- or
                    // zero-extended from the narrower integer type in the low bits.
                    value as Self
                }
            }
        )*
    };
}

impl_narrow_integer_argument!(u8, u16, u32, i8, i16, i32);

impl SafeSyscallArgument for u64 {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        value
    }
}

impl SafeSyscallArgument for i64 {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        value as Self
    }
}

// The register is the width of a pointer, so `usize` and `isize` are used as is.
zr::static_assert!(size_of::<usize>() == size_of::<u64>());
zr::static_assert!(size_of::<isize>() == size_of::<i64>());

impl SafeSyscallArgument for usize {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        value as Self
    }
}

impl SafeSyscallArgument for isize {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        value as Self
    }
}

/// `bool` arguments are not currently used, but this would make them safe.
/// (They must still be excluded from struct layouts used via copy-in.)
impl SafeSyscallArgument for bool {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        value & 1 != 0
    }
}

impl SafeSyscallArgument for HandleValue {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        HandleValue::new(zx_handle_t::sanitize(value))
    }
}

impl<T> SafeSyscallArgument for UserInPtr<T> {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        UserInPtr::new(core::ptr::with_exposed_provenance(usize::sanitize(value)))
    }
}

impl<T> SafeSyscallArgument for UserOutPtr<T> {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        UserOutPtr::new(core::ptr::with_exposed_provenance_mut(usize::sanitize(value)))
    }
}

impl<T> SafeSyscallArgument for UserInOutPtr<T> {
    #[inline(always)]
    fn sanitize(value: u64) -> Self {
        UserInOutPtr::new(core::ptr::with_exposed_provenance_mut(usize::sanitize(value)))
    }
}
