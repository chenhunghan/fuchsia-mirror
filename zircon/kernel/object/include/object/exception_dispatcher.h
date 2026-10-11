// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_

#include <lib/object-constants.h>
#include <zircon/rights.h>
#include <zircon/syscalls/exception.h>
#include <zircon/types.h>

#include <arch/exception.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/opaque_storage.h>
#include <object/thread_dispatcher.h>

class ExceptionDispatcher;

extern "C" {
ExceptionDispatcher* cpp_exception_dispatcher_create(ThreadDispatcher* thread,
                                                     zx_excp_type_t exception_type,
                                                     const zx_exception_report_t* report,
                                                     const arch_exception_context_t* arch_context);

void rust_exception_dispatcher_state_init(void* state, void* disp, ThreadDispatcher* thread,
                                          zx_excp_type_t exception_type,
                                          const zx_exception_report_t* report,
                                          const arch_exception_context_t* arch_context);
void rust_exception_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_exception_dispatcher_state_get_lock(const void* state);

ExceptionDispatcher* rust_exception_dispatcher_create(ThreadDispatcher* thread_raw,
                                                      zx_excp_type_t exception_type,
                                                      const zx_exception_report_t* report,
                                                      const arch_exception_context_t* arch_context);
const fbl::RefPtr<ThreadDispatcher>* rust_exception_dispatcher_get_thread(
    const ExceptionDispatcher* disp);
zx_excp_type_t rust_exception_dispatcher_get_exception_type(const ExceptionDispatcher* disp);
void rust_exception_dispatcher_on_zero_handles(ExceptionDispatcher* disp);
bool rust_exception_dispatcher_fill_report(const ExceptionDispatcher* disp,
                                           zx_exception_report_t* report);
void rust_exception_dispatcher_set_task_rights(ExceptionDispatcher* disp, zx_rights_t thread_rights,
                                               zx_rights_t process_rights);
bool rust_exception_dispatcher_is_second_chance(const ExceptionDispatcher* disp);
zx_status_t rust_exception_dispatcher_wait_for_handle_close(ExceptionDispatcher* disp);
void rust_exception_dispatcher_discard_handle_close(ExceptionDispatcher* disp);
void rust_exception_dispatcher_clear(ExceptionDispatcher* disp);
}

// Zircon channel-based exception handling uses two primary classes,
// ExceptionDispatcher (this file) and Exceptionate (exceptionate.h).
//
// An ExceptionDispatcher represents a single currently-active exception. This
// will be transmitted to registered exception handlers in userspace and
// provides them with exception state and control functionality.
//
// An Exceptionate wraps a channel endpoint to help with sending exceptions to
// userspace. It is a kernel-internal helper class and not exposed to userspace.

class ExceptionDispatcher final : public Dispatcher {
 public:
  static constexpr zx_rights_t default_rights() {
    return ZX_DEFAULT_EXCEPTION_RIGHTS;  // NOLINT(bugprone-signed-bitwise)
  }

  // Returns nullptr on memory allocation failure.
  static fbl::RefPtr<ExceptionDispatcher> Create(fbl::RefPtr<ThreadDispatcher> thread,
                                                 zx_excp_type_t exception_type,
                                                 const zx_exception_report_t* report,
                                                 const arch_exception_context_t* arch_context);

  static zx_exception_report_t BuildArchReport(uint32_t type,
                                               const arch_exception_context_t& arch_context);

  ~ExceptionDispatcher() final;

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_EXCEPTION; }
  zx_koid_t get_related_koid() const final { return ZX_KOID_INVALID; }
  bool is_waitable() const final {
    return (default_rights() & ZX_RIGHT_WAIT) != 0;  // NOLINT(bugprone-signed-bitwise)
  }

  zx_status_t user_signal_self(uint32_t clear_mask, uint32_t set_mask) final {
    return UserSignalSelfSolo(this, clear_mask, set_mask, 0);
  }
  zx_status_t user_signal_peer(uint32_t clear_mask, uint32_t set_mask) final {
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Marks the current exception handler as done.
  //
  // Once a handle has been created around this object, either
  // WaitForHandleClose() or DiscardHandleClose() must be called to reset
  // our state for the next handler.
  void on_zero_handles() final { rust_exception_dispatcher_on_zero_handles(this); }

  const fbl::RefPtr<ThreadDispatcher>& thread() const {
    return *rust_exception_dispatcher_get_thread(this);
  }
  zx_excp_type_t exception_type() const {
    return rust_exception_dispatcher_get_exception_type(this);
  }

  // Copies the exception report provided at ExceptionDispatcher creation.
  //
  // The exception report is only available while the exception thread is
  // still alive.
  //
  // Returns false and leaves |report| untouched if the thread has died.
  bool FillReport(zx_exception_report_t* report) const {
    return rust_exception_dispatcher_fill_report(this, report);
  }

  // Sets the task rights to use for subsequent handle creation.
  //
  // rights == 0 indicates that the current exception handler is not allowed
  // to access the corresponding task handle, for example a thread-level
  // handler cannot access its parent process handle.
  //
  // This must only be called by an Exceptionate before transmitting the
  // exception - we don't ever want to be changing task rights while the
  // exception is out in userspace.
  void SetTaskRights(zx_rights_t thread_rights, zx_rights_t process_rights) {
    rust_exception_dispatcher_set_task_rights(this, thread_rights, process_rights);
  }

  // Whether a debugger should have a second chance to handle the exception
  // after the process handler has tried and failed to do so.
  bool IsSecondChance() const { return rust_exception_dispatcher_is_second_chance(this); }

  // Blocks until the exception handler is done processing.
  //
  // This must be called exactly once every time this exception is
  // successfully sent out to userspace, in order to wait for the response
  // and reset the internal state.
  //
  // Returns:
  //   ZX_OK if the exception was handled and the thread should resume.
  //   ZX_ERR_NEXT if the exception should be passed to the next handler.
  //   ZX_ERR_INTERNAL_INTR_KILLED if the thread was killed.
  zx_status_t WaitForHandleClose() { return rust_exception_dispatcher_wait_for_handle_close(this); }

  // Resets the exception state for the next handler.
  //
  // This must be called instead of WaitForHandleClose() if a handle is
  // created around this exception but fails to make it out to userspace,
  // in order to reset the internal state.
  void DiscardHandleClose() { rust_exception_dispatcher_discard_handle_close(this); }

  // Wipe out exception state, which indicates the thread has died.
  void Clear() { rust_exception_dispatcher_clear(this); }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  friend ExceptionDispatcher* cpp_exception_dispatcher_create(
      ThreadDispatcher* thread, zx_excp_type_t exception_type, const zx_exception_report_t* report,
      const arch_exception_context_t* arch_context);

  ExceptionDispatcher(fbl::RefPtr<ThreadDispatcher> thread, zx_excp_type_t exception_type,
                      const zx_exception_report_t* report,
                      const arch_exception_context_t* arch_context);

  OpaqueStorage<kExceptionDispatcherStateSize, kExceptionDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTION_DISPATCHER_H_
