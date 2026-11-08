// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/thread_dispatcher.h"

#include <assert.h>
#include <inttypes.h>
#include <lib/counters.h>
#include <platform.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <arch/exception.h>
#include <arch/regs.h>
#include <arch/vm.h>
#include <fbl/algorithm.h>
#include <fbl/alloc_checker.h>
#include <kernel/thread.h>
#include <object/handle.h>
#include <object/process_dispatcher.h>
#include <vm/kstack.h>
#include <vm/vm.h>
#include <vm/vm_address_region.h>
#include <vm/vm_aspace.h>
#include <vm/vm_object_paged.h>

#define LOCAL_TRACE 0

IMPLEMENT_DISPATCHER_RUST_STATE(ThreadDispatcher, rust_thread_dispatcher_state_get_lock,
                                rust_thread_dispatcher_state_destroy)

ThreadDispatcher::ThreadDispatcher(fbl::RefPtr<ProcessDispatcher> process) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(ThreadDispatcher, kThreadDispatcherStateOffset);
  rust_thread_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&process));
}

// low level LK entry point for the thread
int ThreadDispatcher::StartRoutine(void* arg) {
  LTRACE_ENTRY;

  ThreadDispatcher* t = (ThreadDispatcher*)arg;

  // IWBN to dump the values just before calling |arch_enter_uspace()|
  // but at that point they're in |iframe| and may have been modified by
  // the debugger user, and fetching them out of the iframe will require
  // architecture-specific code. Instead just print them here. This is just
  // for tracing which is generally off, and then only time the values will
  // have changed is if a debugger user changes them. KISS.
  LTRACEF("arch_enter_uspace SP: %#" PRIxPTR " PC: %#" PRIxPTR ", ARG1: %#" PRIxPTR
          ", ARG2: %#" PRIxPTR "\n",
          t->user_entry_.sp, t->user_entry_.pc, t->user_entry_.arg1, t->user_entry_.arg2);

  // Initialize an iframe for entry into userspace.
  // We need all registers accessible from the ZX_EXCP_THREAD_STARTING
  // exception handler (the debugger wants the thread to look as if the
  // thread is at the first instruction). For architectural exceptions the
  // general regs are left in the iframe for speed and simplicity. To keep
  // things simple we use the same scheme.
  iframe_t iframe;
  arch_prepare_uspace(&t->user_entry_, &iframe);

  arch_exception_context_t context = {.frame = &iframe};

  // Notify job debugger if attached.
  if (t->is_initial_thread_) {
    t->process()->OnProcessStartForJobDebugger(t, &context);
  }

  // Notify debugger if attached.
  t->HandleSingleShotException(t->process()->debug_exceptionate(), ZX_EXCP_THREAD_STARTING,
                               context);

  arch_disable_ints();
  arch_iframe_process_pending_signals(&iframe);

  // switch to user mode and start the process
  arch_enter_uspace(&iframe);
  __UNREACHABLE;
}
