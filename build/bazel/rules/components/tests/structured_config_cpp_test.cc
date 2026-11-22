// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Verify both quoted and angle-bracketed include forms, uppercase/punctuation
// normalization (`Config-A` -> `config_a`, `Custom.Ns` -> `custom_ns`), and
// multiple `fuchsia_structured_config_cpp_elf_lib()` targets in the same
// package (ensuring distinct header include guards and internal FIDL bindings).
#include <build/bazel/rules/components/tests/custom_ns.h>

#include "build/bazel/rules/components/tests/config_a.h"

bool CheckStructuredConfigCpp() {
  config_a::Config a;
  custom_ns::Config b;
  return a.flag_a() || (b.count_b() > 0);
}
