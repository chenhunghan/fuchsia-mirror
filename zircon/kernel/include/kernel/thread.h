// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_H_

#include <arch.h>
#include <debug.h>
#include <lib/backtrace.h>
#include <lib/concurrent/capability_token.h>
#include <lib/fxt/thread_ref.h>
#include <lib/kconcurrent/chainlock.h>
#include <lib/page-map/accessor.h>
#include <lib/relaxed_atomic.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <sys/types.h>
#include <zircon/compiler.h>
#include <zircon/syscalls/rseq.h>
#include <zircon/types.h>

#include <arch/arch_thread.h>
#include <arch/ops.h>
#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/intrusive_single_list.h>
#include <fbl/macros.h>
#include <fbl/null_lock.h>
#include <fbl/ref_ptr.h>
#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/koid.h>
#include <kernel/preempt_disabled_token.h>
#include <kernel/restricted_state.h>
#include <kernel/scheduler_state.h>
#include <kernel/spinlock.h>
#include <kernel/wait.h>
#include <ktl/array.h>
#include <ktl/atomic.h>
#include <ktl/string_view.h>
#include <lockdep/thread_lock_state.h>
#include <vm/kstack.h>

class ThreadDispatcher;
class VmAspace;
struct Thread;

// These forward declarations are needed so that Thread can friend
// them before they are defined.
static inline Thread* arch_get_current_thread();
static inline void arch_set_current_thread(Thread*);

// TODO(https://fxbug.dev/528405969): Remove cfi_unchecked_callee once cross-language
// CFI between Clang C++ and rustc is supported in the kernel.
typedef int (*thread_start_routine)(void* arg) [[clang::cfi_unchecked_callee]];

// Thread trampolines are always called with:
//
// 1) The thread's lock held
// 2) interrupts disabled.
//
// Implementers of trampolines must always remember to unconditionally release
// the current thread's lock, and *then* enable interrupts, before jumping to
// the thread's entry point.
//
// Sadly, there does not seem to be a good way to statically annotate this.
typedef void (*thread_trampoline_routine)() __NO_RETURN;

// LINT.IfChange(thread_flags_and_signals)
// clang-format off
#define THREAD_FLAG_DETACHED                 (1 << 0)
#define THREAD_FLAG_FREE_STRUCT              (1 << 1)
#define THREAD_FLAG_IDLE                     (1 << 2)
#define THREAD_FLAG_VCPU                     (1 << 3)
#define THREAD_FLAG_RESTRICTED_KICK_PENDING  (1 << 4)

#define THREAD_SIGNAL_KILL                   (1 << 0)
#define THREAD_SIGNAL_SUSPEND                (1 << 1)
#define THREAD_SIGNAL_POLICY_EXCEPTION       (1 << 2)
#define THREAD_SIGNAL_RESTRICTED_KICK        (1 << 3)
#define THREAD_SIGNAL_SAMPLE_STACK           (1 << 4)
#define THREAD_SIGNAL_CHECK_RSEQ             (1 << 5)
#define THREAD_SIGNAL_POLICY_KILL            (1 << 6)
// clang-format on
// LINT.ThenChange(
//   //zircon/kernel/kernel/thread.rs:thread_flags_and_signals,
//   //zircon/kernel/kernel/thread_ffi.cc:thread_flags_and_signals,
// )

// thread priority
#define NUM_PRIORITIES (32)
#define LOWEST_PRIORITY (0)
#define HIGHEST_PRIORITY (NUM_PRIORITIES - 1)
#define LOW_PRIORITY (NUM_PRIORITIES / 4)
#define DEFAULT_PRIORITY (NUM_PRIORITIES / 2)

class PreemptionState;

// Rust implementations of PreemptionState's slow paths and cold methods (see kernel/thread.rs).
// The Rust mirror holds only relaxed atomics, so every entry point takes a shared reference.
extern "C" {
[[gnu::cold]] void rust_preemption_state_preempt_reenable_slow(
    const PreemptionState& preemption_state, uint32_t old_state);
[[nodiscard, gnu::cold]] bool rust_preemption_state_preempt_reenable_delay_flush_slow(
    const PreemptionState& preemption_state, uint32_t old_state);
[[gnu::cold]] void rust_preemption_state_eager_resched_reenable_slow(
    const PreemptionState& preemption_state, uint32_t old_state);
bool rust_preemption_state_set_timeslice_extension(const PreemptionState& preemption_state,
                                                   zx_duration_mono_t extension_duration);
void rust_preemption_state_clear_timeslice_extension(const PreemptionState& preemption_state);
void rust_preemption_state_preempt_set_pending(const PreemptionState& preemption_state,
                                               cpu_mask_t reschedule_mask);
}  // extern "C"

class PreemptionState {
 public:
  // Counters contained in state_ are limited to 15 bits.
  static constexpr uint32_t kMaxCountValue = 0x7fff;
  // The preempt disable count is in the lowest 15 bits.
  static constexpr uint32_t kPreemptDisableMask = kMaxCountValue;
  // The eager resched disable count is in the next highest 15 bits.
  static constexpr uint32_t kEagerReschedDisableShift = 15;
  static constexpr uint32_t kEagerReschedDisableMask = kMaxCountValue << kEagerReschedDisableShift;
  // Finally, the timeslice extension flags are the highest 2 bits.
  static constexpr uint32_t kTimesliceExtensionFlagsShift = 30;
  static constexpr uint32_t kTimesliceExtensionFlagsMask =
      ~(kPreemptDisableMask | kEagerReschedDisableMask);
  enum TimesliceExtensionFlags : uint32_t {
    // Thread has a timeslice extension that may or may not be Active.
    Present = 0b01u << kTimesliceExtensionFlagsShift,
    // Thread has an Active (in use) timeslice extension.
    Active = 0b10u << kTimesliceExtensionFlagsShift,
  };

  bool PreemptIsEnabled() const {
    // Preemption is enabled iff both counts are zero and there's no runtime
    // extension.
    return state_.load() == 0;
  }

  // TODO(https://fxbug.dev/537458631): PreemptDisable, PreemptReenable,
  // PreemptReenableDelayFlush, EagerReschedDisable, and EagerReschedReenable
  // duplicate the Rust fast path on purpose because release kernels do not use
  // cross-language LTO; they can be removed once kernel LTO is on by default or
  // the C++ callers are gone.
  // LINT.IfChange(PreemptionStateFastPaths)

  // PreemptDisable() increments the preempt disable counter for the current
  // thread. While preempt disable is non-zero, preemption of the thread is
  // disabled, including preemption from interrupt handlers. During this time,
  // any call to Reschedule() will only record that a reschedule is pending, and
  // won't do a context switch.
  //
  // Note that this does not disallow blocking operations (e.g.
  // mutex.Acquire()). Disabling preemption does not prevent switching away from
  // the current thread if it blocks.
  //
  // A call to PreemptDisable() must be matched by a later call to
  // PreemptReenable() to decrement the preempt disable counter.
  void PreemptDisable() {
    const uint32_t old_state = state_.fetch_add(1);
    ASSERT(PreemptDisableCount(old_state) < kMaxCountValue);
  }

  // PreemptReenable() decrements the preempt disable counter and flushes any
  // pending local preemption operation.  Callers must ensure that they are
  // calling from a context where blocking is allowed, as the call may result in
  // the immediate preemption of the calling thread.
  void PreemptReenable() {
    const uint32_t old_state = state_.fetch_sub(1);
    // Fast path: count drops to zero, no eager resched count, no extension, nothing pending.
    if (likely(old_state == 1)) {
      // Order the state_ RMW before the preempts_pending_ load for same-CPU interrupts.
      ktl::atomic_signal_fence(ktl::memory_order_seq_cst);
      if (unlikely(preempts_pending_.load() != 0)) {
        rust_preemption_state_preempt_reenable_slow(*this, old_state);
      }
      return;
    }
    // Are either of the counters non-zero?  Only take the early return when the preempt
    // disable count was at least one: the underflow case (count already zero) must fall
    // through to the slow path, which asserts on it.
    if (PreemptDisableCount(old_state) != 0 &&
        (old_state & (kPreemptDisableMask | kEagerReschedDisableMask)) > 1) {
      // We've got a non-zero count in one of the counters.
      return;
    }
    rust_preemption_state_preempt_reenable_slow(*this, old_state);
  }

  void PreemptDisableAnnotated() TA_ACQ(preempt_disabled_token) {
    preempt_disabled_token.Acquire();
    PreemptDisable();
  }

  void PreemptReenableAnnotated() TA_REL(preempt_disabled_token) {
    preempt_disabled_token.Release();
    PreemptReenable();
  }

  // PreemptReenableDelayFlush() decrements the preempt disable counter, but
  // deliberately does _not_ flush any pending local preemption operation.
  // Instead, if local preemption has become enabled again after the count
  // drops, and the local pending bit is set, the method will clear the bit and
  // return true.  Otherwise, it will return false.
  //
  // This method may only be called when interrupts are disabled and blocking is
  // not allowed.
  //
  // Callers of this method are "taking" ownership of the responsibility to
  // ensure that preemption on the local CPU takes place in the near future
  // after the call if the method returns true.
  //
  // Use of this method is strongly discouraged outside of top-level interrupt
  // glue and early threading setup.
  //
  // TODO(johngro): Consider replacing the bool return type with a move-only
  // RAII type which wraps the bool, and ensures that preemption event _must_
  // happen, either by having the user call a method on the object to manually
  // force the preemption event, or when the object destructs.
  [[nodiscard]] bool PreemptReenableDelayFlush() {
    DEBUG_ASSERT(arch_ints_disabled());
    DEBUG_ASSERT(arch_blocking_disallowed());

    const uint32_t old_state = state_.fetch_sub(1);

    // First, check for the expected situation of dropping the preempt count to zero
    // with a zero eager resched disable count and no timeslice extension.
    if (likely(old_state == 1)) {
      const cpu_mask_t local_mask = cpu_num_to_mask(arch_curr_cpu_num());
      const cpu_mask_t prev_mask = preempts_pending_.fetch_and(~local_mask);
      return (local_mask & prev_mask) != 0;
    }
    // Are either of the counters non-zero?  Only take the early return when the preempt
    // disable count was at least one: the underflow case (count already zero) must fall
    // through to the slow path, which asserts on it.
    if (PreemptDisableCount(old_state) != 0 &&
        (old_state & (kPreemptDisableMask | kEagerReschedDisableMask)) > 1) {
      // We've got a non-zero count in one of the counters.
      return false;
    }
    return rust_preemption_state_preempt_reenable_delay_flush_slow(*this, old_state);
  }

  // EagerReschedDisable() increments the eager resched disable counter for the
  // current thread. When early resched disable is non-zero, issuing local and
  // remote preemptions is disabled, including from interrupt handlers. During
  // this time, any call to Reschedule() or other scheduler entry points that
  // imply a reschedule will only record the pending reschedule for the affected
  // CPU, but will not perform reschedule IPIs or a local context switch.
  //
  // As with PreemptDisable, blocking operations are still allowed while
  // eager resched disable is non-zero.
  //
  // A call to EagerReschedDisable() must be matched by a later call to
  // EagerReschedReenable() to decrement the eager resched disable counter.
  void EagerReschedDisable() {
    const uint32_t old_state = state_.fetch_add(1u << kEagerReschedDisableShift);
    ASSERT_MSG(EagerReschedDisableCount(old_state) < kMaxCountValue, "old_state: %#" PRIx32,
               old_state);
  }

  // EagerReschedReenable() decrements the eager resched disable counter and
  // flushes pending local and/or remote preemptions if enabled, respectively.
  void EagerReschedReenable() {
    const uint32_t old_state = state_.fetch_sub(1u << kEagerReschedDisableShift);
    // Fast path: count drops to zero, no preempt disable count, no extension, nothing pending.
    if (likely(old_state == (1u << kEagerReschedDisableShift))) {
      // Order the state_ RMW before the preempts_pending_ load for same-CPU interrupts.
      ktl::atomic_signal_fence(ktl::memory_order_seq_cst);
      if (unlikely(preempts_pending_.load() != 0)) {
        rust_preemption_state_eager_resched_reenable_slow(*this, old_state);
      }
      return;
    }
    if (EagerReschedDisableCount(old_state) > 1) {
      // Nothing to do since eager resched disable implies preempt disable.
      return;
    }
    rust_preemption_state_eager_resched_reenable_slow(*this, old_state);
  }
  // LINT.ThenChange(//zircon/kernel/kernel/thread.rs:PreemptionStateFastPaths)

  void EagerReschedDisableAnnotated() TA_ACQ(preempt_disabled_token) {
    preempt_disabled_token.Acquire();
    EagerReschedDisable();
  }

  void EagerReschedReenableAnnotated() TA_REL(preempt_disabled_token) {
    preempt_disabled_token.Release();
    EagerReschedReenable();
  }

  // Sets a timeslice extension if one is not already set.
  //
  // This method should only be called in normal thread context.
  //
  // Returns false if a timeslice extension was already present or if the
  // supplied duration is <= 0.
  //
  // Note: It OK to call this from a context where preemption is (hard)
  // disabled.  If preemption is requested while the preempt disable count is
  // non-zero and a timeslice extension is in place, the extension will be
  // activated, but preemption will not occur until the count has dropped to
  // zero and the extension has expired or has been clear.
  bool SetTimesliceExtension(zx_duration_mono_t extension_duration) {
    return rust_preemption_state_set_timeslice_extension(*this, extension_duration);
  }

  // Unconditionally clears any timeslice extension.
  //
  // This method must be called in normal thread context because it may trigger
  // local preemption.
  void ClearTimesliceExtension() { rust_preemption_state_clear_timeslice_extension(*this); }

  // PreemptSetPending() marks a pending preemption for the given CPUs.
  //
  // This is similar to Reschedule(), except that it may only be used inside an
  // interrupt handler while interrupts and preemption are disabled, between
  // PreemptDisable() and PreemptReenable(). It is similar to Reschedule(),
  // except that it does not need to be called with thread's lock held.
  void PreemptSetPending(cpu_mask_t reschedule_mask = cpu_num_to_mask(arch_curr_cpu_num())) {
    rust_preemption_state_preempt_set_pending(*this, reschedule_mask);
  }

 private:
  friend concurrent::CapabilityToken<PreemptionState>;

  // <kernel/thread_layout.h> exposes member offsets to the Rust mirror of this class.
  friend struct PreemptionStateLayout;

  // Implements the runtime assertion for concurrent::CapabilityToken<PreemptionState>::AssertHeld.
  static void AssertCapabilityHeld(concurrent::CapabilityToken<PreemptionState>::TagType);

  static inline uint32_t EagerReschedDisableCount(uint32_t state) {
    return (state & kEagerReschedDisableMask) >> kEagerReschedDisableShift;
  }

  static inline uint32_t PreemptDisableCount(uint32_t state) { return state & kPreemptDisableMask; }

  // state_ contains three fields:
  //
  //  * a 15-bit preempt disable counter (bits 0-14)
  //  * a 15-bit eager resched disable counter (bits 15-29)
  //  * a 2-bit for TimesliceExtensionFlags (bits 30-31)
  //
  // This is a single field so that both counters and the flags can be compared
  // against zero with a single memory access and comparison.
  //
  // state_'s counts are modified by interrupt handlers, but the counts are
  // always restored to their original value before the interrupt handler
  // returns, so modifications are not visible to the interrupted thread.
  RelaxedAtomic<uint32_t> state_{};

  // preempts_pending_ tracks pending reschedules to both local and remote CPUs
  // due to activity in the context of the current thread.
  //
  // This value can be changed asynchronously by an interrupt handler.
  //
  // preempts_pending_ should only be non-zero:
  //  * if PreemptDisableCount() or EagerReschedDisable() are non-zero, or
  //  * after PreemptDisableCount() or EagerReschedDisable() have been
  //    decremented, while preempts_pending_ is being checked.
  RelaxedAtomic<cpu_mask_t> preempts_pending_{};

  // The maximum duration of the thread's timeslice extension.
  //
  // This field is only valid when |state_|'s Present timeslice extension flag
  // is set.
  //
  // This field may only be accessed by its owning thread or in an interrupt
  // context of the owning thread.  When reading this field, be sure to issue an
  // atomic_signal_fence (compiler barrier) with acquire semantics after
  // observing the Present flag.  Likewise, when writing this field, use an
  // atomic_signal_fence with release semantics prior to setting the Present
  // flag.  By using these fences, we ensure the flag and field value remain in
  // sync.
  RelaxedAtomic<zx_duration_mono_t> timeslice_extension_{};

  // The deadline at which the thread timeslice extension expires.
  //
  // This field is only valid when |state_|'s Active timeslice extension flag
  // is set.
  //
  // This field may only be accessed by its owning thread or in an interrupt
  // context of the owning thread.  When reading this field, be sure to issue an
  // atomic_signal_fence (compiler barrier) with acquire semantics after
  // observing the Active flag.  Likewise, when writing this field, use an
  // atomic_signal_fence with release semantics prior to setting the Active
  // flag.  By using these fences, we ensure the flag and field value remain in
  // sync.
  RelaxedAtomic<zx_instant_mono_t> timeslice_extension_deadline_{};
};

// TaskState is responsible for running the task defined by
// |entry(arg)|, and reporting its value to any joining threads.
//
// TODO: the detached state in Thread::flags_ probably belongs here.
class TaskState {
 public:
  TaskState() = default;

 private:
  // <kernel/thread_layout.h> exposes member offsets to the Rust mirror of this class.
  friend struct TaskStateLayout;

  // The Thread's entry point, and its argument.
  [[maybe_unused]] thread_start_routine entry_ = nullptr;
  [[maybe_unused]] void* arg_ = nullptr;

  // Storage for the return code.
  //
  // TODO(johngro): This is logically protected by our thread's lock.  What is a
  // good way to represent this with a static annotation?
  [[maybe_unused]] int retcode_ = 0;

  // Other threads waiting to join this Thread.
  WaitQueue retcode_wait_queue_;
};

// Keeps track of whether a thread is allowed to allocate memory.
//
// A thread's |MemoryAllocationState| should only be accessed by that thread itself or interrupt
// handlers running in the thread's context.
class MemoryAllocationState {
 public:
  // Returns true if memory allocation is allowed.
  bool IsEnabled() {
    ktl::atomic_signal_fence(ktl::memory_order_seq_cst);
    return disable_count_ == 0;
  }

 private:
  // <kernel/thread_layout.h> exposes member offsets to the Rust mirror of this class.
  friend struct MemoryAllocationStateLayout;

  // Notice that we aren't using atomic operations to access the field.  We don't need atomic
  // operations here as long as...
  //
  // 1. We use atomic_signal_fence to prevent compiler reordering.
  //
  // 2. We use volatile to ensure the compiler actually generates loads and stores for the value (so
  // the interrupt handler can see what the thread see, and vice versa).
  //
  // 3. Upon completion, an interrupt handler that modified the field restores it to the value it
  // held at the start of the interrupt.
  volatile uint32_t disable_count_ = 0;
};

// Stall accounting state of a thread.
//
// Kernel-only threads are never considered for the purpose of memory stall
// accounting (`IgnoredKernelOnly`).
//
// If a user thread is not signalling a stall, it will contribute to the current
// CPU's memory stall accumulator as `Progressing` while running and do not
// contribute (`Inactive`) in any other state.
//
// If a user thread signals that a stall is ongoing, it will contribute to the
// current CPU's accumulator as `Stalling`. If preempted, the contribution will
// persist on the last CPU's accumulator. Normal operation is resumed when the
// thread signals that the stall is over.
enum class ThreadStallState {
  IgnoredKernelOnly,
  Inactive,
  Progressing,
  // Safety invariant: a thread only enters `Stalling` while it is the running
  // thread on its CPU (Rust `ScopedMemoryStall` on the current thread), so a
  // `Stalling` thread's `last_cpu` is a valid CPU number (`<
  // PerCpu::processor_count()`); `last_cpu` is only written at context switch
  // and idle init and never reset.
  Stalling,
};

enum class ThreadMigrateStage : uint8_t {
  // The stage before the thread has migrated. Called from the old CPU to save state.
  Save,
  // The stage after the thread has migrated. Called from the new CPU to restore state.
  Restore,
  // The Thread is exiting. Can be called from any CPU.
  Exiting,
};

extern "C" {

// Rust implementations of the Thread flags and name accessors and of
// CheckKillOrSuspendSignal. The C++ methods of the same name below are thin
// forwarders to these; none of them takes a lock.
bool rust_thread_vcpu(const Thread* thread);
void rust_thread_set_vcpu(const Thread* thread, bool value);
const char* rust_thread_name(const Thread* thread);
zx_status_t rust_thread_check_kill_or_suspend_signal(const Thread* thread);

// Rust implementations of Thread and Thread::Current methods. The C++ methods of the same name
// below are thin forwarders to these. rust_thread_create_etc returns null on failure, and
// rust_thread_join stores the return code through |out_retcode| (if non-null) on success.
Thread* rust_thread_create_etc(Thread* t, const char* name, size_t name_len,
                               thread_start_routine entry, void* arg,
                               const SchedulerState::BaseProfile& profile,
                               thread_trampoline_routine alt_trampoline);
void rust_thread_resume(Thread* thread);
zx_status_t rust_thread_detach_and_resume(Thread* thread);
zx_status_t rust_thread_join(Thread* thread, int* out_retcode, zx_instant_mono_t deadline);
void rust_thread_forget(Thread* thread);
void rust_thread_secondary_cpu_init_early(Thread* thread);
ThreadDispatcher* rust_thread_current_user_thread();
cpu_mask_t rust_thread_set_cpu_affinity(Thread* thread, cpu_mask_t affinity);
cpu_mask_t rust_thread_get_cpu_affinity(const Thread* thread);
cpu_num_t rust_thread_last_cpu(const Thread* thread);
cpu_num_t rust_thread_last_cpu_locked(const Thread* thread);
zx_status_t rust_thread_current_soft_fault_in_range(vaddr_t va, uint flags, size_t len);
void rust_thread_current_yield();
zx_status_t rust_thread_current_sleep(zx_instant_mono_t deadline);
zx_status_t rust_thread_current_sleep_relative(zx_duration_mono_t delay);
void rust_thread_current_get_backtrace_with_fp(vaddr_t fp, Backtrace& out_bt);
lockdep::Lock<SpinLock>* rust_thread_get_list_lock();
Thread* rust_thread_create_empty_named(const char* name, size_t name_len);
void rust_thread_delete(Thread* thread);
void rust_thread_set_migrate_fn(Thread* thread,
                                void (*func)(void* ctx, Thread* thread, ThreadMigrateStage stage),
                                void* ctx);
void rust_thread_set_migrate_fn_locked(
    Thread* thread, void (*func)(void* ctx, Thread* thread, ThreadMigrateStage stage), void* ctx);
void rust_thread_set_context_switch_fn_locked(Thread* thread, void (*func)(void* ctx), void* ctx);

}  // extern "C"

struct Thread : public ChainLockable {
  // `Thread` is constructed and destroyed exclusively in Rust (`Thread::init` / `PinnedDrop`).
  Thread() = delete;
  ~Thread() = delete;

  // Creates a thread with |name| that will execute |entry| at |priority|. |arg|
  // will be passed to |entry| when executed, the return value of |entry| will be
  // passed to Exit().
  // This call allocates a thread and places it in the global thread list. This
  // memory will be freed by either Join() or Detach(), one of these
  // MUST be called.
  // The thread will not be scheduled until Resume() is called.
  static Thread* Create(ktl::string_view name, thread_start_routine entry, void* arg, int priority)
      TA_EXCL(chainlock_transaction_token) {
    return CreateEtc(nullptr, name, entry, arg, SchedulerState::BaseProfile{priority}, nullptr);
  }
  static Thread* CreateEtc(Thread* t, ktl::string_view name, thread_start_routine entry, void* arg,
                           const SchedulerState::BaseProfile& profile,
                           thread_trampoline_routine alt_trampoline)
      TA_EXCL(chainlock_transaction_token) {
    return rust_thread_create_etc(t, name.data(), name.size(), entry, arg, profile, alt_trampoline);
  }

  // Internal initialization routines. Eventually, these should be private.
  void SecondaryCpuInitEarly() { rust_thread_secondary_cpu_init_early(this); }

  static lockdep::Lock<SpinLock>& get_list_lock() { return *rust_thread_get_list_lock(); }

  // Returns the koid of the associated ProcessDispatcher for user threads or
  // ZX_KOID_INVLID for kernel threads.
  zx_koid_t pid() const { return pid_.load(ktl::memory_order_relaxed); }

  // Returns the koid of the associated ThreadDispatcher for user threads or an
  // independent koid for kernel threads.
  zx_koid_t tid() const { return tid_.load(ktl::memory_order_relaxed); }

  // Return the pid/tid of the thread as a tracing thread reference.
  fxt::ThreadRef<fxt::RefType::kInline> fxt_ref() const { return {pid(), tid()}; }

  // Called to mark a thread as schedulable.
  void Resume() TA_EXCL(chainlock_transaction_token) { rust_thread_resume(this); }
  void Forget() TA_EXCL(chainlock_transaction_token) { rust_thread_forget(this); }
  zx_status_t DetachAndResume() TA_EXCL(chainlock_transaction_token) {
    return rust_thread_detach_and_resume(this);
  }
  // Waits |deadline| time for a thread to complete execution then releases its memory.
  zx_status_t Join(int* retcode, zx_instant_mono_t deadline) TA_EXCL(chainlock_transaction_token) {
    return rust_thread_join(this, retcode, deadline);
  }

  // Checks whether the kill or suspend signal has been raised. If kill has been
  // raised, then `ZX_ERR_INTERNAL_INTR_KILLED` will be returned. If suspend has
  // been raised, then `ZX_ERR_INTERNAL_INTR_RETRY` will be returned. Otherwise,
  // `ZX_OK` will be returned.
  zx_status_t CheckKillOrSuspendSignal() const {
    return rust_thread_check_kill_or_suspend_signal(this);
  }

  void* recursive_object_deletion_list() { return recursive_object_deletion_list_; }
  void set_recursive_object_deletion_list(void* ptr) { recursive_object_deletion_list_ = ptr; }

  // Get/set the hard CPU affinity mask of valid CPUs that the thread may run on. If a new mask is
  // set, the thread will be migrated to satisfy the new constraint, and the scheduler will panic if
  // it can't satisfy this affinity.
  //
  // Setting the affinity mask returns the previous value to make pinning and restoring operations
  // simpler and more efficient.
  cpu_mask_t SetCpuAffinity(cpu_mask_t affinity) TA_EXCL(get_lock()) {
    return rust_thread_set_cpu_affinity(this, affinity);
  }
  cpu_mask_t GetCpuAffinity() const TA_EXCL(get_lock()) {
    return rust_thread_get_cpu_affinity(this);
  }

  using MigrateStage = ThreadMigrateStage;

  // Inline storage size and alignment for Rust-owned `Option<MigrateFn>` /
  // `Option<ContextSwitchFn>` (`zr::SendInlineFnMut` with 2-word inline capacity + 1-word static
  // vtable pointer).
  static constexpr size_t kCallbackSize = 3 * sizeof(void*);
  static constexpr size_t kCallbackAlign = alignof(void*);

  // The migrate function will be invoked twice when a thread is migrated
  // between CPUs. Firstly when the thread is removed from the old CPU's
  // scheduler, secondly when the thread is rescheduled on the new CPU. When the
  // migrate function is called, `get_lock()` is held.
  template <typename F>
  void SetMigrateFn(F&& migrate_fn) TA_EXCL(get_list_lock(), get_lock()) {
    CppCallback<Thread*, MigrateStage> fn(ktl::forward<F>(migrate_fn));
    rust_thread_set_migrate_fn(this, fn.func, fn.ctx);
  }
  template <typename F>
  void SetMigrateFnLocked(F&& migrate_fn) TA_REQ(get_list_lock(), get_lock()) {
    CppCallback<Thread*, MigrateStage> fn(ktl::forward<F>(migrate_fn));
    rust_thread_set_migrate_fn_locked(this, fn.func, fn.ctx);
  }

  // The context switch function will be invoked when a thread is context
  // switched to or away from. This will be called when a thread is about to be
  // run on a CPU, after it's stopped from running on a CPU, or about to exit.
  template <typename F>
  void SetContextSwitchFnLocked(F&& context_switch_fn) TA_REQ(get_lock()) {
    CppCallback<> fn(ktl::forward<F>(context_switch_fn));
    rust_thread_set_context_switch_fn_locked(this, fn.func, fn.ctx);
  }

  // Last cpu this thread was running on, or INVALID_CPU if it has never run.
  cpu_num_t LastCpu() const TA_EXCL(get_lock()) { return rust_thread_last_cpu(this); }
  cpu_num_t LastCpuLocked() const TA_REQ_SHARED(get_lock()) {
    return rust_thread_last_cpu_locked(this);
  }

  // All of these operations implicitly operate on the current thread.
  struct Current {
    // This is defined below, just after the Thread declaration.
    static inline Thread* Get();

    // Scheduler routines to be used by regular kernel code.
    static void Yield() { rust_thread_current_yield(); }

    // Wait until the deadline has occurred (non-interruptible).
    static zx_status_t Sleep(zx_instant_mono_t deadline) {
      return rust_thread_current_sleep(deadline);
    }
    // Non-interruptible relative delay version of Sleep.
    static zx_status_t SleepRelative(zx_duration_mono_t delay) {
      return rust_thread_current_sleep_relative(delay);
    }

    static PreemptionState& preemption_state() {
      return Thread::Current::Get()->preemption_state();
    }

    static MemoryAllocationState& memory_allocation_state() {
      return Thread::Current::Get()->memory_allocation_state_;
    }

    static RestrictedState* restricted_state() {
      return Thread::Current::Get()->restricted_state();
    }

    static VmAspace* active_aspace() { return Thread::Current::Get()->aspace_; }

    // This function handles faults on the address space containing va. If there is no
    // aspace that contains va, or we don't have access to it, a ZX_ERR_NOT_FOUND is returned.
    //
    // Calling this method on a pure kernel thread (i.e. one without an associated
    // `ThreadDispatcher`) is a programming error.
    // May block on page requests and must be called without locks held.
    static zx_status_t SoftFaultInRange(vaddr_t va, uint flags, size_t len) {
      return rust_thread_current_soft_fault_in_range(va, flags, len);
    }

    // Generate a backtrace for the calling thread starting at frame pointer |fp|.
    //
    // |out_bt| will be reset() prior to be filled in and if a backtrace cannot
    // be obtained, it will be left empty.
    static void GetBacktrace(vaddr_t fp, Backtrace& out_bt) {
      rust_thread_current_get_backtrace_with_fp(fp, out_bt);
    }
  };  // struct Current;

  // Traits for the temporary unblock list, used to batch-unblock threads.
  struct UnblockListTrait {
    static fbl::DoublyLinkedListNodeState<Thread*>& node_state(Thread& thread) {
      return thread.unblock_list_node_;
    }
  };
  using UnblockList = fbl::DoublyLinkedListCustomTraits<Thread*, UnblockListTrait>;

  // Traits for the save state list, used to run the Save stage of migration functions.
  struct SaveStateListTrait {
    static fbl::SinglyLinkedListNodeState<Thread*>& node_state(Thread& thread) {
      return thread.save_state_list_node_;
    }
  };
  using SaveStateList = fbl::SinglyLinkedListCustomTraits<Thread*, SaveStateListTrait>;

  struct Linebuffer {
    static constexpr size_t kBufferSize = 128;

    size_t pos = 0;
    ktl::array<char, kBufferSize> buffer{};
  };

  // Accessors into Thread state. When the conversion to all-private
  // members is complete (bug 54383), we can revisit the overall
  // Thread API.

  thread_state state() const TA_REQ_SHARED(get_lock()) { return scheduler_state_.state(); }

  // Accessors for specific flags_ bits.
  bool vcpu() const { return rust_thread_vcpu(this); }
  void set_vcpu(bool value) { rust_thread_set_vcpu(this, value); }

  PreemptionState& preemption_state() { return preemption_state_; }
  const PreemptionState& preemption_state() const { return preemption_state_; }

  SchedulerState& scheduler_state() TA_REQ(get_lock()) { return scheduler_state_; }
  const SchedulerState& scheduler_state() const TA_REQ_SHARED(get_lock()) {
    return scheduler_state_;
  }
#ifdef UNITTESTS_ENABLED
  void* rust_ktrace_override() const { return rust_ktrace_override_; }
#endif

  WaitQueueCollection::ThreadState& wait_queue_state() TA_REQ(get_lock()) {
    return wait_queue_state_;
  }
  const WaitQueueCollection::ThreadState& wait_queue_state() const TA_REQ_SHARED(get_lock()) {
    return wait_queue_state_;
  }

  RestrictedState* restricted_state() { return restricted_state_.get(); }

  arch_thread& arch() { return arch_; }
  const arch_thread& arch() const { return arch_; }

  KernelStack& stack() { return stack_; }
  const KernelStack& stack() const { return stack_; }

  // Rules for the aspace_ member.
  //
  // 1) Raw VmAspace pointers held by Thread object are guaranteed to always be
  //    "alive" from a C++ perspective (the object has not been destructed, and
  //    the memory is still alive).  The actual VmAspace object is a ref-counted
  //    object which is guaranteed to be alive because:
  // 1a) It is a member of a thread's process (the main aspace, as well as the
  //     restricted mode aspace), and a thread's process object cannot be
  //     destroyed while the thread is still alive.  Or --
  // 1b) It is a special statically allocated aspace object, such as the EFI
  //     address space, or the kernel address space.
  // 2) Only the current thread is permitted to mutate its own aspace_ member.
  //    The only exception to this is during initialization, before the thread
  //    is running, by VmAspace::attach_to_thread.
  // 3) The currently running thread is allowed to access its own currently
  //    assigned aspace without needing any locks.
  // 4) Any other thread who wishes to access a thread's aspace must do so via
  //    Rust `Thread::get_aspace_ref` or `ChainLockProof::get_aspace_ref_locked`,
  //    which will return a RefPtr to the thread's current aspace as long as the
  //    thread has not entered the THREAD_DEATH state.
  // 5) Other threads who access a thread's aspace via those methods cannot
  //    assume that either the thread or the aspace is still alive after the
  //    thread's lock is dropped.  At best, they know that the RefPtr is keeping
  //    the VmAspace object alive from a C++ perspective, nothing more.
  // 6) The scheduler and low level context switch routines are allowed to
  //    directly access the aspace_ of both the old and new threads involved in the
  //    context switch.  Neither one is currently really running (the aspace_
  //    members cannot change), and both threads have to be alive enough that
  //    their process is still alive, and therefore so is their aspace_ (see
  //    #1).  Note; it is possible that the old thread involved in a context
  //    switch is in the process of dying, but we know that its process cannot
  //    exit (destroying its aspace in the process) until after it has context
  //    switched for the last time.

  // Returns the currently active address space, which is the address space currently hosting
  // page tables for the thread.
  //
  // The active address space should be used only when context switching. It should not be used for
  // resolving faults, as it may be a unified aspace that does not keep track of its own mappings.
  //
  // Kernel-only thread -- This will return nullptr, unless a caller has explicitly set aspace_
  // using Rust `thread::current_switch_aspace`, which is done by a few kernel unittests.
  //
  // User thread -- If the thread is in Restricted Mode, this will return the restricted aspace.
  // Otherwise, it will return the process's normal aspace.
  //
  // Note, the normal aspace is, by definition, the aspace that's active when a thread is in Normal
  // Mode.  All threads not in Restricted Mode are said to be in Normal Mode.  See
  // `ProcessDispatcher::normal_aspace()` for more information.
  VmAspace* active_aspace() { return aspace_; }
  const VmAspace* active_aspace() const { return aspace_; }

  const char* name() const { return rust_thread_name(this); }

  Linebuffer& linebuffer() { return linebuffer_; }

  using Canary = fbl::Canary<fbl::magic("thrd")>;

 private:
  // The architecture-specific methods for getting and setting the
  // current thread may need to see Thread's arch_ member via offsetof.
  friend inline Thread* arch_get_current_thread();
  friend inline void arch_set_current_thread(Thread*);

  // Packs a C++ callable (or nullptr) into a `(func, ctx)` pair for the Rust
  // `zr::SendInlineFnMut` FFI setter shims.
  template <typename... Args>
  struct CppCallback {
    using RawFn = void (*)(void* ctx, Args...);
    RawFn func = nullptr;
    void* ctx = nullptr;

    constexpr CppCallback() = default;
    constexpr explicit CppCallback(nullptr_t) {}

    template <typename F, typename Decayed = ktl::decay_t<F>,
              typename = ktl::enable_if_t<!ktl::is_same_v<Decayed, CppCallback> &&
                                          !ktl::is_same_v<Decayed, nullptr_t>>>
    explicit CppCallback(F&& f) {
      static_assert(sizeof(Decayed) <= sizeof(void*), "Callable too large for Thread callback");
      static_assert(alignof(Decayed) <= alignof(void*),
                    "Callable over-aligned for Thread callback");
      static_assert(ktl::is_trivially_copyable_v<Decayed>, "Callable must be trivially copyable");
      static_assert(ktl::is_trivially_destructible_v<Decayed>,
                    "Callable must be trivially destructible");
      __builtin_memcpy(&ctx, &f, sizeof(Decayed));
      func = [](void* c, Args... args) {
        alignas(Decayed) unsigned char buf[sizeof(Decayed)];
        __builtin_memcpy(buf, &c, sizeof(Decayed));
        (*reinterpret_cast<Decayed*>(buf))(args...);
      };
    }
  };

  // <kernel/thread_layout.h> exposes member offsets to the Rust mirror of this class.
  friend struct ThreadLayout;

  // LINT.IfChange(ThreadLayout)
  Canary canary_;

  // These fields are among the most active in the thread. They are grouped
  // together near the front to improve cache locality.
  __NO_UNIQUE_ADDRESS mutable fbl::NullLock scheduler_variable_lock_;
  // ktl::atomic is used for type parity with the Rust side (RelaxedAtomicU32 / RelaxedAtomicU64).
  ktl::atomic<uint32_t> flags_{};
  ktl::atomic<uint32_t> signals_;
  SchedulerState scheduler_state_ TA_GUARDED(get_lock());
  SchedulerQueueState scheduler_queue_state_ TA_GUARDED(scheduler_variable_lock_);
  WaitQueueCollection::ThreadState wait_queue_state_ TA_GUARDED(get_lock());
  // Rust-owned `OwnedWaitQueueList` (`fbl::DoublyLinkedList<NonNull<OwnedWaitQueue>>`),
  // initialized and dropped in Rust.
  [[maybe_unused]] OpaqueStorage<sizeof(void*), alignof(void*)> owned_wait_queues_
      TA_GUARDED(get_lock());
  TaskState task_state_;
  PreemptionState preemption_state_;
  MemoryAllocationState memory_allocation_state_;

  // Must only be accessed by "this" thread.
  RestrictedStatePtr restricted_state_;

#if WITH_LOCK_DEP
  // state for runtime lock validation when in thread context
  lockdep::ThreadLockState lock_state_;
#endif

  // The current address space this thread is associated with.
  // This can be null if this is a kernel thread.
  // See the comments near active_aspace() for more details.
  RelaxedAtomic<VmAspace*> aspace_{nullptr};

#ifdef UNITTESTS_ENABLED
  // Accessed with relaxed atomics from Rust (Thread::rust_ktrace_override).
  RelaxedAtomic<void*> rust_ktrace_override_{nullptr};
#endif

  // Saved by SignalPolicyException() to store the type of policy error, and
  // passed to exception disptach in ProcessPendingSignals().
  [[maybe_unused]] uint32_t extra_policy_exception_code_ TA_GUARDED(get_lock()) = 0;
  [[maybe_unused]] uint32_t extra_policy_exception_data_ TA_GUARDED(get_lock()) = 0;

  // Is this thread allowed to own wait queues?  Set to false by a thread as
  // it exits, but before it reaches the DEAD state.
  [[maybe_unused]] bool can_own_wait_queues_ TA_GUARDED(get_lock()) = true;

  // Strong reference to user thread if one exists for this thread.
  // In the common case freeing Thread will also free ThreadDispatcher when this
  // reference is dropped.
  //
  // Stored once by Rust (Thread::set_usermode_thread) during user-thread
  // setup and read lock-free via rust_thread_user_thread / Thread::user_thread.
  fbl::RefPtr<ThreadDispatcher> user_thread_;

  // When user_thread_ is set, these values are copied from ThreadDispatcher and
  // its parent ProcessDispatcher. Kernel threads maintain an independent tid.
  ktl::atomic<zx_koid_t> tid_{KernelObjectId::Generate()};
  ktl::atomic<zx_koid_t> pid_{ZX_KOID_INVALID};

  // Architecture-specific stuff.
  arch_thread arch_{};

  KernelStack stack_;

  // Per-thread deferred object deletion state (deferred_deleter and legacy
  // dispatcher.cc:SafeDeleter).
  void* recursive_object_deletion_list_ = nullptr;

  // This always includes the trailing NUL.
  [[maybe_unused]] char name_[ZX_MAX_NAME_LEN]{};

  // Buffering for Debuglog output.
  Linebuffer linebuffer_;

  // The flow id allocated before blocking on the last lock.
  RelaxedAtomic<uint64_t> lock_flow_id_{0};

  // Indicates whether user register state (debug, vector, fp regs, etc.) has been saved to the
  // arch_thread_t as part of thread suspension / exception handling.
  //
  // When a user thread is suspended or generates an exception (synthetic or architectural) that
  // might be observed by another process, we save user register state to the thread's arch_thread_t
  // so that it may be accessed by a debugger.  Upon leaving a suspended or exception state, we
  // restore user register state.
  [[maybe_unused]] bool user_state_saved_ TA_GUARDED(get_lock()){false};

  // For threads with migration functions, indicates whether a migration is in progress. When true,
  // the migrate function has been called with Before but not yet with After.
  //
  // TODO(johngro): What to do about this?  Should it be protected by the
  // thread's lock?  Does the thread's lock need to be held exclusively when a
  // thread's migrate function is called?
  [[maybe_unused]] bool migrate_pending_ TA_GUARDED(get_lock()){};

  // Provides a way to execute custom logic when a thread must be migrated between CPUs.
  // Rust-owned `Option<MigrateFn>` (`zr::SendInlineFnMut`), initialized and dropped in Rust.
  OpaqueStorage<kCallbackSize, kCallbackAlign> migrate_fn_ TA_GUARDED(get_lock());

  // Provides a way to execute custom logic when a thread is context switched to or away from.
  // Rust-owned `Option<ContextSwitchFn>` (`zr::SendInlineFnMut`), initialized and dropped in Rust.
  OpaqueStorage<kCallbackSize, kCallbackAlign> context_switch_fn_ TA_GUARDED(get_lock());

  // An Accessor for the thread's restartable sequence structure, if any.
  //
  // This is used to detect when a thread is preempted or enters a waiting state
  // while in a restartable sequence.  When this happens, the restartable sequence
  // is interrupted, and the thread will restart the sequence when it resumes.
  //
  // Kernel threads do not use restartable sequences, so this will always be !IsValid() for kernel
  // threads.
  //
  // Must be accessed only by the current thread.
  page_map::Accessor<zx_rseq_t> rseq_accessor_;

  // The sampling session this thread is associated with.
  static constexpr uint64_t kInvalidSamplingSession = -1;
  RelaxedAtomic<uint64_t> sampling_session_{kInvalidSamplingSession};

  // Used to track threads that have set |migrate_fn_|. This is used to migrate
  // threads before a CPU is taken offline.
  fbl::DoublyLinkedListNodeState<Thread*> migrate_list_node_ TA_GUARDED(get_list_lock());

  // Node storage for existing on the global thread list.
  fbl::DoublyLinkedListNodeState<Thread*> thread_list_node_ TA_GUARDED(get_list_lock());

  // Node storage for existing on the temporary batch unblock list.
  fbl::DoublyLinkedListNodeState<Thread*> unblock_list_node_ TA_GUARDED(get_lock());

  // Node storage for existing on the save state list.
  fbl::SinglyLinkedListNodeState<Thread*> save_state_list_node_ TA_GUARDED(get_lock());

  // State of the thread with respect to memory stalls.
  //
  // It can only be accessed by the thread's current CPU. Hence, preemption must
  // be disabled while accessing it.
  [[maybe_unused]] ThreadStallState memory_stall_state_ TA_GUARDED(preempt_disabled_token) =
      ThreadStallState::IgnoredKernelOnly;
  // LINT.ThenChange(
  //   //zircon/kernel/kernel/thread.rs:ThreadLayout,
  //   //zircon/kernel/include/kernel/thread_layout.h:ThreadLayout,
  // )
};

// For the moment, the arch-specific current thread implementations need to come here, after the
// Thread definition. One of the arches needs to know the structure of Thread to compute the offset
// that the hardware pointer holds into Thread.
#include <arch/current_thread.h>
Thread* Thread::Current::Get() { return arch_get_current_thread(); }

// Call the arch-specific signal handler.
extern "C" void arch_iframe_process_pending_signals(iframe_t* iframe);

////////////////////////////////////////////////////////////////////////////////
//
// Here and below, we need to deal with static analysis.  Right now, we need to
// disable static analysis while we figure it out.
//
////////////////////////////////////////////////////////////////////////////////

// WaitQueue collection trait implementations.  While typically these would be
// implemented in-band in the trait class itself, these definitions must come
// last, after the definition Thread.  This is because the traits (defined in
// WaitQueueCollection) need to understand the layout of Thread in order to be
// able to access both scheduler state and wait queue state variables.
inline WaitQueueCollection::Key WaitQueueCollection::BlockedThreadTreeTraits::GetKey(
    const Thread& thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  // TODO(johngro): consider extending FBL to support a "MultiWAVLTree"
  // implementation which would allow for nodes with identical keys, breaking
  // ties (under the hood) using pointer value.  This way, we would not need to
  // manifest our own pointer in GetKey or in our key type.
  return {thread.wait_queue_state().blocked_threads_tree_sort_key_,
          reinterpret_cast<uintptr_t>(&thread)};
}

inline fbl::WAVLTreeNodeState<Thread*>& WaitQueueCollection::BlockedThreadTreeTraits::node_state(
    Thread& thread) TA_NO_THREAD_SAFETY_ANALYSIS {
  return thread.wait_queue_state().blocked_threads_tree_node_;
}

inline void PreemptionState::AssertCapabilityHeld(
    concurrent::CapabilityToken<PreemptionState>::TagType) {
  DEBUG_ASSERT(Thread::Current::preemption_state().PreemptIsEnabled() == false);
}

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_THREAD_H_
