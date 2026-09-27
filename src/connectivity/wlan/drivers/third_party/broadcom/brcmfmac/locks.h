// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_LOCKS_H_
#define SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_LOCKS_H_

#include <zircon/compiler.h>

#include <shared_mutex>

// Because of missing thread annotations the standard library thread analysis doesn't work for
// std::shared_mutex when used with locks like std::scoped_lock, std::unique_lock etc. Provide a
// correctly annotated type here.
class __TA_SCOPED_CAPABILITY ScopedSharedWriteLock {
 public:
  explicit ScopedSharedWriteLock(std::shared_mutex& mutex) __TA_ACQUIRE(mutex) : mutex_(mutex) {
    mutex_.lock();
  }
  ~ScopedSharedWriteLock() __TA_RELEASE() { mutex_.unlock(); }

  ScopedSharedWriteLock(const ScopedSharedWriteLock&) = delete;
  ScopedSharedWriteLock(ScopedSharedWriteLock&&) = delete;
  ScopedSharedWriteLock& operator=(const ScopedSharedWriteLock&) = delete;
  ScopedSharedWriteLock& operator=(ScopedSharedWriteLock&&) = delete;

 private:
  std::shared_mutex& mutex_;
};

// For the same reason as above std::shared_lock doesn't work correctly with thread analysis.
// Provide a read lock, which acquires a shared lock.
class __TA_SCOPED_CAPABILITY ScopedSharedReadLock {
 public:
  explicit ScopedSharedReadLock(std::shared_mutex& mutex) __TA_ACQUIRE_SHARED(mutex)
      : mutex_(mutex) {
    mutex_.lock_shared();
  }
  ~ScopedSharedReadLock() __TA_RELEASE() { mutex_.unlock_shared(); }

  ScopedSharedReadLock(const ScopedSharedReadLock&) = delete;
  ScopedSharedReadLock(ScopedSharedReadLock&&) = delete;
  ScopedSharedReadLock& operator=(const ScopedSharedReadLock&) = delete;
  ScopedSharedReadLock& operator=(ScopedSharedReadLock&&) = delete;

 private:
  std::shared_mutex& mutex_;
};

#endif  // SRC_CONNECTIVITY_WLAN_DRIVERS_THIRD_PARTY_BROADCOM_BRCMFMAC_LOCKS_H_
