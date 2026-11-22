// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "build/bazel/rules/python/tests/c_extension/shared_helper.h"

__attribute__((visibility("default"))) int64_t c_ext_shared_mul(int a, int b) {
  return static_cast<int64_t>(a) * b;
}
