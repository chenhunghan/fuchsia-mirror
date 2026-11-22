// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_JOB_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_JOB_DISPATCHER_H_

#include <lib/object-constants.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/assert.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <object/dispatcher.h>

// Job signal that is active when a job has no children (i.e., no child jobs and no child
// processes).
//
// TODO(https://fxbug.dev/42131457): This is a temporary signal that we don't want userspace using
// (yet?). Either expose this signal to userspace in "zircon/types.h", or remove this signal.
#define ZX_JOB_NO_CHILDREN __ZX_OBJECT_SIGNAL_6

class JobDispatcher;
class JobPolicy;
class ProcessDispatcher;

// These are declared ahead of the class because JobDispatcher befriends cpp_job_dispatcher_alloc,
// and a friend declaration requires the declaration it names to already be visible.
extern "C" {
// Allocates and constructs a JobDispatcher, returning it with an acquired reference (or null on
// allocation failure). The Rust JobDispatcher::create() and JobDispatcher::create_root_job() call
// this and then register the job with its parent. |parent| carries an acquired reference (or is
// null for the root job) and |policy| is copied.
JobDispatcher* cpp_job_dispatcher_alloc(JobDispatcher* parent, const JobPolicy* policy);

// Rust FFI prototypes:
void rust_job_dispatcher_state_init(void* state, JobDispatcher* parent, const JobPolicy* policy);
void rust_job_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_job_dispatcher_state_get_lock(const void* state);
void rust_job_dispatcher_leave_job_tree(JobDispatcher* job);
zx_koid_t rust_job_dispatcher_get_related_koid(const JobDispatcher* job);
zx_status_t rust_job_dispatcher_get_name(const JobDispatcher* job, char* out_name);
zx_status_t rust_job_dispatcher_set_name(JobDispatcher* job, const char* name, size_t len);
void rust_job_dispatcher_remove_child_process(JobDispatcher* job, ProcessDispatcher* process);
}  // extern "C"

// C++ shell of the job dispatcher. The job state and the methods operating on it live in the Rust
// JobDispatcher (object/job_dispatcher.rs); the methods below forward to it for the C++ callers
// that remain.
class JobDispatcher final : public Dispatcher {
 public:
  ~JobDispatcher() final;

  // Dispatcher implementation.
  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_JOB; }
  zx_koid_t get_related_koid() const final { return rust_job_dispatcher_get_related_koid(this); }

  bool is_waitable() const final {
    return (ZX_DEFAULT_JOB_RIGHTS & ZX_RIGHT_WAIT) != 0;  // NOLINT(bugprone-signed-bitwise)
  }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }

  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Job methods.
  [[nodiscard]] zx_status_t get_name(char (&out_name)[ZX_MAX_NAME_LEN]) const final {
    return rust_job_dispatcher_get_name(this, out_name);
  }
  [[nodiscard]] zx_status_t set_name(const char* name, size_t len) final {
    return rust_job_dispatcher_set_name(this, name, len);
  }

  void RemoveChildProcess(ProcessDispatcher* process) {
    DEBUG_ASSERT(process);
    rust_job_dispatcher_remove_child_process(this, process);
  }

 protected:
  // Exposes the job dispatcher lock, which is supplied by the Rust JobDispatcherState's KMutex, to
  // the generic Dispatcher machinery.
  Lock<CriticalMutex>* get_lock() const final;

 private:
  // The constructor stays private. It is only reachable through this shim, and friendship does
  // not follow the caller across the FFI boundary, so the shim itself is the friend rather than
  // the Rust code that calls it.
  friend JobDispatcher* cpp_job_dispatcher_alloc(JobDispatcher* parent, const JobPolicy* policy);

  JobDispatcher(fbl::RefPtr<JobDispatcher> parent, const JobPolicy* policy);

  JobDispatcher(const JobDispatcher&) = delete;
  JobDispatcher& operator=(const JobDispatcher&) = delete;

  // Stores the Rust JobDispatcherState instance, which holds every job field: the parent
  // reference, max height, name, state machine, return code, kill-on-OOM bit, child job and
  // process lists, policy, exceptionates, aggregated runtime stats, the intrusive nodes through
  // which the parent job links this job, and the dispatcher lock.
  OpaqueStorage<kJobDispatcherStateSize, kJobDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_JOB_DISPATCHER_H_
