// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <assert.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <threads.h>

#include <condition_variable>
#include <mutex>

#include "librace.h"
#include "model-assert.h"

// CDSChecker's <stdatomic.h> omits using declarations for these two symbols.
using std::atomic_init;
using std::atomic_uintptr_t;

using Func = void (*)(void*);

// RCU Implementation mimicking fuchsia-rcu
struct Callback {
  Func func;
  void* arg;
  void* next;
};

atomic_int generation;
atomic_int read_counters[2];
atomic_uintptr_t callback_chain;
Callback* waiting_callbacks = nullptr;

// The real implementation uses a futex for waking the advancer and a mutex for waiting_callbacks.
// This model uses std::mutex and std::condition_variable to simulate that behavior.
struct State {
  std::mutex waiting_callbacks_mtx;

  // Used by rcu_wait_for_callbacks and rcu_call to simulate the advancer futex.
  std::mutex advancer_mtx;
  std::condition_variable advancer_cnd;
  uint32_t work_pending;
}* state;

void my_rcu_read_lock(int* index) {
  int gen = atomic_load_explicit(&generation, memory_order_relaxed);
  *index = gen & 1;
  atomic_fetch_add_explicit(&read_counters[*index], 1, memory_order_seq_cst);
}

void my_rcu_read_unlock(int index) {
  atomic_fetch_sub_explicit(&read_counters[index], 1, memory_order_seq_cst);
}

void rcu_call(Func func, void* arg) {
  // We need to synchronize with the rcu_read_lock.
  atomic_thread_fence(memory_order_release);
  atomic_fetch_add_explicit(&read_counters[0], 0, memory_order_relaxed);
  atomic_fetch_add_explicit(&read_counters[1], 0, memory_order_relaxed);

  Callback* cb = (Callback*)malloc(sizeof(Callback));
  cb->func = func;
  cb->arg = arg;
  for (;;) {
    uintptr_t old_head = atomic_load_explicit(&callback_chain, memory_order_relaxed);
    store_64(&cb->next, old_head);
    if (atomic_compare_exchange_strong_explicit(&callback_chain, &old_head, (uintptr_t)cb,
                                                memory_order_release, memory_order_relaxed)) {
      break;
    }
  }

  // Wake the advancer.
  state->advancer_mtx.lock();
  store_32(&state->work_pending, 1);
  state->advancer_cnd.notify_all();
  state->advancer_mtx.unlock();
}

bool has_pending_work() {
  state->waiting_callbacks_mtx.lock();
  bool has_work = (atomic_load_explicit(&callback_chain, memory_order_relaxed) != 0) ||
                  (waiting_callbacks != nullptr);
  state->waiting_callbacks_mtx.unlock();
  return has_work;
}

void rcu_wait_for_callbacks() {
  state->advancer_mtx.lock();
  while (load_32(&state->work_pending) == 0) {
    state->advancer_cnd.wait(state->advancer_mtx);
  }
  store_32(&state->work_pending, 0);
  state->advancer_mtx.unlock();
}

void rcu_grace_period() {
  state->waiting_callbacks_mtx.lock();

  Callback* ready = waiting_callbacks;

  waiting_callbacks = (Callback*)atomic_exchange_explicit(&callback_chain, 0, memory_order_acquire);

  int gen = atomic_fetch_add_explicit(&generation, 1, memory_order_relaxed);

  state->advancer_mtx.lock();
  while (atomic_load_explicit(&read_counters[gen & 1], memory_order_acquire) > 0) {
    thrd_yield();
  }
  state->advancer_mtx.unlock();

  state->waiting_callbacks_mtx.unlock();

  while (ready != nullptr) {
    Callback* next = (Callback*)load_64(&ready->next);
    ready->func(ready->arg);
    free(ready);
    ready = next;
  }
}

bool rcu_run_callbacks() {
  if (has_pending_work()) {
    rcu_grace_period();
    rcu_grace_period();
    return true;
  }
  return false;
}

// DirEntry test structures
struct DirEntry {
  uint8_t alive;
};

atomic_uintptr_t global_parent;

void drop_dir_entry(void* arg) {
  DirEntry* e = (DirEntry*)arg;
  store_8(&e->alive, 0);
}

void thread_reader(void* arg) {
  int index;
  my_rcu_read_lock(&index);
  DirEntry* p = (DirEntry*)atomic_load_explicit(&global_parent, memory_order_acquire);
  if (p) {
    MODEL_ASSERT(load_8(&p->alive) == 1);
  }
  my_rcu_read_unlock(index);
}

void thread_writer(void* arg) {
  DirEntry* new_p = (DirEntry*)malloc(sizeof(DirEntry));
  new_p->alive = 1;

  uintptr_t old_p_val =
      atomic_exchange_explicit(&global_parent, (uintptr_t)new_p, memory_order_acq_rel);
  DirEntry* old_p = (DirEntry*)old_p_val;
  if (old_p) {
    rcu_call(drop_dir_entry, old_p);
  }
}

void thread_advancer(void* arg) {
  rcu_wait_for_callbacks();
  while (rcu_run_callbacks()) {
  }
}

int user_main(int argc, char** argv) {
  state = new State;

  atomic_init(&generation, 0);
  atomic_init(&read_counters[0], 0);
  atomic_init(&read_counters[1], 0);
  atomic_init(&callback_chain, 0);

  store_32(&state->work_pending, 0);

  waiting_callbacks = nullptr;

  DirEntry* initial_p = (DirEntry*)malloc(sizeof(DirEntry));
  initial_p->alive = 1;
  atomic_init(&global_parent, (uintptr_t)initial_p);

  thrd_t t_reader, t_writer, t_advancer;
  thrd_create(&t_advancer, thread_advancer, nullptr);
  thrd_create(&t_reader, thread_reader, nullptr);
  thrd_create(&t_writer, thread_writer, nullptr);

  thrd_join(t_reader);
  thrd_join(t_writer);
  thrd_join(t_advancer);

  MODEL_ASSERT(initial_p->alive == 0);

  return 0;
}
