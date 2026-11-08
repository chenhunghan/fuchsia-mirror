// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
// Copyright (c) 2012-2012 Shantanu Gupta
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "kernel/mutex.h"

#include <assert.h>
#include <debug.h>
#include <stdint.h>

#include <arch/ops.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>

#include <ktl/enforce.h>

// The rest of the Mutex implementation is in Rust, in //zircon/kernel/kernel/mutex.rs.  These
// methods are defined out of line here, rather than inline in mutex.h, so that ThinLTO decides
// where to inline them; see Mutex::ReleaseInternal in mutex.h.

Mutex::~Mutex() { rust_mutex_destroy(this); }

void Mutex::ReleaseInternal() {
  magic_.Assert();
  DEBUG_ASSERT(!arch_blocking_disallowed());
  Thread* const current_thread = Thread::Current::Get();

  // Clear the guess as to which CPU the owner of the mutex is running on.
  maybe_acquired_on_cpu_.store(INVALID_CPU, ktl::memory_order_relaxed);

  // Try the fast path.  Assume that we are locked, but uncontested.
  // Success memory order is release: makes sure the lock state is ordered so
  // the next thread sees the content. Failure memory order is acquire: makes
  // sure we re-observe the state of the mutex before we try to wake up threads
  // and unwind the state.
  uintptr_t old_mutex_state = reinterpret_cast<uintptr_t>(current_thread);
  if (likely(val_.compare_exchange_strong(old_mutex_state, STATE_FREE, ktl::memory_order_release,
                                          ktl::memory_order_acquire))) {
    // We're done.  Since this mutex was uncontested, we know that we were
    // not receiving any priority pressure from the wait queue, and there is
    // nothing further to do.
    return;
  }

  // The mutex is contended.
  rust_mutex_release_contended(this, current_thread, old_mutex_state);
}
