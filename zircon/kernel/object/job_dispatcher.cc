// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/job_dispatcher.h"

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/mutex.h>
#include <object/dispatcher.h>

#include <ktl/enforce.h>

JobDispatcher::JobDispatcher(fbl::RefPtr<JobDispatcher> parent, const JobPolicy* policy)
    : Dispatcher(ZX_JOB_NO_PROCESSES | ZX_JOB_NO_JOBS | ZX_JOB_NO_CHILDREN) {
  DISPATCHER_VERIFY_OFFSET(JobDispatcher, kJobDispatcherStateOffset);
  rust_job_dispatcher_state_init(&opaque_storage_, fbl::ExportToRawPtr(&parent), policy);
}

JobDispatcher::~JobDispatcher() {
  // Leave the job tree while this job is still a complete object: until it is unlinked from the
  // parent's child list, a tree walk holding the parent's lock may still reach it. Only then can
  // the state be destroyed, which requires exclusive access to it.
  rust_job_dispatcher_leave_job_tree(this);
  rust_job_dispatcher_state_destroy(&opaque_storage_);
}

Lock<CriticalMutex>* JobDispatcher::get_lock() const {
  return rust_job_dispatcher_state_get_lock(&opaque_storage_);
}
