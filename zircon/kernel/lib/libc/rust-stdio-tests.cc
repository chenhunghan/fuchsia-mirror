// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <stdint.h>
#include <stdio.h>
#include <string-file.h>

#include <ktl/array.h>
#include <ktl/utility.h>

extern "C" {  // Defined in Rust.

bool write_to_stdio_in_rust(FILE*);
bool write_to_stdio_with_nul_in_rust(FILE*);

}  // extern "C"

namespace {

static_assert(sizeof(FILE) == 16);
static_assert(alignof(FILE) == 8);

constexpr ktl::string_view kHello = "Hello world!";
constexpr ktl::string_view kHelNo = "Hello\0world!";

template <bool (*Rust)(FILE*), const ktl::string_view& TestString>
bool rust_writes_to_stdio_test() {
  BEGIN_TEST;
  ktl::array<char, TestString.size() + 1> buffer;
  ktl::string_view str;
  {
    StringFile file{buffer};
    EXPECT_TRUE(Rust(&file));
    ktl::span chars = ktl::move(file).take();
    str = {chars.data(), chars.size()};
    if (!str.empty()) {
      EXPECT_EQ(str.back(), '\0');
      str.remove_suffix(1);
    }
  }
  EXPECT_EQ(str.size(), TestString.size());
  EXPECT_TRUE(str == TestString);
  END_TEST;
}

bool rust_stdio_error_test() {
  BEGIN_TEST;
  FILE fails{[](ktl::string_view str) { return -1; }};
  EXPECT_FALSE(write_to_stdio_in_rust(&fails));
  END_TEST;
}

UNITTEST_START_TESTCASE(rust_stdio_tests)
UNITTEST("test Rust writing to a FILE*",
         (rust_writes_to_stdio_test<write_to_stdio_in_rust, kHello>))
UNITTEST("test Rust writing to a FILE* with embedded NUL",
         (rust_writes_to_stdio_test<write_to_stdio_with_nul_in_rust, kHelNo>))
UNITTEST("test Rust getting error from FILE* write", rust_stdio_error_test)
UNITTEST_END_TESTCASE(rust_stdio_tests, "rust-stdio", "Tests for Rust libc::stdio")

}  // namespace
