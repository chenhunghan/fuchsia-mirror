// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_UTILS_CHECK_IS_ON_THREAD_H_
#define SRC_UI_SCENIC_LIB_UTILS_CHECK_IS_ON_THREAD_H_

#include <lib/async/dispatcher.h>
#include <lib/syslog/cpp/macros.h>

namespace utils {

// Returns whether a `ScopedThreadDispatcherSetter` is installed and the current thread's default
// dispatcher matches the stored main or input dispatcher, respectively. Returns false on every
// thread before a setter is installed and after the setter is destroyed.
//
// When the same dispatcher is installed for both roles (e.g. single-threaded Scenic without a
// separate input thread), both predicates return true on that thread; neither predicate is the
// negation of the other, so callers should test the specific role they need.
//
// Exposed alongside `CheckIsOnMainThread()` and `CheckIsOnInputThread()` so release-critical
// invariants can use `FX_CHECK` and unit tests can check thread affinity in release builds.
bool IsOnMainThread();
bool IsOnInputThread();

// Asserts (via `FX_DCHECK`) that the current thread's default dispatcher matches the stored main
// or input dispatcher, respectively. Deliberately lenient when no `ScopedThreadDispatcherSetter`
// is installed and the calling thread has no default dispatcher (`nullptr == nullptr`), so
// loop-less unit tests keep working; callers that need that case to fail use `IsOnMainThread()`
// or `IsOnInputThread()`.
void CheckIsOnMainThread();
void CheckIsOnInputThread();

// RAII object to set the main and input thread dispatchers for testing or production
// initialization.
//
// This object ensures that the dispatchers are set for the duration of its scope and automatically
// reset to `nullptr` when it goes out of scope.
//
// It is a fatal error (`FX_DCHECK`) to create a `ScopedThreadDispatcherSetter` if the dispatchers
// are already set (i.e., not `nullptr`). This prevents nested overrides and ensures test isolation.
class ScopedThreadDispatcherSetter {
 public:
  // Sets the main and input thread dispatchers.
  ScopedThreadDispatcherSetter(async_dispatcher_t* main_dispatcher,
                               async_dispatcher_t* input_dispatcher);

  ~ScopedThreadDispatcherSetter();

  ScopedThreadDispatcherSetter(const ScopedThreadDispatcherSetter&) = delete;
  ScopedThreadDispatcherSetter& operator=(const ScopedThreadDispatcherSetter&) = delete;
};

}  // namespace utils

#endif  // SRC_UI_SCENIC_LIB_UTILS_CHECK_IS_ON_THREAD_H_
