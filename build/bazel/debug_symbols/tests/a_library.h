// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BAZEL_DEBUG_SYMBOLS_TESTS_A_LIBRARY_H_
#define BUILD_BAZEL_DEBUG_SYMBOLS_TESTS_A_LIBRARY_H_

extern int a_function() __attribute__((visibility("default")));
extern const char* a_library_file() __attribute__((visibility("default")));

inline __attribute__((noinline)) int a_header_offset(int x) { return x + 42; }

#endif  // BUILD_BAZEL_DEBUG_SYMBOLS_TESTS_A_LIBRARY_H_
