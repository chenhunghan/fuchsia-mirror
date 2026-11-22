// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_FFI_H_
#define ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_FFI_H_

// TODO(https://fxbug.dev/572853263): Remove once all C++ callers are ported to Rust.

#include <stdint.h>
#include <zircon/types.h>

namespace hypervisor {
template <uint32_t N>
class InterruptTracker;
}  // namespace hypervisor

extern "C" {

void rust_interrupt_tracker_init(hypervisor::InterruptTracker<256>* tracker);
void rust_interrupt_tracker_destroy(hypervisor::InterruptTracker<256>* tracker);
bool rust_interrupt_tracker_pending(const hypervisor::InterruptTracker<256>* tracker);
void rust_interrupt_tracker_clear(hypervisor::InterruptTracker<256>* tracker, uint32_t min,
                                  uint32_t max);
bool rust_interrupt_tracker_try_pop(hypervisor::InterruptTracker<256>* tracker, uint32_t vector);
bool rust_interrupt_tracker_pop(hypervisor::InterruptTracker<256>* tracker, uint32_t* vector_out);
zx_status_t rust_interrupt_tracker_track(hypervisor::InterruptTracker<256>* tracker,
                                         uint32_t vector);
zx_status_t rust_interrupt_tracker_interrupt(hypervisor::InterruptTracker<256>* tracker,
                                             uint32_t vector);
void rust_interrupt_tracker_cancel(hypervisor::InterruptTracker<256>* tracker);
zx_status_t rust_interrupt_tracker_wait(hypervisor::InterruptTracker<256>* tracker,
                                        zx_instant_mono_t deadline);

}  // extern "C"

#endif  // ZIRCON_KERNEL_HYPERVISOR_INCLUDE_HYPERVISOR_INTERRUPT_TRACKER_FFI_H_
