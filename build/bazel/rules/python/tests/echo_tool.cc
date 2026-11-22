// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <iostream>

int main(int argc, char** argv) {
  std::cout << "echo_tool:" << (argc > 1 ? argv[1] : "") << '\n';
  return 0;
}
