// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_H_

#include <lib/zx/result.h>
#include <zircon/types.h>

#include <fbl/macros.h>
#include <hypervisor/interrupt_tracker_ffi.h>
#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/spinlock.h>

namespace hypervisor {

// |N| is the maximum number of interrupts to be tracked.
template <uint32_t N>
class InterruptTracker {
 public:
  static_assert(N == 256, "InterruptTracker FFI expects N == 256");

  InterruptTracker() { rust_interrupt_tracker_init(this); }
  DISALLOW_COPY_ASSIGN_AND_MOVE(InterruptTracker);
  ~InterruptTracker() { rust_interrupt_tracker_destroy(this); }

  // Returns whether there are pending interrupts.
  bool Pending() const { return rust_interrupt_tracker_pending(this); }

  // Clears all vectors in the range [min, max).
  void Clear(uint32_t min, uint32_t max) { rust_interrupt_tracker_clear(this, min, max); }

  // Pops the specified vector, if it is pending.
  bool TryPop(uint32_t vector) { return rust_interrupt_tracker_try_pop(this, vector); }

  // Pops the highest priority interrupt.
  bool Pop(uint32_t* vector) { return rust_interrupt_tracker_pop(this, vector); }

  // Tracks the given interrupt.
  zx::result<> Track(uint32_t vector) {
    return zx::make_result(rust_interrupt_tracker_track(this, vector));
  }

  template <uint32_t Vector>
  void Track() {
    static_assert(Vector < N, "Vector out of bounds");
    [[maybe_unused]] zx::result<> result = Track(Vector);
  }

  // Tracks the given interrupt, and signals any waiters.
  zx::result<> Interrupt(uint32_t vector) {
    return zx::make_result(rust_interrupt_tracker_interrupt(this, vector));
  }

  template <uint32_t Vector>
  void Interrupt() {
    static_assert(Vector < N, "Vector out of bounds");
    [[maybe_unused]] zx::result<> result = Interrupt(Vector);
  }

  // Cancels a wait for an interrupt.
  //
  // We signal `ZX_ERR_INTERNAL_INTR_RETRY`, so that if the status is propagated
  // to the syscall-layer, we will retry the syscall.
  void Cancel() { rust_interrupt_tracker_cancel(this); }

  // Waits for an interrupt.
  zx::result<> Wait(zx_instant_mono_t deadline) {
    return zx::make_result(rust_interrupt_tracker_wait(this, deadline));
  }

 private:
  using LockType = DECLARE_SPINLOCK(InterruptTracker);
  static constexpr size_t kLockSize = (sizeof(LockType) + size_t{7}) & ~size_t{7};
  // Rust `InterruptBitmap<256>` stores four 64-bit words plus a `size` field (40 bytes).
  static constexpr size_t kBitmap256StorageSize = 40;
  static constexpr size_t kStorageSize =
      sizeof(AutounsignalEvent) + kLockSize + kBitmap256StorageSize;

  [[maybe_unused]] OpaqueStorage<kStorageSize, 8> storage_{};
  static_assert(sizeof(storage_) == kStorageSize);
  static_assert(alignof(decltype(storage_)) == 8);
};

}  // namespace hypervisor

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_H_
