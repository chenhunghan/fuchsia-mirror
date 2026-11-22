// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Intentionally use same-directory header lookup (emitting
// include_directories = "build/bazel/debug_symbols/tests" without "./") while
// a_main.cc uses the workspace-root path (emitting "./build/..." via -iquote .).
#include "a_library.h"

const char* a_library_file() { return __FILE__; }

int a_function() {
  volatile int non_const = static_cast<int>(a_library_file()[0] != '\0');
  return a_header_offset(non_const);
}
