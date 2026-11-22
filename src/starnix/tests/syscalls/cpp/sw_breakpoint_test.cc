// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/starnix/tests/syscalls/cpp/test_helper.h"

TEST(SwBreakpointTest, Int3CompactGeneratesSIGTRAP) {
  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([] {
    test_helper::ScopedSigAction scoped_sigtrap(SIGTRAP,
                                                [](int signo, siginfo_t* info, void* ucontext) {
                                                  if (signo == SIGTRAP) {
                                                    _exit(EXIT_SUCCESS);
                                                  }
                                                  _exit(EXIT_FAILURE);
                                                });
    __builtin_debugtrap();
    ADD_FAILURE() << "Expected to generate SIGTRAP.";
    exit(EXIT_FAILURE);
  });
  ASSERT_TRUE(helper.WaitForChildren());
}
