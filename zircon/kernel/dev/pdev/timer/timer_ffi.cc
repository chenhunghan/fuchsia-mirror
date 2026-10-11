// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
//
// Ported from zircon/kernel/dev/pdev/timer/timer.cc

#include <zircon/time.h>
#include <zircon/types.h>

#include <dev/timer.h>
#include <kernel/ffi.h>
#include <pdev/timer.h>

extern "C" {

zx_ticks_t rust_timer_current_ticks();
zx_status_t rust_timer_set_oneshot_timer(zx_ticks_t deadline);
zx_status_t rust_timer_stop();
zx_status_t rust_timer_shutdown();

void rust_pdev_register_timer(const pdev_timer_ops* ops);

}  // extern "C"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_ticks_t timer_current_ticks() { return rust_timer_current_ticks(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t timer_set_oneshot_timer(zx_ticks_t deadline) {
  return rust_timer_set_oneshot_timer(deadline);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t timer_stop() { return rust_timer_stop(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t timer_shutdown() { return rust_timer_shutdown(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void pdev_register_timer(const pdev_timer_ops* ops) {
  rust_pdev_register_timer(ops);
}
