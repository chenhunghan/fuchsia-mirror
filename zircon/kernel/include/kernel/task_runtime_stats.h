// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_

#include <stddef.h>
#include <zircon/syscalls/object.h>
#include <zircon/time.h>

//
// Types and utilities for efficiently accumulating and aggregating task runtime
// stats.
//
// Runtime stats are maintained at three levels: thread, process, and job.
// Threads maintain and update their runtime stats as threads change state.
// Terminating threads roll up to their owning process, and terminating
// processes roll up to their owning job, however queries of process and job
// stats require summing all of the currently running thread stats with the
// rolled up stats for terminated threads.
//

struct TaskRuntimeStats;

extern "C" {
void rust_task_runtime_stats_to_info(const TaskRuntimeStats* stats, zx_info_task_runtime_t* out);
}

// Runtime stats of a thread, process, or job.
//
// Not safe for concurrent use by multiple threads.
struct TaskRuntimeStats {
  // The total duration (in ticks) spent running on a CPU.
  zx_duration_mono_ticks_t cpu_ticks = 0;

  // The total duration (in ticks) spent ready to start running.
  zx_duration_mono_ticks_t queue_ticks = 0;

  // The total duration (in ticks) spent handling page faults.
  zx_duration_mono_ticks_t page_fault_ticks = 0;

  // The total duration (in ticks) spent contented on kernel locks.
  zx_duration_mono_ticks_t lock_contention_ticks = 0;

  // Adds another TaskRuntimeStats to this one.
  constexpr TaskRuntimeStats& operator+=(const TaskRuntimeStats& other) {
    cpu_ticks = zx_ticks_add_ticks(cpu_ticks, other.cpu_ticks);
    queue_ticks = zx_ticks_add_ticks(queue_ticks, other.queue_ticks);
    page_fault_ticks = zx_ticks_add_ticks(page_fault_ticks, other.page_fault_ticks);
    lock_contention_ticks = zx_ticks_add_ticks(lock_contention_ticks, other.lock_contention_ticks);
    return *this;
  }

  // Conversion to zx_info_task_runtime_t.
  operator zx_info_task_runtime_t() const {
    zx_info_task_runtime_t info;
    rust_task_runtime_stats_to_info(this, &info);
    return info;
  }
};
static_assert(sizeof(TaskRuntimeStats) == 32 && alignof(TaskRuntimeStats) == 8);
static_assert(offsetof(TaskRuntimeStats, cpu_ticks) == 0);
static_assert(offsetof(TaskRuntimeStats, queue_ticks) == 8);
static_assert(offsetof(TaskRuntimeStats, page_fault_ticks) == 16);
static_assert(offsetof(TaskRuntimeStats, lock_contention_ticks) == 24);

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_TASK_RUNTIME_STATS_H_
