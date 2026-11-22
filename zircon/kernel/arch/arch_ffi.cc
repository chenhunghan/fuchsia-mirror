// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>
#include <platform.h>
#include <sys/types.h>
#include <zircon/types.h>

#include <arch/arch_ops.h>
#include <arch/ops.h>
#include <kernel/ffi.h>
#include <kernel/percpu.h>
#include <kernel/thread.h>

// The Rust `arch_rs::InterruptSavedState`, passed across FFI by `cpp_arch_interrupt_save` and
// `cpp_arch_interrupt_restore`, must match `interrupt_saved_state_t`.
#ifdef __x86_64__
constexpr size_t kExpectedInterruptSavedStateSize = 8;
constexpr size_t kExpectedInterruptSavedStateAlign = 8;
#else
constexpr size_t kExpectedInterruptSavedStateSize = 1;
constexpr size_t kExpectedInterruptSavedStateAlign = 1;
#endif

static_assert(sizeof(interrupt_saved_state_t) == kExpectedInterruptSavedStateSize,
              "Rust InterruptSavedState size mismatch with C++");
static_assert(alignof(interrupt_saved_state_t) == kExpectedInterruptSavedStateAlign,
              "Rust InterruptSavedState alignment mismatch with C++");

extern "C" {

void cpp_arch_yield();
bool cpp_arch_blocking_disallowed();
bool cpp_arch_ints_disabled();
void cpp_arch_disable_ints();
void cpp_arch_enable_ints();
interrupt_saved_state_t cpp_arch_interrupt_save();
void cpp_arch_interrupt_restore(interrupt_saved_state_t state);
uint32_t cpp_arch_max_num_cpus();
void* cpp_arch_get_current_thread();
void cpp_arch_set_current_thread(void* thread);
void cpp_arch_set_restricted_flag(bool in_restricted);
bool cpp_with_frame_pointers();
void cpp_arch_set_blocking_disallowed(bool value);
__attribute__((returns_nonnull, assume_aligned(alignof(percpu)))) void* cpp_arch_get_curr_percpu();

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_arch_yield() { arch::Yield(); }
FFI_ALWAYS_INLINE bool cpp_arch_blocking_disallowed() { return arch_blocking_disallowed(); }
FFI_ALWAYS_INLINE bool cpp_arch_ints_disabled() { return arch_ints_disabled(); }
FFI_ALWAYS_INLINE void cpp_arch_disable_ints() { arch_disable_ints(); }
FFI_ALWAYS_INLINE void cpp_arch_enable_ints() { arch_enable_ints(); }
FFI_ALWAYS_INLINE interrupt_saved_state_t cpp_arch_interrupt_save() {
  return arch_interrupt_save();
}
FFI_ALWAYS_INLINE void cpp_arch_interrupt_restore(interrupt_saved_state_t state) {
  arch_interrupt_restore(state);
}
FFI_ALWAYS_INLINE uint32_t cpp_arch_max_num_cpus() { return arch_max_num_cpus(); }
FFI_ALWAYS_INLINE void* cpp_arch_get_current_thread() { return arch_get_current_thread(); }
FFI_ALWAYS_INLINE void cpp_arch_set_current_thread(void* thread) {
  arch_set_current_thread(static_cast<Thread*>(thread));
}
FFI_ALWAYS_INLINE void cpp_arch_set_restricted_flag(bool in_restricted) {
  arch_set_restricted_flag(in_restricted);
}
FFI_ALWAYS_INLINE bool cpp_with_frame_pointers() { return WITH_FRAME_POINTERS != 0; }
FFI_ALWAYS_INLINE void cpp_arch_set_blocking_disallowed(bool value) {
  arch_set_blocking_disallowed(value);
}
FFI_ALWAYS_INLINE __attribute__((returns_nonnull, assume_aligned(alignof(percpu)))) void*
cpp_arch_get_curr_percpu() {
  return arch_get_curr_percpu();
}
FFI_ALWAYS_INLINE uint32_t cpp_arch_vm_features() { return arch_vm_features(); }
FFI_ALWAYS_INLINE uint32_t cpp_arch_cpu_features() { return arch_cpu_features(); }
FFI_ALWAYS_INLINE uint32_t cpp_arch_cpu_sve_features() { return arch_cpu_sve_features(); }
FFI_ALWAYS_INLINE uint32_t cpp_arch_dcache_line_size() { return arch_dcache_line_size(); }
FFI_ALWAYS_INLINE uint32_t cpp_arch_icache_line_size() { return arch_icache_line_size(); }

}  // extern "C"
