// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lock_dep_ffi.h"

#if WITH_LOCK_DEP

#include <zircon/assert.h>

#include <ktl/bit.h>
#include <ktl/type_traits.h>

#include <ktl/enforce.h>

zx_status_t TriggerAndWaitForLoopDetection(zx_instant_mono_t deadline) {
  return rust_lockdep_trigger_and_wait_for_loop_detection(deadline);
}

namespace lockdep {

void SystemLockValidationError(AcquiredLockEntry* lock_entry, AcquiredLockEntry* conflicting_entry,
                               ThreadLockState* state, void* caller_address, void* caller_frame,
                               LockResult result) {
  DEBUG_ASSERT(lock_entry != nullptr);
  DEBUG_ASSERT(conflicting_entry != nullptr);
  DEBUG_ASSERT(state != nullptr);
  rust_lockdep_system_lock_validation_error(lock_entry, conflicting_entry, state, caller_address,
                                            caller_frame, result);
}

void SystemLockValidationFatal(AcquiredLockEntry* lock_entry, ThreadLockState* state,
                               void* caller_address, void* caller_frame, LockResult result) {
  DEBUG_ASSERT(lock_entry != nullptr);
  DEBUG_ASSERT(state != nullptr);
  rust_lockdep_system_lock_validation_fatal(lock_entry, state, caller_address, caller_frame,
                                            result);
}

void SystemCircularLockDependencyDetected(ValidatorLockClassState* connected_set_root) {
  DEBUG_ASSERT(connected_set_root != nullptr);
  rust_lockdep_system_circular_lock_dependency_detected(connected_set_root);
}

ThreadLockState* SystemGetThreadLockState(LockFlags lock_flags) {
  return rust_lockdep_system_get_thread_lock_state(lock_flags);
}

void SystemInitThreadLockState(ThreadLockState* state) {
  DEBUG_ASSERT(state != nullptr);
  rust_lockdep_system_init_thread_lock_state(state);
}

void SystemTriggerLoopDetection() { rust_lockdep_system_trigger_loop_detection(); }

}  // namespace lockdep

extern "C" {

static_assert(ktl::is_empty_v<lockdep::LockClassState>);
static_assert(sizeof(lockdep::ValidatorLockClassState::Iterator) == sizeof(void*));
static_assert(alignof(lockdep::ValidatorLockClassState::Iterator) == alignof(void*));
static_assert(alignof(lockdep::MetadataLockClassState) == 8);
static_assert(alignof(lockdep::ValidatorLockClassState) == 8);
static_assert(alignof(lockdep::AcquiredLockEntry) == 8);
static_assert(sizeof(lockdep::LockFlags) == sizeof(uint16_t));
static_assert(alignof(lockdep::LockFlags) == alignof(uint16_t));
static_assert(lockdep::LockFlagsIrqSafe == (1u << 0u));
static_assert(sizeof(lockdep::LockResult) == sizeof(uint8_t));
static_assert(alignof(lockdep::LockResult) == alignof(uint8_t));
static_assert(lockdep::kMaxLockDependencies == 193);
static_assert(sizeof(lockdep::LockDependencySet) == lockdep::kMaxLockDependencies * sizeof(void*));
static_assert(alignof(lockdep::LockDependencySet) == alignof(void*));
static_assert(sizeof(lockdep::ThreadLockState) == 2 * sizeof(uintptr_t));
static_assert(alignof(lockdep::ThreadLockState) == alignof(uintptr_t));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE lockdep::ValidatorLockClassState* cpp_lockdep_iter_begin() {
  auto it = lockdep::ValidatorLockClassState::Iter().begin();
  return it != lockdep::ValidatorLockClassState::Iter().end() ? &*it : nullptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE lockdep::ValidatorLockClassState* cpp_lockdep_iter_next(
    lockdep::ValidatorLockClassState* state) {
  auto it = ktl::bit_cast<lockdep::ValidatorLockClassState::Iterator>(state);
  ++it;
  return it != lockdep::ValidatorLockClassState::Iter().end() ? &*it : nullptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const char* cpp_lockdep_state_name(const lockdep::LockClassState* state) {
  return lockdep::MetadataLockClassState::GetName(state);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const lockdep::LockDependencySet* cpp_lockdep_state_dependency_set(
    const lockdep::ValidatorLockClassState* state) {
  return &state->dependency_set();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE lockdep::ValidatorLockClassState* cpp_lockdep_state_connected_set(
    lockdep::ValidatorLockClassState* state) {
  return state->connected_set();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_lockdep_thread_lock_state_init(
    ffi::Uninitialized<lockdep::ThreadLockState>* out) {
  DEBUG_ASSERT(out != nullptr);
  out->Initialize();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_lockdep_thread_lock_state_destroy(lockdep::ThreadLockState* state) {
  DEBUG_ASSERT(state != nullptr);
  state->~ThreadLockState();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const lockdep::LockClassState* cpp_lockdep_entry_id(
    const lockdep::AcquiredLockEntry* entry) {
  DEBUG_ASSERT(entry != nullptr);
  return entry->id();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_lockdep_entry_order(const lockdep::AcquiredLockEntry* entry) {
  DEBUG_ASSERT(entry != nullptr);
  return entry->order();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const char* cpp_lockdep_result_to_string(lockdep::LockResult result) {
  return lockdep::ToString(result);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_lockdep_loop_detection_pass() { lockdep::LoopDetectionPass(); }

}  // extern "C"

#endif  // WITH_LOCK_DEP
