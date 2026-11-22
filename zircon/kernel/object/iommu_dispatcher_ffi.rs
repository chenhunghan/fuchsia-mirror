// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::dispatcher::impl_dispatcher_state_init;
use super::iommu_dispatcher::{IommuDispatcher, IommuDispatcherState};
use crate::iommu::Iommu;
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Allocates and initializes a new `IommuDispatcher` via C++ `KernelHandle`, writing the
    /// resulting handle to `handle_out` on `ZX_OK`.
    ///
    /// # Safety
    ///
    /// `iommu` must be a valid reference for the duration of the call, and `handle_out` must
    /// point to valid, aligned, writable uninitialized storage.
    pub(crate) fn cpp_iommu_dispatcher_create(
        iommu: &Iommu,
        handle_out: *mut MaybeUninit<KernelHandle<IommuDispatcher>>,
    ) -> zx_status_t;
}

impl_dispatcher_state_init!(
    IommuDispatcher,
    IommuDispatcherState,
    iommu: &Iommu,
);
