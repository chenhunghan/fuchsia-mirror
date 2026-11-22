// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/job_dispatcher.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE JobDispatcher* cpp_job_dispatcher_alloc(JobDispatcher* parent,
                                                          const JobPolicy* policy) {
  fbl::RefPtr<JobDispatcher> parent_ref = fbl::ImportFromRawPtr(parent);
  fbl::AllocChecker ac;
  auto job = fbl::AdoptRef(new (&ac) JobDispatcher(ktl::move(parent_ref), policy));
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&job);
}

}  // extern "C"
