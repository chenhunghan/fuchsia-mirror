// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

extern int is_lib_built_without_pic();

int main() {
  // In both tests, the binary itself is compiled with "-pic" (either via
  // transitive_features = ["-pic"] or via features = ["-pic"] overriding
  // transitive_features = ["pic"]), so __PIE__ must be defined here.
#ifndef __PIE__
  return 1;
#endif

  if (is_lib_built_without_pic() != EXPECT_LIB_NO_PIC) {
    return 2;
  }
  return 0;
}
