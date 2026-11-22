// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_TESTING_LOADBENCH_WORKER_H_
#define SRC_TESTING_LOADBENCH_WORKER_H_

#include <lib/sync/completion.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/clock.h>
#include <lib/zx/profile.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <lib/zx/timer.h>
#include <zircon/time.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <iostream>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <utility>
#include <vector>

#include "action.h"
#include "object.h"
#include "utility.h"
#include "workload.h"

class Worker {
 public:
  Worker(Worker&&) = delete;
  Worker& operator=(Worker&&) = delete;
  Worker(const Worker&) = delete;
  Worker& operator=(const Worker&) = delete;

  static std::pair<std::thread, std::unique_ptr<Worker>> Create(WorkerConfig config) {
    std::unique_ptr<Worker> worker{new Worker{std::move(config.actions), config.name, config.group,
                                              config.priority, config.affinity}};
    return {std::thread{&Worker::Run, worker.get()}, std::move(worker)};
  }

  // Sleeps the worker for the given duration. Returns early if the termination
  // flag is set, and otherwise records scheduler wake-to-run latency past the
  // requested sleep deadline.
  void Sleep(std::chrono::nanoseconds duration_ns) {
    // Wait on the deadline that latency is measured from. A relative wait
    // computes a second deadline, and preemption between the two would be
    // counted as wakeup latency.
    const zx::time expected_wake = zx::deadline_after(zx::duration{duration_ns.count()});
    const zx_status_t status =
        sync_completion_wait_deadline(&terminate_completion_, expected_wake.get());
    if (status == ZX_ERR_TIMED_OUT && !should_terminate()) {
      RecordWakeupLatency(zx::clock::get_monotonic() - expected_wake);
    }
  }

  // Spins the worker for the given duration. Returns early if the termination
  // flag is set.
  void Spin(std::chrono::nanoseconds duration_ns) {
    const auto end_time = std::chrono::steady_clock::now() + duration_ns;
    while (std::chrono::steady_clock::now() < end_time && !should_terminate()) {
      spin_iterations_.fetch_add(1, std::memory_order_relaxed);
    }
  }

  // Yields the worker.
  void Yield() { zx::nanosleep(zx::time{0}); }

  void SetProfile(const zx::unowned_profile& profile) {
    const auto status = zx::thread::self()->set_profile(*profile, 0);
    FX_CHECK(status == ZX_OK);
  }

  void Exit() { early_exit_ = true; }

  bool IsTerminating() const { return should_terminate() || early_exit_; }

  void SetExpectedTimerWakeTime(zx_handle_t timer_handle, zx::time expected_wake) {
    expected_timer_handle_ = timer_handle;
    expected_timer_wake_ = expected_wake;
  }

  std::optional<zx::time> TakeExpectedTimerWakeTime(zx_handle_t handle) {
    if (expected_timer_wake_.has_value() && expected_timer_handle_ == handle) {
      const zx::time expected = *expected_timer_wake_;
      expected_timer_wake_.reset();
      expected_timer_handle_ = ZX_HANDLE_INVALID;
      return expected;
    }
    return std::nullopt;
  }

  void SetPendingPortWakeTime(zx::time wake_time) { pending_port_wake_ = wake_time; }

  std::optional<zx::time> TakePendingPortWakeTime() {
    const auto wake = pending_port_wake_;
    pending_port_wake_.reset();
    return wake;
  }

  void RecordWakeupLatency(zx::duration latency) {
    using std::chrono_literals::operator""ms;
    using std::chrono_literals::operator""s;
    const std::chrono::nanoseconds clamped_ns{std::max<zx_duration_t>(0, latency.get())};
    wakeup_count_++;
    total_wakeup_latency_ += clamped_ns;
    max_wakeup_latency_ = std::max(max_wakeup_latency_, clamped_ns);
    if (clamped_ns >= 100ms) {
      wakeups_over_100ms_++;
    }
    if (clamped_ns >= 1s) {
      wakeups_over_1s_++;
    }
  }

  void Dump() {
    using double_ms = std::chrono::duration<double, std::milli>;
    static std::mutex output_lock;
    std::lock_guard<std::mutex> guard{output_lock};
    std::cout << "Thread " << id_ << ": group=" << group() << " name=" << name() << std::endl;
    std::cout << "    Completed loops: " << completed_loops() << std::endl;
    std::cout << "    Spin iterations: " << spin_iterations() << std::endl;
    std::cout << "    Total runtime: " << double_seconds{total_runtime()}.count() << " s"
              << std::endl;
    if (wakeup_count_ > 0) {
      const double avg_ms =
          double_ms{total_wakeup_latency_}.count() / static_cast<double>(wakeup_count_);
      const double max_ms = double_ms{max_wakeup_latency_}.count();
      std::cout << "    Wakeups: " << wakeup_count_ << " (avg latency: " << avg_ms
                << " ms, max latency: " << max_ms << " ms, >100ms: " << wakeups_over_100ms_
                << ", >1s: " << wakeups_over_1s_ << ")" << std::endl;
    }
  }

  static void WaitForAllReady(size_t count) {
    using std::chrono_literals::operator""s;
    constexpr auto kTimeoutSeconds = 5s;

    NullLock lock;
    ready_condition_.wait_for(lock, kTimeoutSeconds, [count] { return ready_count() == count; });
    FX_CHECK(ready_count() == count) << "ready_count=" << ready_count() << " count=" << count;
  }

  static void StartAll() { sync_completion_signal(&start_completion_); }

  static void TerminateAll() {
    sync_completion_signal(&terminate_completion_);

    // Exit any indefinite port_wait syscalls.
    const auto status = PortObject::GetTerminateEvent()->signal(0, PortObject::kTerminateSignal);
    FX_CHECK(status == ZX_OK) << "Failed to send signal to terminate event: " << status;
  }

  std::chrono::nanoseconds total_runtime() const {
    return total_runtime_end_ - total_runtime_begin_;
  }
  uint64_t spin_iterations() const { return spin_iterations_.load(); }
  uint64_t completed_loops() const { return completed_loops_; }
  uint64_t wakeup_count() const { return wakeup_count_; }
  std::chrono::nanoseconds total_wakeup_latency() const { return total_wakeup_latency_; }
  std::chrono::nanoseconds max_wakeup_latency() const { return max_wakeup_latency_; }
  uint64_t wakeups_over_100ms() const { return wakeups_over_100ms_; }
  uint64_t wakeups_over_1s() const { return wakeups_over_1s_; }

  const std::string& name() const { return name_; }
  const std::string& group() const { return group_; }

 private:
  Worker(std::vector<std::unique_ptr<Action>> actions, const std::string& name,
         const std::string& group, WorkerConfig::PriorityType priority,
         std::optional<zx_cpu_set_t> affinity)
      : id_{thread_counter_++},
        actions_{std::move(actions)},
        name_{name},
        group_{group},
        priority_{priority},
        affinity_{affinity} {}

  void Run() {
    if (std::holds_alternative<int>(priority_)) {
      auto profile = GetProfile(std::get<int>(priority_), affinity_);
      const auto status = zx::thread::self()->set_profile(*profile, 0);
      FX_CHECK(status == ZX_OK) << "Failed to set worker " << id_ << " to priority "
                                << std::get<int>(priority_) << "!";
    } else if (std::holds_alternative<WorkerConfig::DeadlineParams>(priority_)) {
      const auto params = std::get<WorkerConfig::DeadlineParams>(priority_);
      auto profile =
          GetProfile(params.capacity, params.deadline, params.period, affinity_, params.critical);
      const auto status = zx::thread::self()->set_profile(*profile, 0);
      FX_CHECK(status == ZX_OK) << "Failed to set worker " << id_
                                << " to {capacity=" << params.capacity.get()
                                << ", deadline=" << params.deadline.get()
                                << ", period=" << params.period.get() << "}!";
    } else if (affinity_.has_value()) {
      auto profile = GetProfile(affinity_.value());
      const auto status = zx::thread::self()->set_profile(*profile, 0);
      FX_CHECK(status == ZX_OK) << "Failed to set worker " << id_ << " affinity!";
    }

    // Setup the actions on this worker.
    for (auto& action : actions_) {
      action->Setup(this);
    }

    // Signal that the worker is ready and wait for the benchmark to kick off.
    {
      ready_count_++;
      ready_condition_.notify_one();

      const auto status = sync_completion_wait(&start_completion_, ZX_TIME_INFINITE);
      FX_CHECK(status == ZX_OK) << "Failed to wait for start condition: status=" << status;
    }

    zx_info_thread_stats_t thread_stats{};
    zx::thread::self()->get_info(ZX_INFO_THREAD_STATS, &thread_stats, sizeof(thread_stats), nullptr,
                                 nullptr);
    total_runtime_begin_ = std::chrono::nanoseconds{thread_stats.total_runtime};

    while (!should_terminate() && !early_exit_) {
      bool completed_all = true;
      for (auto& action : actions_) {
        if (should_terminate() || early_exit_) {
          completed_all = false;
          break;
        }
        action->Perform(this);
      }
      if (completed_all && !should_terminate() && !early_exit_) {
        completed_loops_++;
      }
    }

    zx::thread::self()->get_info(ZX_INFO_THREAD_STATS, &thread_stats, sizeof(thread_stats), nullptr,
                                 nullptr);
    total_runtime_end_ = std::chrono::nanoseconds{thread_stats.total_runtime};
  }

  struct NullLock {
    void lock() {}
    void unlock() {}
  };

  int id_;
  std::vector<std::unique_ptr<Action>> actions_;
  std::string name_;
  std::string group_;
  WorkerConfig::PriorityType priority_;
  std::optional<zx_cpu_set_t> affinity_;

  bool early_exit_{false};
  std::atomic<uint64_t> spin_iterations_{0};
  uint64_t completed_loops_{0};

  zx_handle_t expected_timer_handle_{ZX_HANDLE_INVALID};
  std::optional<zx::time> expected_timer_wake_;
  std::optional<zx::time> pending_port_wake_;

  uint64_t wakeup_count_{0};
  std::chrono::nanoseconds total_wakeup_latency_{0};
  std::chrono::nanoseconds max_wakeup_latency_{0};
  uint64_t wakeups_over_100ms_{0};
  uint64_t wakeups_over_1s_{0};

  std::chrono::nanoseconds total_runtime_begin_;
  std::chrono::nanoseconds total_runtime_end_;

  static bool should_terminate() { return sync_completion_signaled(&terminate_completion_); }
  static size_t ready_count() { return ready_count_.load(); }

  inline static std::atomic<int> thread_counter_{0};

  inline static sync_completion terminate_completion_;
  inline static sync_completion start_completion_;

  inline static std::condition_variable_any ready_condition_{};
  inline static std::atomic<size_t> ready_count_{0};
};

#endif  // SRC_TESTING_LOADBENCH_WORKER_H_
