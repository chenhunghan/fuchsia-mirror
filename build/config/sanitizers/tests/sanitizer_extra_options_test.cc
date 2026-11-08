// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstddef>
#include <cstdio>
#include <print>
#include <string_view>

// The runtime callback defined by sanitizer_default_options.c
// when DEFINE_SANITIZER_DEFAULT_OPTIONS=1.
// NOLINTNEXTLINE(bugprone-reserved-identifier)
extern "C" const char* __asan_default_options();

int main() {
  const char* options = __asan_default_options();
  if (options == nullptr) {
    std::println(stderr, "FAIL: __asan_default_options() returned null");
    return 1;
  }

  std::string_view opt_str(options);
  std::println("Resolved ASAN options string: \"{}\"", opt_str);

  // In sanitizer_default_options.c:
  // - The linker groups individual string contributions in link order.
  // - The runtime callback copies strings into a buffer in reverse order,
  //   separated by ':'.
  // - Sanitizer runtimes evaluate options left-to-right (last occurrence wins).
  // - Therefore, later dependencies override earlier ones by appearing later in opt_str.

  // Verify all expected option tokens are present.
  if (!opt_str.contains("detect_stack_use_after_return=1")) {
    std::println(stderr, "FAIL: Missing 'detect_stack_use_after_return=1'");
    return 1;
  }
  if (!opt_str.contains("quarantine_size_mb=32")) {
    std::println(stderr, "FAIL: Missing 'quarantine_size_mb=32'");
    return 1;
  }
  if (!opt_str.contains("detect_leaks=1")) {
    std::println(stderr, "FAIL: Missing 'detect_leaks=1'");
    return 1;
  }
  if (!opt_str.contains("quarantine_size_mb=48")) {
    std::println(stderr, "FAIL: Missing 'quarantine_size_mb=48'");
    return 1;
  }
  if (!opt_str.contains("quarantine_size_mb=64")) {
    std::println(stderr, "FAIL: Missing 'quarantine_size_mb=64'");
    return 1;
  }
  if (!opt_str.contains("verbosity=2")) {
    std::println(stderr, "FAIL: Missing 'verbosity=2'");
    return 1;
  }

  // Verify ordering:
  // - quarantine_size_mb=32 comes from default asan_default_options (dep of test_base_options).
  // - quarantine_size_mb=48 was defined in test_base_options.
  // - quarantine_size_mb=64 was defined in test_override_options (dep on test_base_options).
  // In the reversed string, higher-priority options must appear AFTER lower-priority ones
  // so that the sanitizer runtime will pick the override.
  const size_t pos_default = opt_str.find("quarantine_size_mb=32");
  const size_t pos_base = opt_str.find("quarantine_size_mb=48");
  const size_t pos_override = opt_str.find("quarantine_size_mb=64");

  if (pos_base <= pos_default) {
    std::println(stderr,
                 "FAIL: Expected base 'quarantine_size_mb=48' (pos {}) to appear AFTER "
                 "default 'quarantine_size_mb=32' (pos {})",
                 pos_base, pos_default);
    return 1;
  }

  if (pos_override <= pos_base) {
    std::println(stderr,
                 "FAIL: Expected override 'quarantine_size_mb=64' (pos {}) to appear AFTER "
                 "base 'quarantine_size_mb=48' (pos {})",
                 pos_override, pos_base);
    return 1;
  }

  std::println("PASS: Sanitizer options aggregation and reversal verified successfully.");
  return 0;
}
