// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/syscalls/object.h>
#include <zircon/types.h>

#include <fbl/ref_counted.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/thread.h>
#include <ktl/string_view.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/handle_table.h>
#include <object/shareable_process_state.h>
#include <object/thread_dispatcher.h>

class Exceptionate;
class JobDispatcher;
class ProcessDispatcher;
class VmAddressRegionDispatcher;

// These are declared ahead of the class because ProcessDispatcher befriends
// cpp_process_dispatcher_alloc, and a friend declaration requires the declaration it names to
// already be visible.
extern "C" {
// Allocates and constructs a ProcessDispatcher. The Rust ProcessDispatcher::create() calls this
// and then initializes the address spaces and registers the process with its job.
zx_status_t cpp_process_dispatcher_alloc(
    ShareableProcessState* shareable_state, JobDispatcher* job, const char* name_ptr,
    size_t name_len, uint32_t flags,
    ffi::Uninitialized<KernelHandle<ProcessDispatcher>>* out_handle);

// Rust FFI prototypes:
void rust_process_dispatcher_state_init(void* state, const ProcessDispatcher* disp,
                                        ShareableProcessState* shareable_state, JobDispatcher* job,
                                        const char* name_ptr, size_t name_len, uint32_t flags);
void rust_process_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_process_dispatcher_state_get_lock(const void* state);
zx_status_t rust_process_dispatcher_create(
    JobDispatcher* job, const char* name_ptr, size_t name_len, uint32_t flags,
    ffi::Uninitialized<KernelHandle<ProcessDispatcher>>* out_handle,
    ffi::Uninitialized<zx_rights_t>* out_rights,
    ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>>* out_vmar_handle,
    ffi::Uninitialized<zx_rights_t>* out_vmar_rights);
void rust_process_dispatcher_on_zero_handles(ProcessDispatcher* process);
zx_koid_t rust_process_dispatcher_get_related_koid(const ProcessDispatcher* process);
zx_status_t rust_process_dispatcher_get_name(const ProcessDispatcher* process, char* out_name);
zx_status_t rust_process_dispatcher_set_name(ProcessDispatcher* process, const char* name,
                                             size_t len);
JobDispatcher* rust_process_dispatcher_job(const ProcessDispatcher* process);
void rust_process_dispatcher_kill(ProcessDispatcher* process, int64_t retcode);
Exceptionate* rust_process_dispatcher_debug_exceptionate(ProcessDispatcher* process);
ShareableProcessState* rust_process_dispatcher_shareable_state(const ProcessDispatcher* process);
ProcessDispatcher* rust_process_dispatcher_lookup_process_by_id(zx_koid_t koid);
void rust_process_dispatcher_on_process_start_for_job_debugger(
    ProcessDispatcher* process, ThreadDispatcher* thread, const arch_exception_context_t* context);
}  // extern "C"

// C++ shell of the process dispatcher. The process state and the methods operating on it live in
// the Rust ProcessDispatcher (object/process_dispatcher.rs); the methods below forward to it for
// the C++ callers that remain.
class ProcessDispatcher final : public Dispatcher {
 public:
  static zx_status_t Create(fbl::RefPtr<JobDispatcher> job, ktl::string_view name, uint32_t flags,
                            KernelHandle<ProcessDispatcher>* handle, zx_rights_t* rights,
                            KernelHandle<VmAddressRegionDispatcher>* root_vmar_handle,
                            zx_rights_t* root_vmar_rights);

  static ProcessDispatcher* GetCurrent() {
    ThreadDispatcher* current = ThreadDispatcher::GetCurrent();
    DEBUG_ASSERT(current);
    return current->process();
  }

  // Dispatcher implementation
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_PROCESS; }
  void on_zero_handles() final { rust_process_dispatcher_on_zero_handles(this); }
  zx_koid_t get_related_koid() const final {
    return rust_process_dispatcher_get_related_koid(this);
  }

  bool is_waitable() const final { return true; }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }

  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  ~ProcessDispatcher() final;

  // Accessors.
  HandleTable& handle_table() { return shareable_state()->handle_table(); }
  const HandleTable& handle_table() const { return shareable_state()->handle_table(); }

  fbl::RefPtr<JobDispatcher> job() const;

  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final {
    return rust_process_dispatcher_get_name(this, out_name);
  }
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final {
    return rust_process_dispatcher_set_name(this, name, len);
  }

  void Kill(int64_t retcode) { rust_process_dispatcher_kill(this, retcode); }

  Exceptionate* debug_exceptionate() { return rust_process_dispatcher_debug_exceptionate(this); }

  // Look up a process given its koid.
  // Returns nullptr if not found.
  static fbl::RefPtr<ProcessDispatcher> LookupProcessById(zx_koid_t koid) {
    return fbl::ImportFromRawPtr(rust_process_dispatcher_lookup_process_by_id(koid));
  }

 protected:
  // Exposes the process dispatcher lock, which is supplied by the Rust ProcessDispatcherState's
  // KMutex, to the generic Dispatcher machinery.
  Lock<CriticalMutex>* get_lock() const final;

 private:
  // The constructor stays private. It is only reachable through this shim, and friendship does
  // not follow the caller across the FFI boundary, so the shim itself is the friend rather than
  // the Rust code that calls it.
  friend zx_status_t cpp_process_dispatcher_alloc(
      ShareableProcessState* shareable_state, JobDispatcher* job, const char* name_ptr,
      size_t name_len, uint32_t flags,
      ffi::Uninitialized<KernelHandle<ProcessDispatcher>>* out_handle);

  ProcessDispatcher(fbl::RefPtr<ShareableProcessState> shareable_state,
                    fbl::RefPtr<JobDispatcher> job, ktl::string_view name, uint32_t flags);

  ProcessDispatcher(const ProcessDispatcher&) = delete;
  ProcessDispatcher& operator=(const ProcessDispatcher&) = delete;

  // Thread lifecycle support.
  friend class ThreadDispatcher;

  void OnProcessStartForJobDebugger(ThreadDispatcher* t, const arch_exception_context_t* context) {
    DEBUG_ASSERT(t);
    DEBUG_ASSERT(context);
    rust_process_dispatcher_on_process_start_for_job_debugger(this, t, context);
  }

  ShareableProcessState* shareable_state() const {
    return rust_process_dispatcher_shareable_state(this);
  }

  // Stores the Rust ProcessDispatcherState instance, which holds every process field: the
  // shareable state and job references, policy, thread list, address spaces, state machine,
  // vDSO code address cache, exceptionates, debug properties, runtime stats, name, intrusive
  // job-list nodes, and the dispatcher lock.
  OpaqueStorage<kProcessDispatcherStateSize, kProcessDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_PROCESS_DISPATCHER_H_
