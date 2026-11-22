// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef LIB_MEMORY_BARRIERS_MEMORY_BARRIERS_H_
#define LIB_MEMORY_BARRIERS_MEMORY_BARRIERS_H_

// Hardware memory barriers for drivers.
//
// std::atomic_thread_fence only orders accesses between CPUs (dmb ish on
// AArch64), so it does not order CPU accesses against devices, DMA, or MMIO.
// The barriers here apply to the full system shareability domain because
// external devices and DMA engines are only guaranteed to participate in the
// full system domain.
//
// Choosing between DataMemoryBarrier* and DataSynchronizationBarrier*:
// - DataMemoryBarrier* (dmb on AArch64, ARMv8 ARM B2.3.5) orders explicit
//   memory accesses as observed by agents in the system domain without waiting
//   for access completion. Use these when all agents observe coherent or
//   uncached memory directly without cache maintenance (such as DMA descriptor
//   rings and ownership bits in memory), or between MMIO accesses to the same
//   device.
// - DataSynchronizationBarrier* (dsb on AArch64, ARMv8 ARM B2.3.5 and K11.5.4)
//   also waits on AArch64 for prior accesses to complete before subsequent
//   instructions execute. Per K11.5.4, prefer these over DMB when ordering
//   memory accesses against MMIO accesses that trigger or observe DMA, around
//   cache maintenance, or before syscalls that release DMA memory. On x86-64
//   and RISC-V, the two sets emit the same instructions.
//
// Use BarrierAfterFlush(), BarrierBeforeInvalidate(), and
// BarrierBeforeRelease() around cache maintenance and buffer release.

// #elifdef is C++23-only, and IDK headers must also compile as C++17 and C++20.
// NOLINTBEGIN(readability-use-concise-preprocessor-directives)

// Orders prior reads and writes before later reads and writes, including
// store-to-load. Does not order cache maintenance; use BarrierAfterFlush() or
// DataSynchronizationBarrier().
inline void DataMemoryBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dmb sy" : : : "memory");
#elif defined(__x86_64__)
  __asm__ volatile("mfence" : : : "memory");
#elif defined(__riscv)
  __asm__ volatile("fence iorw,iorw" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Orders prior reads before later reads and writes, such as reading a DMA
// ownership bit or ring index in coherent/uncached memory before reading the
// descriptor it guards.
inline void ReadDataMemoryBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dmb ld" : : : "memory");
#elif defined(__x86_64__)
  // lfence only adds ordering for WC and non-temporal loads, since TSO already
  // orders WB and UC loads.
  __asm__ volatile("lfence" : : : "memory");
#elif defined(__riscv)
  // ir,iorw rather than ir,ir to match dmb ld, which also orders later writes.
  __asm__ volatile("fence ir,iorw" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Orders prior writes before later writes, such as filling a DMA descriptor in
// coherent/uncached memory before setting its ownership bit. Use
// DataMemoryBarrier() to order writes before later reads.
inline void WriteDataMemoryBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dmb st" : : : "memory");
#elif defined(__x86_64__)
  // sfence only adds ordering for WC and non-temporal stores, since TSO
  // already orders WB and UC stores.
  __asm__ volatile("sfence" : : : "memory");
#elif defined(__riscv)
  __asm__ volatile("fence ow,ow" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Like DataMemoryBarrier(), but on AArch64 also waits for prior accesses and
// cache maintenance to complete (ARMv8 ARM B2.3.5, K11.5.4). Use around cache
// maintenance, when full read/write completion is needed with respect to MMIO,
// or before syscalls that hand DMA memory back to the kernel, such as
// zx_pmt_unpin() or zx_bti_release_quarantine().
inline void DataSynchronizationBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dsb sy" : : : "memory");
#elif defined(__x86_64__)
  __asm__ volatile("mfence" : : : "memory");
#elif defined(__riscv)
  __asm__ volatile("fence iorw,iorw" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Like ReadDataMemoryBarrier(), but on AArch64 also waits for prior reads to
// complete (ARMv8 ARM B2.3.5, K11.5.4), such as ordering an MMIO status read
// that signals DMA completion before reading coherent/uncached DMA memory, or
// finishing reads from a buffer before an MMIO write hands it back to
// hardware. Does not order cache maintenance; use BarrierBeforeInvalidate().
inline void ReadDataSynchronizationBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dsb ld" : : : "memory");
#elif defined(__x86_64__)
  // lfence only adds ordering for WC and non-temporal loads, since TSO already
  // orders WB and UC loads.
  __asm__ volatile("lfence" : : : "memory");
#elif defined(__riscv)
  // ir,iorw rather than ir,ir to match dsb ld, which also orders later writes.
  __asm__ volatile("fence ir,iorw" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Like WriteDataMemoryBarrier(), but on AArch64 also waits for prior writes to
// complete (ARMv8 ARM B2.3.5, K11.5.4), such as ordering coherent/uncached
// memory writes (for example, page-table or descriptor updates) before an MMIO
// doorbell write starts hardware access without a cache flush. On x86-64 and
// RISC-V it only orders writes before later writes, so it cannot replace
// DataSynchronizationBarrier() before syscalls, nor BarrierAfterFlush() after
// cache flushes.
inline void WriteDataSynchronizationBarrier() {
#ifdef __aarch64__
  __asm__ volatile("dsb st" : : : "memory");
#elif defined(__x86_64__)
  // sfence only adds ordering for WC and non-temporal stores, since TSO
  // already orders WB and UC stores.
  __asm__ volatile("sfence" : : : "memory");
#elif defined(__riscv)
  __asm__ volatile("fence ow,ow" : : : "memory");
#else
#error need definition for this platform
#endif
}

// Used after a cache flush of memory before an MMIO access makes it accessible
// to hardware. On AArch64, ARMv8 ARM K11.5.4 recommends DSB over DMB for
// ordering with respect to MMIO, and B2.3.5 requires both read and write
// access types (dsb sy) to order cache maintenance operations. On x86-64, even
// when cache-coherent DMA avoids CLFLUSH, mfence orders any prior non-temporal
// or string-operation stores before an MMIO write (or read) starts DMA.
inline void BarrierAfterFlush() { DataSynchronizationBarrier(); }

// Used after hardware has signaled that memory has data, before invalidating
// the cache. See ARMv8 ARM K11.5.1 for why a barrier is necessary before cache
// invalidation and B2.3.5 for why dsb sy is required for cache operations.
inline void BarrierBeforeInvalidate() { DataSynchronizationBarrier(); }

// Used after hardware has signaled it is done with a buffer, before releasing
// it. Often unnecessary when another implicit dependency orders the release;
// uses the full read/write barrier because neither how completion was observed
// nor the next operation on the buffer is fixed.
inline void BarrierBeforeRelease() { DataSynchronizationBarrier(); }

// NOLINTEND(readability-use-concise-preprocessor-directives)

#endif  // LIB_MEMORY_BARRIERS_MEMORY_BARRIERS_H_
