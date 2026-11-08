// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/auxv.h>
#include <sys/prctl.h>

#include <gtest/gtest.h>

#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"

int main(int argc, char** argv) {
  bool expect_at_secure_set = atoi(argv[1]);
  EXPECT_EQ(getauxval(AT_SECURE), expect_at_secure_set);

  int expected_pdeathsig = argc > 2 ? atoi(argv[2]) : 0;
  int actual_pdeathsig = -1;
  EXPECT_THAT(prctl(PR_GET_PDEATHSIG, &actual_pdeathsig), SyscallSucceeds());
  EXPECT_EQ(actual_pdeathsig, expected_pdeathsig);

  return ::testing::Test::HasFailure() ? 1 : 0;
}
