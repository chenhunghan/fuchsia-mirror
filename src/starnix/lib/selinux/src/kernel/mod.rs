// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Starnix kernel object class and permission definitions, policy-to-kernel index, and access
//! vector cache (AVC).

mod avc;
mod index;
mod permissions;

pub use avc::{
    AccessCacheStorage, AccessQueryArgs, CacheStats, ConcurrentAccessCache, DEFAULT_SHARED_SIZE,
    PerThreadCache,
};
pub(crate) use avc::{AccessVectorCache, Query};
pub(crate) use index::PolicyIndex;
pub use permissions::*;

use crate::policy::{AccessVector, ClassId, XpermsBitmap};
use fuchsia_rcu::RcuDroppable;
use std::num::NonZeroU32;

/// Identifies a specific class by its policy-defined Id, or as a kernel object class enum Id.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, RcuDroppable)]
pub(crate) enum ObjectClass {
    /// Refers to a well-known SELinux [`KernelClass`] (e.g. "process", "file", "capability").
    Kernel(KernelClass),
    /// Refers to a policy-defined class by its policy-defined numeric [`ClassId`]. This is most
    /// commonly used when handling queries from userspace, which refer to classes by ID.
    ClassId(ClassId),
}

impl From<ClassId> for ObjectClass {
    fn from(id: ClassId) -> Self {
        Self::ClassId(id)
    }
}

impl<T: Into<KernelClass>> From<T> for ObjectClass {
    fn from(class: T) -> Self {
        Self::Kernel(class.into())
    }
}

/// Access decision translated into kernel permission bit positions for a [`KernelClass`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelAccessDecision {
    /// Set of kernel permissions that are allowed.
    pub allow: AccessVector,
    /// Set of kernel permissions that should be audited when checked (as `auditallow` if the
    /// corresponding bit in `allow` is set, or `auditdeny` if it is cleared).
    pub audit: AccessVector,
    /// Flags associated with the access decision (e.g. `SELINUX_AVD_FLAGS_PERMISSIVE`).
    pub flags: u32,
    /// Bug ID to include in audit logs if denials are permitted via an exception rule.
    pub todo_bug: Option<NonZeroU32>,
}

impl KernelAccessDecision {
    /// Access decision that allows all permissions without auditing.
    pub const ALLOW_ALL: Self =
        Self { allow: AccessVector::ALL, audit: AccessVector::NONE, flags: 0, todo_bug: None };

    /// Access decision that denies all permissions and audits all denials.
    pub const DENY_ALL: Self =
        Self { allow: AccessVector::NONE, audit: AccessVector::ALL, flags: 0, todo_bug: None };
}

/// Extended permission access decision as seen from the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct KernelXpermsAccessDecision {
    /// Set of xperms that are allowed.
    pub allow: XpermsBitmap,
    /// Set of xperms that should be audited (as allowed or denials depending on `allow`).
    pub audit: XpermsBitmap,
    /// Whether the domain is permissive.
    pub permissive: bool,
    /// Whether the entry has an associated todo.
    pub has_todo: bool,
}

impl KernelXpermsAccessDecision {
    /// Extended permission decision that allows all extended permissions without auditing.
    pub(crate) const ALLOW_ALL: Self = Self {
        allow: XpermsBitmap::ALL,
        audit: XpermsBitmap::NONE,
        permissive: false,
        has_todo: false,
    };

    /// Extended permission decision that denies all extended permissions and audits all denials.
    #[cfg(test)]
    pub(crate) const DENY_ALL: Self = Self {
        allow: XpermsBitmap::NONE,
        audit: XpermsBitmap::ALL,
        permissive: false,
        has_todo: false,
    };
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_class_permissions() {
        let test_class_id = ClassId::for_test(20);
        assert_eq!(ObjectClass::ClassId(test_class_id), test_class_id.into());
        for variant in KernelClass::Process.permissions() {
            assert_eq!(KernelClass::Process, variant.class());
            assert_eq!("process", variant.class().name());
            assert_eq!(ObjectClass::Kernel(KernelClass::Process), variant.class().into());
        }
    }

    #[test]
    fn kernel_class_from_name() {
        assert_eq!(KernelClass::from_name("process"), Some(KernelClass::Process));
        assert_eq!(KernelClass::from_name("file"), Some(KernelClass::File));
        assert_eq!(KernelClass::from_name("socket"), Some(KernelClass::Socket));
        assert_eq!(KernelClass::from_name("non_existent_class"), None);
    }
}
