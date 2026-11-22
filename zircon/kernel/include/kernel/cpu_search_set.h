// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_INCLUDE_KERNEL_CPU_SEARCH_SET_H_
#define ZIRCON_KERNEL_INCLUDE_KERNEL_CPU_SEARCH_SET_H_

#include <stddef.h>

#include <kernel/ffi.h>

// CpuSearchSet is a cache/cluster-aware search list that minimizes cache
// crossings and maximizes remote CPU access distribution when searching for a
// target CPU to place a task.
//
// Implemented in Rust (//zircon/kernel/kernel/cpu_search_set.rs). C++ only
// reserves the storage so `struct percpu` can embed `CpuSearchSet` by value;
// //zircon/kernel/kernel/percpu.rs and cpu_search_set.rs assert layout parity.
class CpuSearchSet {
 public:
  CpuSearchSet() = delete;
  ~CpuSearchSet() = delete;

  CpuSearchSet(const CpuSearchSet&) = delete;
  CpuSearchSet& operator=(const CpuSearchSet&) = delete;
  CpuSearchSet(CpuSearchSet&&) = delete;
  CpuSearchSet& operator=(CpuSearchSet&&) = delete;

 private:
  static constexpr size_t kEntrySize = 2 * sizeof(size_t);
  static constexpr size_t kStorageAlign = alignof(size_t);
  // Matches `CpuSearchSetInner` (`cpu_count`, `ordered_cpus[SMP_MAX_CPUS]`,
  // and `this_cpu` plus trailing alignment padding, rounded up to
  // `kStorageAlign`).
  static constexpr size_t kStorageSize =
      sizeof(size_t) + (kEntrySize * SMP_MAX_CPUS) + kStorageAlign;

  [[maybe_unused]] OpaqueStorage<kStorageSize, kStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_INCLUDE_KERNEL_CPU_SEARCH_SET_H_
