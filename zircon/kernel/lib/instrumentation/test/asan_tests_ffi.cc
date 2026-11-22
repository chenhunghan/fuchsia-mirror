// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <stdint.h>

#if __has_feature(address_sanitizer)

// TODO(https://fxbug.dev/542709218): Rust globals have ASan instrumentation temporarily
// disabled (`-asan-globals=0`), so keep this 8-byte global test buffer in C++ to exercise
// `__asan_register_globals` and `asan_register_globals_late`.
extern "C" uint8_t g_asan_test_global_buffer[8];
extern "C" uint8_t g_asan_test_global_buffer[8] = {};

#endif  // __has_feature(address_sanitizer)
