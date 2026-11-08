// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef BUILD_BAZEL_RULES_CC_TESTS_INCLUDE_ZX_LIB_PUBLIC_H_
#define BUILD_BAZEL_RULES_CC_TESTS_INCLUDE_ZX_LIB_PUBLIC_H_

#include <lib/fit/function.h>

int zx_library_test_call(fit::function<int()> f);

#endif  // BUILD_BAZEL_RULES_CC_TESTS_INCLUDE_ZX_LIB_PUBLIC_H_
