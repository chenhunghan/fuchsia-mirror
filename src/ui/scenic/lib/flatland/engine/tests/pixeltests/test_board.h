// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_TESTS_PIXELTESTS_TEST_BOARD_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_TESTS_PIXELTESTS_TEST_BOARD_H_

#include <string_view>

namespace flatland::test {

// Returns the name of the board the pixeltests are running on, as passed to
// the test binary via the `--board=<name>` command-line argument (typically
// set in the test component's manifest `program.args`).
//
// Returns "default" if no board was specified.
std::string_view GetTestBoardName();

}  // namespace flatland::test

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_TESTS_PIXELTESTS_TEST_BOARD_H_
