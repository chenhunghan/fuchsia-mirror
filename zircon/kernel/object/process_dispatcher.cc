// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/process_dispatcher.h"

#include <assert.h>
#include <lib/object-constants.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>

#include <fbl/ref_ptr.h>
#include <object/dispatcher.h>
#include <object/job_dispatcher.h>
#include <object/vm_address_region_dispatcher.h>

#define LOCAL_TRACE 0

Lock<CriticalMutex>* ProcessDispatcher::get_lock() const {
  return rust_process_dispatcher_state_get_lock(&opaque_storage_);
}

ProcessDispatcher::~ProcessDispatcher() {
  LTRACE_ENTRY_OBJ;

  // Remove ourselves from the parent job's raw ref to us. Note that this might
  // have been called when transitioning to ProcessState::Dead. The Job can handle double calls.
  job()->RemoveChildProcess(this);
  rust_process_dispatcher_state_destroy(&opaque_storage_);

  LTRACE_EXIT_OBJ;
}

// static
zx_status_t ProcessDispatcher::Create(fbl::RefPtr<JobDispatcher> job, ktl::string_view name,
                                      uint32_t flags, KernelHandle<ProcessDispatcher>* handle,
                                      zx_rights_t* rights,
                                      KernelHandle<VmAddressRegionDispatcher>* root_vmar_handle,
                                      zx_rights_t* root_vmar_rights) {
  ffi::Uninitialized<KernelHandle<ProcessDispatcher>> new_handle;
  ffi::Uninitialized<zx_rights_t> new_rights;
  ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>> new_vmar_handle;
  ffi::Uninitialized<zx_rights_t> new_vmar_rights;
  zx_status_t status =
      rust_process_dispatcher_create(fbl::ExportToRawPtr(&job), name.data(), name.size(), flags,
                                     &new_handle, &new_rights, &new_vmar_handle, &new_vmar_rights);
  if (status == ZX_OK) {
    *handle = ktl::move(new_handle.Get());
    *rights = new_rights.Get();
    *root_vmar_handle = ktl::move(new_vmar_handle.Get());
    *root_vmar_rights = new_vmar_rights.Get();
  }
  return status;
}

ProcessDispatcher::ProcessDispatcher(fbl::RefPtr<ShareableProcessState> shareable_state,
                                     fbl::RefPtr<JobDispatcher> job, ktl::string_view name,
                                     uint32_t flags)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(ProcessDispatcher, kProcessDispatcherStateOffset);
  rust_process_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&shareable_state),
                                     fbl::ExportToRawPtr(&job), name.data(), name.length(), flags);
}

fbl::RefPtr<JobDispatcher> ProcessDispatcher::job() const {
  return fbl::ImportFromRawPtr(rust_process_dispatcher_job(this));
}
