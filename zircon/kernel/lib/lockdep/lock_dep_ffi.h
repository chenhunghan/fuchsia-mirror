// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_LOCKDEP_LOCK_DEP_FFI_H_
#define ZIRCON_KERNEL_LIB_LOCKDEP_LOCK_DEP_FFI_H_

#if WITH_LOCK_DEP

#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <lockdep/lockdep.h>

// Wait for a loop detection pass to complete, or timeout. This is exposed for tests.
zx_status_t TriggerAndWaitForLoopDetection(zx_instant_mono_t deadline);

__BEGIN_CDECLS

lockdep::ValidatorLockClassState* cpp_lockdep_iter_begin();
lockdep::ValidatorLockClassState* cpp_lockdep_iter_next(lockdep::ValidatorLockClassState* state);
const char* cpp_lockdep_state_name(const lockdep::LockClassState* state);
const lockdep::LockDependencySet* cpp_lockdep_state_dependency_set(
    const lockdep::ValidatorLockClassState* state);
lockdep::ValidatorLockClassState* cpp_lockdep_state_connected_set(
    lockdep::ValidatorLockClassState* state);
void cpp_lockdep_thread_lock_state_init(ffi::Uninitialized<lockdep::ThreadLockState>* out);
void cpp_lockdep_thread_lock_state_destroy(lockdep::ThreadLockState* state);
const lockdep::LockClassState* cpp_lockdep_entry_id(const lockdep::AcquiredLockEntry* entry);
uintptr_t cpp_lockdep_entry_order(const lockdep::AcquiredLockEntry* entry);
const char* cpp_lockdep_result_to_string(lockdep::LockResult result);
void cpp_lockdep_loop_detection_pass();

zx_status_t rust_lockdep_trigger_and_wait_for_loop_detection(zx_instant_mono_t deadline);
void rust_lockdep_system_lock_validation_error(const lockdep::AcquiredLockEntry* lock_entry,
                                               const lockdep::AcquiredLockEntry* conflicting_entry,
                                               const lockdep::ThreadLockState* state,
                                               const void* caller_address, const void* caller_frame,
                                               lockdep::LockResult result);
void rust_lockdep_system_lock_validation_fatal(const lockdep::AcquiredLockEntry* lock_entry,
                                               const lockdep::ThreadLockState* state,
                                               const void* caller_address, const void* caller_frame,
                                               lockdep::LockResult result) __NO_RETURN;
void rust_lockdep_system_circular_lock_dependency_detected(
    const lockdep::ValidatorLockClassState* connected_set_root);
lockdep::ThreadLockState* rust_lockdep_system_get_thread_lock_state(lockdep::LockFlags lock_flags);
void rust_lockdep_system_init_thread_lock_state(const lockdep::ThreadLockState* state);
void rust_lockdep_system_trigger_loop_detection();

__END_CDECLS

#endif  // WITH_LOCK_DEP

#endif  // ZIRCON_KERNEL_LIB_LOCKDEP_LOCK_DEP_FFI_H_
