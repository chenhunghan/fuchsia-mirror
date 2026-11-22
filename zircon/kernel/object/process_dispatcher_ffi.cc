// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <new>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/string_view.h>
#include <object/handle.h>
#include <object/job_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/shareable_process_state.h>

extern "C" {

zx_status_t cpp_process_dispatcher_alloc(
    ShareableProcessState* shareable_state, JobDispatcher* job, const char* name_ptr,
    size_t name_len, uint32_t flags,
    ffi::Uninitialized<KernelHandle<ProcessDispatcher>>* out_handle) {
  fbl::RefPtr<ShareableProcessState> shareable_state_ref = fbl::ImportFromRawPtr(shareable_state);
  fbl::RefPtr<JobDispatcher> job_ref = fbl::ImportFromRawPtr(job);
  fbl::AllocChecker ac;
  auto process =
      fbl::AdoptRef(new (&ac) ProcessDispatcher(ktl::move(shareable_state_ref), ktl::move(job_ref),
                                                ktl::string_view(name_ptr, name_len), flags));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  out_handle->Initialize(KernelHandle<ProcessDispatcher>(ktl::move(process)));
  return ZX_OK;
}

ShareableProcessState* cpp_shareable_process_state_create() {
  fbl::AllocChecker ac;
  fbl::RefPtr<ShareableProcessState> state = fbl::AdoptRef(new (&ac) ShareableProcessState);
  if (!ac.check()) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_shareable_process_state_get_ref_counted(ShareableProcessState* state) {
  return static_cast<fbl::RefCounted<ShareableProcessState>*>(state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_shareable_process_state_free(ShareableProcessState* state) {
  delete state;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_shareable_process_state_increment_share_count(
    ShareableProcessState* state) {
  return state->IncrementShareCount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_shareable_process_state_decrement_share_count(
    ShareableProcessState* state) {
  state->DecrementShareCount();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_shareable_process_state_initialize(ShareableProcessState* state,
                                                              vaddr_t aspace_base,
                                                              size_t aspace_size,
                                                              const char* aspace_name,
                                                              VmAspace::ShareOpt share_opt) {
  return state->Initialize(aspace_base, aspace_size, aspace_name, share_opt);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t
cpp_shareable_process_state_share_count(const ShareableProcessState* state) {
  return state->share_count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const HandleTable* cpp_shareable_process_state_handle_table(
    const ShareableProcessState* state) {
  return &const_cast<ShareableProcessState*>(state)->handle_table();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void* cpp_shareable_process_state_futex_context(
    const ShareableProcessState* state) {
  return const_cast<ShareableProcessState*>(state)->futex_context().get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE VmAspace* cpp_shareable_process_state_aspace(const ShareableProcessState* state) {
  return state->aspace();
}

}  // extern "C"
