// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/escher_process_init.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"

int main(int argc, char** argv) {
#ifndef __Fuchsia__
  // The global `EscherEnvironment` starts Vulkan driver threads before any test runs, so the
  // default "fast" death-test style, which forks this process, risks deadlocking the child on locks
  // held by threads that do not exist in it. Re-execute the test binary only for each death test
  // instead. Set before `InitGoogleTest()` so `--gtest_death_test_style` can still override it.
  GTEST_FLAG_SET(death_test_style, "threadsafe");
#endif
  testing::InitGoogleTest(&argc, argv);
  escher::test::EscherEnvironment::RegisterGlobalTestEnvironment();
  int result = RUN_ALL_TESTS();

  return result;
}
