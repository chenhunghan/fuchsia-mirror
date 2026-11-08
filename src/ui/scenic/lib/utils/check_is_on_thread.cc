// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/utils/check_is_on_thread.h"

#include <lib/async/default.h>
#include <lib/syslog/cpp/macros.h>

#include <atomic>

namespace utils {

namespace {
// Dispatcher globals are atomic because `ScopedThreadDispatcherSetter` writes on the main thread
// while background threads, tests, or worker threads may read concurrently (e.g. during test
// teardown while a worker thread runs).
//
// `std::memory_order_relaxed` suffices for all loads and stores because the pointers publish no
// other data and are only compared for pointer identity against `async_get_default_dispatcher()`.
// Reader threads observe the constructor's store via thread-creation or `async::PostTask`
// happens-before edges, and any reader racing the destructor's `nullptr` store safely observes
// either the prior dispatcher pointer or `nullptr`.
std::atomic<async_dispatcher_t*> g_main_dispatcher{nullptr};
std::atomic<async_dispatcher_t*> g_input_dispatcher{nullptr};
}  // namespace

bool IsOnMainThread() {
  async_dispatcher_t* const main = g_main_dispatcher.load(std::memory_order_relaxed);
  return main != nullptr && main == async_get_default_dispatcher();
}

bool IsOnInputThread() {
  async_dispatcher_t* const input = g_input_dispatcher.load(std::memory_order_relaxed);
  return input != nullptr && input == async_get_default_dispatcher();
}

void CheckIsOnMainThread() {
  FX_DCHECK(g_main_dispatcher.load(std::memory_order_relaxed) == async_get_default_dispatcher());
}

void CheckIsOnInputThread() {
  FX_DCHECK(g_input_dispatcher.load(std::memory_order_relaxed) == async_get_default_dispatcher());
}

ScopedThreadDispatcherSetter::ScopedThreadDispatcherSetter(async_dispatcher_t* main,
                                                           async_dispatcher_t* input) {
  FX_DCHECK(main != nullptr);
  FX_DCHECK(input != nullptr);
  FX_DCHECK(g_main_dispatcher.load(std::memory_order_relaxed) == nullptr);
  FX_DCHECK(g_input_dispatcher.load(std::memory_order_relaxed) == nullptr);
  g_main_dispatcher.store(main, std::memory_order_relaxed);
  g_input_dispatcher.store(input, std::memory_order_relaxed);
}

ScopedThreadDispatcherSetter::~ScopedThreadDispatcherSetter() {
  FX_DCHECK(g_main_dispatcher.load(std::memory_order_relaxed) != nullptr);
  FX_DCHECK(g_input_dispatcher.load(std::memory_order_relaxed) != nullptr);
  g_main_dispatcher.store(nullptr, std::memory_order_relaxed);
  g_input_dispatcher.store(nullptr, std::memory_order_relaxed);
}

}  // namespace utils
