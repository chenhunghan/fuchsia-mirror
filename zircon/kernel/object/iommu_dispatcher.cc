// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/iommu_dispatcher.h"

#include <lib/object-constants.h>

IommuDispatcher::IommuDispatcher(const void* iommu) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(IommuDispatcher, kIommuDispatcherStateOffset);
  rust_iommu_dispatcher_state_init(&opaque_storage_, this, iommu);
}

IMPLEMENT_DISPATCHER_RUST_STATE(IommuDispatcher, rust_iommu_dispatcher_state_get_lock,
                                rust_iommu_dispatcher_state_destroy)
