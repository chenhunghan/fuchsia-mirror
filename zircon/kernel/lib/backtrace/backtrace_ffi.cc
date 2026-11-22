// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/backtrace.h>
#include <lib/object-constants.h>
#include <stddef.h>

#include <ktl/type_traits.h>

#if defined(__x86_64__)
#include <lib/backtrace/global_cpu_context_exchange.h>

#include <arch/regs.h>
#include <kernel/ffi.h>
#endif

// The layout is shared with the Rust `Backtrace` in //zircon/kernel/lib/backtrace/src/mod.rs.
struct BacktraceLayoutCheck {
  static_assert(ktl::is_standard_layout_v<Backtrace>);
  static_assert(ktl::is_trivially_copyable_v<Backtrace>);
  static_assert(sizeof(Backtrace) == kBacktraceSize);
  static_assert(alignof(Backtrace) == kBacktraceAlign);
  static_assert(Backtrace::kMaxSize == 32);
  static_assert(offsetof(Backtrace, addr_) == 0);
  static_assert(offsetof(Backtrace, size_) == 256);
  static_assert(offsetof(Backtrace, first_frame_type_) == 264);
  static_assert(sizeof(Backtrace::FrameType) == 4);
  static_assert(alignof(Backtrace::FrameType) == 4);
  static_assert(Backtrace::FrameType::ReturnAddress == 0);
  static_assert(Backtrace::FrameType::PreciseLocation == 1);
};

#if defined(__x86_64__)

static_assert(sizeof(CpuContext) == 448);
static_assert(alignof(CpuContext) == 8);
static_assert(offsetof(CpuContext, frame) == 0);
static_assert(offsetof(CpuContext, backtrace) == 176);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_global_cpu_context_exchange_request_context(
    cpu_num_t target_cpu, zx_duration_mono_t timeout, CpuContext* context) {
  return g_cpu_context_exchange.RequestContext(target_cpu, timeout, *context);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_global_cpu_context_exchange_handle_request(uintptr_t fp,
                                                                      const iframe_t* frame) {
  g_cpu_context_exchange.HandleRequest(fp, *frame);
}

}  // extern "C"

#endif  // defined(__x86_64__)
