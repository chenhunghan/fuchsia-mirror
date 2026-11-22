// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <ctype.h>
#include <stdio.h>

#include <vector>

#include "build/bazel/debug_symbols/tests/a_library.h"
#include "build/bazel/debug_symbols/tests/generated_header.h"

// Defined in a_asm.S and a_raw_asm.s. main() calls them so that the linker
// pulls in both assembly objects, whose DWARF CUs check_debug_binaries.py
// verifies.
extern "C" void a_asm_func();
extern "C" void a_raw_asm_func();

int main() {
  char buffer[16] = "Hello world!";
  for (char& ch : buffer) {
    ch = static_cast<char>(::toupper(static_cast<unsigned char>(ch)));
  }
  a_asm_func();
  a_raw_asm_func();
  std::vector<int> values = {
      a_function(),
      a_header_offset(static_cast<unsigned char>(buffer[1])),
      generated_header_value(static_cast<unsigned char>(buffer[0])),
  };
  fprintf(stdout, "%s (%s): %s (%d)\n", __FILE__, a_library_file(), buffer,
          values[0] + values[1] + values[2]);
  return 0;
}
