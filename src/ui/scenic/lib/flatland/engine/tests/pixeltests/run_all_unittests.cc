// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <string>
#include <string_view>

#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/scenic/lib/flatland/engine/tests/pixeltests/test_board.h"

namespace flatland::test {
namespace {

std::string& TestBoardName() {
  static std::string board_name = "default";
  return board_name;
}

}  // namespace

std::string_view GetTestBoardName() { return TestBoardName(); }

}  // namespace flatland::test

int main(int argc, char** argv) {
  // Strips all gtest flags from `argv`.
  testing::InitGoogleTest(&argc, argv);

  constexpr std::string_view kBoardFlag = "--board=";
  for (int i = 1; i < argc; ++i) {
    std::string_view arg(argv[i]);
    if (arg.starts_with(kBoardFlag)) {
      flatland::test::TestBoardName() = std::string(arg.substr(kBoardFlag.size()));
    }
  }

  escher::test::EscherEnvironment::RegisterGlobalTestEnvironment();
  int result = RUN_ALL_TESTS();
  return result;
}
