// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/console.h>

#if CONSOLE_ENABLED

#include <lib/unittest/unittest.h>
#include <string.h>

extern const cmd __start_commands[];
extern const cmd __stop_commands[];

namespace {

// This test verifies that a command registered in Rust (via the `static_command!` macro
// in `tests/src/lib.rs`) is correctly placed in the `.data.rel.ro.commands` linker section
// and is visible from C++.
bool command_visibility_test() {
  BEGIN_TEST;

  bool found = false;
  for (const cmd* c = __start_commands; c != __stop_commands; c++) {
    if (strcmp(c->cmd_str, "mock_success") == 0) {
      found = true;
      EXPECT_EQ(0, strcmp("mock_success help", c->help_str));
      EXPECT_EQ(CMD_AVAIL_NORMAL, c->availability_mask);
      break;
    }
  }

  ASSERT_TRUE(found);

  END_TEST;
}

UNITTEST_START_TESTCASE(console_tests)
UNITTEST("test command visibility in C++", command_visibility_test)
UNITTEST_END_TESTCASE(console_tests, "console", "Tests for Console")

}  // namespace

#endif  // CONSOLE_ENABLED
