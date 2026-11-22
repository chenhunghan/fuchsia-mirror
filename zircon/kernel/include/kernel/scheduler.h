// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_

#include <debug.h>
#include <lib/power-management/bandwidth-reservation-cache.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/power-state.h>
#include <lib/power-management/types.h>
#include <lib/relaxed_atomic.h>
#include <lib/sched/affine.h>
#include <lib/zircon-internal/macros.h>
#include <platform.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <ffl/fixed.h>
#include <kernel/ffi.h>
#include <kernel/scheduler_state.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <ktl/array.h>
#include <ktl/atomic.h>
#include <ktl/optional.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>
#include <platform/timer.h>

struct RunQueueIterator {
  Thread* current;
};
static_assert(sizeof(RunQueueIterator) == 8 && alignof(RunQueueIterator) == 8);

extern "C" {
void rust_kernel_run_queue_insert(void* rq, Thread* thread);
Thread* rust_kernel_run_queue_erase(void* rq, Thread* thread);
Thread* rust_kernel_run_queue_find_earliest_eligible_thread(const void* rq, SchedTime eligible_time,
                                                            bool (*predicate)(const Thread*,
                                                                              const void*),
                                                            const void* context);
size_t rust_kernel_run_queue_size(const void* rq);
bool rust_kernel_run_queue_is_empty(const void* rq);
Thread* rust_kernel_run_queue_front(const void* rq);
RunQueueIterator rust_kernel_run_queue_iterator_create(const void* rq);
bool rust_kernel_run_queue_iterator_is_valid(const RunQueueIterator* iter);
Thread* rust_kernel_run_queue_iterator_get(const RunQueueIterator* iter);
void rust_kernel_run_queue_iterator_next(RunQueueIterator* iter);
Thread* rust_kernel_run_queue_root(const void* rq);
void rust_kernel_run_queue_init(void* rq);
void rust_kernel_run_queue_destroy(void* rq);
}

// The processing rate of a CPU relative to the fastest CPU in the system.
using SchedProcessingRate = ffl::Fixed<int64_t, 31>;

// Make sure lib/power-management and the scheduler definition stay in sync.
static_assert(ktl::is_same_v<power_management::ProcessingRate, SchedProcessingRate>);

class Scheduler;

extern "C" {

void cpp_scheduler_lock_queue(Scheduler* sched);
void cpp_scheduler_unlock_queue(Scheduler* sched);
uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched);
void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state);

cpu_mask_t cpp_scheduler_power_state_post_update_request(
    Scheduler* sched, const power_management::PowerLevelUpdateRequest* req);

cpu_mask_t rust_scheduler_peek_active_mask();
void rust_scheduler_on_power_domain_set_updated(Scheduler* sched, SchedProcessingRate active_rate,
                                                SchedProcessingRate max_rate, bool has_domain);
zx_status_t rust_scheduler_update_active_power_level(Scheduler* sched, uint8_t power_level);
bool rust_scheduler_request_power_level_for_testing(Scheduler* sched, uint8_t power_level);

void rust_kernel_scheduler_initialize_thread(Thread* thread,
                                             const SchedulerState::BaseProfile* profile);
}
// Implements fair and deadline scheduling algorithms and manages the associated
// per-CPU state.
class Scheduler {
 public:
  class RunQueue;
  using Affine = sched::Affine<10>;

  // Minimum fair time slice. Set to avoid excessive reschedule overhead.
  // TODO(eieio): This needs to be revised and/or scaled for lower OPPs.
  static constexpr SchedDuration kMinimumFairCapacity = SchedMs(1);

  // The default period for fair tasks when period expansion is not in effect.
  static constexpr SchedDuration kDefaultFairPeriod = SchedDefaultFairPeriod;

  // The minimum fair utilization before period expansion is needed.
  static constexpr SchedUtilization kMinFairUtilization = kMinimumFairCapacity / kDefaultFairPeriod;

  // Reciprocal of the default fair period to minimize runtime divisions in fair
  // utilization calculations.
  static constexpr SchedUtilization kReciprocalDefaultFairPeriod = 1 / kDefaultFairPeriod;

  // Ensure that the min fair utilization is in the range (0, 1].
  static_assert(0 < kMinimumFairCapacity && kMinimumFairCapacity <= kDefaultFairPeriod);

  // The per-CPU deadline utilization limit to attempt to honor when selecting a
  // CPU to place a task. It is up to userspace to ensure that the total set of
  // deadline tasks can honor this limit. Even if userspace ensures the total
  // set of deadline utilizations is within the total available processor
  // resources, when total utilization is high enough it may not be possible to
  // honor this limit due to the bin packing problem.
  static constexpr SchedUtilization kCpuUtilizationLimit{1};

  // The maximum deadline utilization permitted for a single thread. This limit
  // is applied when scaling the utilization of a deadline task to the relative
  // performance of a candidate target processor -- placing a task on a
  // processor that would cause the scaled thread utilization to exceed this
  // value is avoided if possible.
  static constexpr SchedUtilization kThreadUtilizationMax{1};

  Scheduler() = default;
  ~Scheduler() = default;

  Scheduler(const Scheduler&) = delete;
  Scheduler& operator=(const Scheduler&) = delete;

  // Returns the number of the CPU this scheduler instance is associated with.
  cpu_num_t this_cpu() const TA_NO_THREAD_SAFETY_ANALYSIS { return rust_state_.this_cpu; }

  // Public entry points.

  // See the comment in create_etc (kernel/thread.rs) for why InitializeThread
  // does not take a stance on whether or not we should be in an active
  // chain-lock transaction. On the surface, demanding that we hold a threads
  // lock without also demanding that we also be involved in an active
  // chain-lock transaction is an apparent contradiction of some fundamental
  // invariants (how can a chain lock be held without being in a transaction?),
  // but when used while constructing a thread that has not been published yet
  // (as create_etc does), it should be OK.
  static void InitializeThread(Thread* thread, const SchedulerState::BaseProfile& profile)
      TA_REQ(thread->get_lock()) {
    rust_kernel_scheduler_initialize_thread(thread, &profile);
  }

  // Peek at the current active mask for all schedulers.
  //
  // Note that this is just a lockless atomic load.  Baring special
  // circumstances, the set of active schedulers can change at any time.
  static cpu_mask_t PeekActiveMask() { return rust_scheduler_peek_active_mask(); }

  using PowerDomain = power_management::PowerDomain;
  using PowerDomainSet = power_management::PowerDomainSet;

  // Sets the power domain set for this scheduler instance, returning the previous set.
  PowerDomainSet ExchangePowerDomainSet(PowerDomainSet domain_set) TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    PowerDomainSet previous =
        rust_tail_.power_state.UpdatePowerDomainSet(ktl::move(domain_set), this_cpu());
    const SchedProcessingRate active_rate = rust_tail_.power_state.active_processing_rate();
    const bool has_domain = static_cast<bool>(rust_tail_.power_state.domain());
    const SchedProcessingRate max_rate =
        has_domain ? rust_tail_.power_state.domain()->model().max_processing_rate()
                   : SchedProcessingRate{0};
    rust_scheduler_on_power_domain_set_updated(this, active_rate, max_rate, has_domain);
    return previous;
  }

  // Sets the power domain set for all CPUs.
  static void SetPowerDomainSet(PowerDomainSet power_domain_set);

  // Updates the current power level with the value reported by the power level
  // controller.
  static zx::result<cpu_mask_t> UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                                 power_management::ControlInterface interface,
                                                 uint64_t arg);

  // Updates the current power level for this CPU with the value reported by the power level
  // controller. Called by kernel tests and sys_system_set_processor_power_state.
  zx::result<> UpdateActivePowerLevel(uint8_t power_level) TA_EXCL(queue_lock_) {
    return zx::make_result(rust_scheduler_update_active_power_level(this, power_level));
  }

  // Returns the current active power level, if any.
  ktl::optional<uint8_t> GetActivePowerLevel() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_tail_.power_state.active_power_level();
  }

  // Sends a power level request through this scheduler's power level controller for testing. This
  // method is called from thread and IRQ context to ensure that the underlying machinery can be
  // safely called within Block, Unblock, and other scheduling operations.
  //
  // Returns true if the request was sent, false otherwise.
  bool RequestPowerLevelForTesting(uint8_t power_level) TA_EXCL(queue_lock_) {
    return rust_scheduler_request_power_level_for_testing(this, power_level);
  }

  // Returns the current power domain for this scheduler instance.
  fbl::RefPtr<PowerDomain> GetPowerDomainForTesting() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_tail_.power_state.domain();
  }

  // Returns the steady-state idle power in nanowatts latched by the last reschedule on this CPU.
  // Updates from UpdateActivePowerLevel() are reflected after the next reschedule.
  uint64_t GetIdlePowerNwForTesting() const TA_EXCL(queue_lock_) {
    Guard<MonitoredSpinLock, IrqSave> guard{&queue_lock_, SOURCE_TAG};
    return rust_state_.power_level_control.idle_power_nw;
  }

  // Const accessor for static analysis.
  const auto& queue_lock_cap() const TA_RET_CAP(queue_lock_) { return queue_lock_; }

 private:
  static void AssertLayout() TA_NO_THREAD_SAFETY_ANALYSIS {
    static_assert(kMinimumFairCapacity.raw_value() == 1'000'000);
    static_assert(kDefaultFairPeriod.raw_value() == 10'000'000);
    static_assert(kMinFairUtilization.raw_value() == 214'748'364);
    static_assert(kReciprocalDefaultFairPeriod.raw_value() == 214);
    static_assert(kCpuUtilizationLimit.raw_value() == (1LL << 31));
    static_assert(kThreadUtilizationMax.raw_value() == (1LL << 31));
    static_assert(sizeof(Affine) == 24);
    static_assert(alignof(Affine) == 8);
    static_assert(ktl::is_standard_layout_v<RustSchedulerState>);
    static_assert(ktl::is_standard_layout_v<RustSchedulerTail>);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Winvalid-offsetof"
    static_assert(offsetof(Scheduler, rust_state_) == 0);
    // Rust's Scheduler::QUEUE_LOCK_OFFSET (scheduler.rs) and percpu::AssertLayout
    // rely on queue_lock_ being the first field after rust_state_.
    static_assert(offsetof(Scheduler, queue_lock_) == sizeof(RustSchedulerState));
    static_assert(offsetof(Scheduler, rust_tail_) == sizeof(Scheduler) - sizeof(RustSchedulerTail));
#pragma clang diagnostic pop
    static_assert(sizeof(PowerLevelControlState) == 112);
    static_assert(alignof(PowerLevelControlState) == 8);
    static_assert(offsetof(PowerLevelControlState, processing_rate) == 0);
    static_assert(offsetof(PowerLevelControlState, processing_rate_reciprocal) == 8);
    static_assert(offsetof(PowerLevelControlState, updated_processing_rate) == 16);
    static_assert(offsetof(PowerLevelControlState, default_processing_rate) == 24);
    static_assert(offsetof(PowerLevelControlState, max_processing_rate) == 32);
    static_assert(offsetof(PowerLevelControlState, processing_rate_limit_min) == 40);
    static_assert(offsetof(PowerLevelControlState, processing_rate_limit_max) == 48);
    static_assert(offsetof(PowerLevelControlState, active_power_coefficient_nw) == 56);
    static_assert(offsetof(PowerLevelControlState, idle_power_nw) == 64);
    static_assert(offsetof(PowerLevelControlState, pending_update_request) == 72);
    static_assert(offsetof(PowerLevelControlState, has_pending_update_request) == 104);
    static_assert(offsetof(PowerLevelControlState, has_pending_power_coefficient_update) == 105);

    // The overall size of Scheduler depends on build configuration (e.g. lockdep
    // changes sizeof(MonitoredSpinLock)); the Rust side derives it from the
    // bindgen-generated percpu bindings instead of a fixed constant.
    static_assert(sizeof(RustSchedulerState) == 800);
    static_assert(alignof(RustSchedulerState) == 8);
    static_assert(offsetof(RustSchedulerState, fair_run_queue) == 0);
    static_assert(offsetof(RustSchedulerState, deadline_run_queue) == 32);
    static_assert(offsetof(RustSchedulerState, critical_deadline_run_queue) == 64);
    static_assert(offsetof(RustSchedulerState, runnable_task_count) == 96);
    static_assert(offsetof(RustSchedulerState, weight_total) == 104);
    static_assert(offsetof(RustSchedulerState, scheduled_weight_total) == 112);
    static_assert(offsetof(RustSchedulerState, active_thread) == 120);
    static_assert(offsetof(RustSchedulerState, fair_period) == 128);
    static_assert(offsetof(RustSchedulerState, reciprocal_fair_period) == 136);
    static_assert(offsetof(RustSchedulerState, last_update_time_ns) == 144);
    static_assert(offsetof(RustSchedulerState, start_of_current_time_slice_ns) == 152);
    static_assert(offsetof(RustSchedulerState, target_preemption_time_ns) == 160);
    static_assert(offsetof(RustSchedulerState, total_expected_runtime_ns) == 168);
    static_assert(offsetof(RustSchedulerState, this_cpu) == 176);
    static_assert(offsetof(RustSchedulerState, cluster) == 184);
    static_assert(offsetof(RustSchedulerState, cpu_deactivating) == 192);
    static_assert(offsetof(RustSchedulerState, exported_fair_weight_total) == 200);
    static_assert(offsetof(RustSchedulerState, exported_deadline_utilization) == 208);
    static_assert(offsetof(RustSchedulerState, exported_clamped_deadline_utilization) == 216);
    static_assert(offsetof(RustSchedulerState, exported_fair_utilization) == 224);
    static_assert(offsetof(RustSchedulerState, exported_clamped_fair_utilization) == 232);
    static_assert(offsetof(RustSchedulerState, exported_clamped_total_utilization) == 240);
    static_assert(offsetof(RustSchedulerState, exported_processing_rate) == 248);
    static_assert(offsetof(RustSchedulerState, exported_max_processing_rate) == 256);
    static_assert(offsetof(RustSchedulerState, previous_thread) == 264);
    static_assert(offsetof(RustSchedulerState, critical_deadline_utilization) == 272);
    static_assert(offsetof(RustSchedulerState, active_thread_weight) == 280);
    static_assert(offsetof(RustSchedulerState, fair_affine_transform) == 288);
    static_assert(offsetof(RustSchedulerState, processor_idle_time_ns) == 312);
    static_assert(offsetof(RustSchedulerState, bandwidth_reservation_cache) == 320);
    static_assert(offsetof(RustSchedulerState, power_level_control) == 520);
    static_assert(offsetof(RustSchedulerState, variable_reservation_cache) == 632);
    static_assert(offsetof(RustSchedulerState, exported_reservation_tids) == 736);

    static_assert(sizeof(Thread::SaveStateList) == 8);
    static_assert(sizeof(RustSchedulerTail) == 64);
    static_assert(alignof(RustSchedulerTail) == 8);
    static_assert(offsetof(RustSchedulerTail, save_state_list) == 0);
    static_assert(offsetof(RustSchedulerTail, power_state) == 8);
  }

 public:
  struct SchedMonoTimeAndBootTicks {
    SchedTime mono_time;
    zx_instant_boot_ticks_t boot_ticks;
  };

  // Returns the current monotonic time / boot ticks using coherent samples of
  // the respective timelines.
  // TODO(eieio): Look into making this non-static and requiring the queue lock
  // of the respective scheduler to ensure this is always called with the queue
  // lock held.
  static SchedMonoTimeAndBootTicks CurrentMonoTimeAndBootTicks() {
    InstantMonoTimeAndBootTicks mono_time_and_boot_ticks = current_mono_time_and_boot_ticks();
    return {.mono_time = SchedTime{mono_time_and_boot_ticks.mono_time},
            .boot_ticks = mono_time_and_boot_ticks.boot_ticks};
  }

  // Returns the Scheduler instance for the current CPU.
  static Scheduler* Get();

  // Returns the Scheduler instance for the given CPU.
  static Scheduler* Get(cpu_num_t cpu);

 private:
  friend void cpp_scheduler_lock_queue(Scheduler* sched);
  friend void cpp_scheduler_unlock_queue(Scheduler* sched);
  friend uint64_t cpp_scheduler_lock_queue_irq_save(Scheduler* sched);
  friend void cpp_scheduler_unlock_queue_irq_restore(Scheduler* sched, uint64_t state);
  friend cpu_mask_t cpp_scheduler_power_state_post_update_request(
      Scheduler* sched, const power_management::PowerLevelUpdateRequest* req);

 public:
  class RunQueue {
   public:
    RunQueue() { rust_kernel_run_queue_init(opaque_state_); }
    ~RunQueue() { rust_kernel_run_queue_destroy(opaque_state_); }

    RunQueue(const RunQueue&) = delete;
    RunQueue& operator=(const RunQueue&) = delete;
    RunQueue(RunQueue&&) = delete;
    RunQueue& operator=(RunQueue&&) = delete;

   private:
    alignas(8) uint8_t opaque_state_[32];
  };

  // Track bandwidth reservations for up to 8 recently blocked threads.
  static constexpr size_t kMaxBandwidthReservations = 8;
  static constexpr size_t kMaxVariableReservations = 4;
  using BandwidthReservationCache =
      power_management::BandwidthReservationCache<kMaxBandwidthReservations>;

  struct PowerLevelControlState {
    SchedProcessingRate processing_rate{1};
    SchedProcessingRate processing_rate_reciprocal{1};
    SchedProcessingRate updated_processing_rate{1};
    SchedProcessingRate default_processing_rate{1};
    SchedProcessingRate max_processing_rate{1};
    SchedProcessingRate processing_rate_limit_min{0};
    SchedProcessingRate processing_rate_limit_max{1};
    uint64_t active_power_coefficient_nw{0};
    uint64_t idle_power_nw{0};
    power_management::PowerLevelUpdateRequest pending_update_request{};
    bool has_pending_update_request{false};
    bool has_pending_power_coefficient_update{false};
  };

  struct RustSchedulerState {
    Scheduler::RunQueue fair_run_queue;
    Scheduler::RunQueue deadline_run_queue;
    Scheduler::RunQueue critical_deadline_run_queue;
    int32_t runnable_task_count{0};
    SchedWeight weight_total{0};
    SchedWeight scheduled_weight_total{0};
    Thread* active_thread{nullptr};
    SchedDuration fair_period{kDefaultFairPeriod};
    SchedUtilization reciprocal_fair_period{kReciprocalDefaultFairPeriod};
    SchedTime last_update_time_ns{0};
    SchedTime start_of_current_time_slice_ns{0};
    SchedTime target_preemption_time_ns{ZX_TIME_INFINITE};
    SchedDuration total_expected_runtime_ns{0};
    cpu_num_t this_cpu;
    size_t cluster{0};
    ktl::atomic<bool> cpu_deactivating{false};
    // Total weight of the fair threads runnable on this CPU. Owned and
    // documented by the Rust scheduler; mirrored here only to keep the layout
    // in sync.
    RelaxedAtomic<SchedWeight> exported_fair_weight_total{SchedWeight{0}};
    RelaxedAtomic<SchedUtilization> exported_deadline_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_deadline_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_fair_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_fair_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedUtilization> exported_clamped_total_utilization{SchedUtilization{0}};
    RelaxedAtomic<SchedProcessingRate> exported_processing_rate{SchedProcessingRate{1}};
    RelaxedAtomic<SchedProcessingRate> exported_max_processing_rate{SchedProcessingRate{1}};
    ktl::atomic<Thread*> previous_thread{nullptr};
    SchedUtilization critical_deadline_utilization{0};
    SchedWeight active_thread_weight{SchedWeight::Max()};
    Affine fair_affine_transform{};
    ktl::atomic<zx_duration_mono_t> processor_idle_time_ns{0};
    BandwidthReservationCache bandwidth_reservation_cache{};
    PowerLevelControlState power_level_control{};
    // Variable (fair) demand reservations. Owned and documented by the Rust
    // scheduler (RustSchedulerState::variable_reservation_cache); mirrored
    // here only to keep the layout in sync.
    power_management::BandwidthReservationCache<kMaxVariableReservations>
        variable_reservation_cache;
    // The tid held by each slot of bandwidth_reservation_cache, exported for
    // cross-CPU readers. Owned and documented by the Rust scheduler
    // (RustSchedulerState::exported_reservation_tids); mirrored here only to
    // keep the layout in sync.
    ktl::array<RelaxedAtomic<zx_koid_t>, kMaxBandwidthReservations> exported_reservation_tids{};
  };

  struct RustSchedulerTail {
    // The list of threads that need to run the Save stage of their migrate functions.
    //
    // There are several invariants that must be maintained when interacting with this
    // list:
    // 1. Other CPUs can put threads into this save_state_list, but only this Scheduler (and thus
    //    this CPU) can remove threads from it. Removal is currently handled in
    //    Scheduler::process_save_state_list.
    // 2. Interrupts must be disabled while removing threads from this list, effectively ensuring
    //    that only one call to Scheduler::process_save_state_list can be made at a time on a given
    //    CPU.
    // 3. Threads cannot be destroyed until they are removed from this list. This is guaranteed
    //    today by the fact that dead threads clean themselves up when they are scheduled, which is
    //    not possible if the thread is in this list.
    Thread::SaveStateList save_state_list;
    power_management::PowerState power_state{};
  };

 private:
  TA_GUARDED(queue_lock_) RustSchedulerState rust_state_;

  static_assert(sizeof(RunQueue) == 32);
  static_assert(alignof(RunQueue) == 8);

  // Protects run queues and associated metadata for this Scheduler instance.
  // The queue lock is the bottom most lock in the system for the CPU it is
  // associated with: no other locks may be nested within a queue lock. A queue
  // lock may be acquired across CPUs, however, the no nesting rule still
  // applies.
  //
  // Note: while the queue_lock_ is the bottom-most lock in the system from the
  // scheduler's perspective, it is not (formally) a leaf-lock right now.
  // Currently, there is one place in the system (DumpThreadLocked) where the
  // queue_lock_ needs to be held, while fprintf is called.
  //
  // This happens when the lockup-detector is unhappy, and asking the Scheduler
  // to print information about its run queue state.  Most of the printf targets
  // which could be passed to the scheduler also involve holding locks.  If the
  // queue_lock_ is flagged as a leaf lock, and lockdep is enabled, and the
  // lockup-detector attempt to report a hang, it will cause lock-dep to
  // complain about obtaining a lock while holding a leaf-lock, which will cause
  // more printing, which causes more complaints, etc...
  //
  // See https://fxbug.dev/42070437 for more details.
  mutable DECLARE_SPINLOCK_WITH_TYPE(Scheduler, MonitoredSpinLock) queue_lock_
      TA_ACQ_AFTER(Thread::get_list_lock());

  // RustSchedulerTail stays after queue_lock_ rather than in RustSchedulerState: folding it into
  // RustSchedulerState would move queue_lock_ 64 bytes later in percpu, toward
  // percpu::active_cl_transaction and chain_lock_conflict_id, which this CPU writes on every
  // chain-lock transaction while other CPUs take queue_lock_ (see percpu::AssertLayout).
  TA_GUARDED(queue_lock_) RustSchedulerTail rust_tail_;
};

extern "C" {
void cpp_scheduler_init(ffi::Uninitialized<Scheduler>* out);
const void* cpp_scheduler_get_queue_lock(const Scheduler* scheduler);
void cpp_current_mono_time_and_boot_ticks(Scheduler::SchedMonoTimeAndBootTicks* out);
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_scheduler_update_power_level(
    uint32_t domain_id, uint64_t controller_id, power_management::ControlInterface interface,
    uint64_t arg, cpu_mask_t* out_mask);
}  // extern "C"

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_SCHEDULER_H_
