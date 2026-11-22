// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Tests the `fostr_fidl()` Bazel macro: the generated header is included at
// the same path as in GN, and the generated code formats FIDL types.

#include <iostream>
#include <sstream>
#include <string>

#include "src/lib/fostr/build/tests/formatting.h"

namespace {

template <typename T>
bool FormatsAs(const T& value, const std::string& expected) {
  std::ostringstream os;
  os << value;
  if (os.str() != expected) {
    std::cerr << "Expected \"" << expected << "\", got \"" << os.str() << "\"\n";
    return false;
  }
  return true;
}

}  // namespace

int main() {
  using fuchsia::example::fostr::ExampleBits;
  using fuchsia::example::fostr::ExampleEnum;
  using fuchsia::example::fostr::MyUnion;

  MyUnion u;
  u.set_i(5);

  bool ok = true;
  ok &= FormatsAs(ExampleEnum::BAR_BAZ, "bar baz");
  ok &= FormatsAs(ExampleBits::A | ExampleBits::C, "a|c");
  ok &= FormatsAs(u, "i 5");
  return ok ? 0 : 1;
}
