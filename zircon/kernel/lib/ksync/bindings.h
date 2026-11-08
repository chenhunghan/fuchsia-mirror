// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_KSYNC_BINDINGS_H_
#define ZIRCON_KERNEL_LIB_KSYNC_BINDINGS_H_

#include <stdint.h>

#include <kernel/brwlock.h>
#include <lockdep/lockdep.h>

extern "C" {

void cpp_brwlock_pi_init(BrwLockPi* lock, const void* class_id);
void cpp_brwlock_pi_destroy(BrwLockPi* lock);
void cpp_brwlock_pi_acquire_read(BrwLockPi* lock);
void cpp_brwlock_pi_release_read(BrwLockPi* lock);
void cpp_brwlock_pi_acquire_write(BrwLockPi* lock);
void cpp_brwlock_pi_release_write(BrwLockPi* lock);
void cpp_brwlock_no_pi_init(BrwLockNoPi* lock, const void* class_id);
void cpp_brwlock_no_pi_destroy(BrwLockNoPi* lock);
void cpp_brwlock_no_pi_acquire_read(BrwLockNoPi* lock);
void cpp_brwlock_no_pi_release_read(BrwLockNoPi* lock);
void cpp_brwlock_no_pi_acquire_write(BrwLockNoPi* lock);
void cpp_brwlock_no_pi_release_write(BrwLockNoPi* lock);
void cpp_lock_dep_acquire(void* lock, const void* class_id, void* entry_storage,
                          lockdep::LockFlags flags, uintptr_t order, bool assert_nestable);
void cpp_lock_dep_release(void* entry_storage, lockdep::LockFlags flags);
void cpp_lock_validate_release(void* entry_storage, lockdep::LockFlags flags);
void cpp_lock_validate_acquire(void* entry_storage, lockdep::LockFlags flags);

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_KSYNC_BINDINGS_H_
