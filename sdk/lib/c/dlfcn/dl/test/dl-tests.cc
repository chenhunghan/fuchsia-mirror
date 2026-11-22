// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "../diagnostics.h"

namespace {

// This file contains only the tests that should work on any platform.  These
// tests don't involve anything that needs an ELF file built, so they can run
// on non-ELF hosts.

TEST(DlTests, Diagnostics) {
  {
    dl::Diagnostics diag;
    fit::result<dl::Error, void*> result = diag.ok(nullptr);
    ASSERT_TRUE(result.is_ok()) << result.error_value();
    EXPECT_EQ(result.value(), nullptr);
  }
  {
    dl::Diagnostics diag;
    fit::result<dl::Error> result = diag.ok();
    EXPECT_TRUE(result.is_ok()) << result.error_value();
  }
  {
    dl::Diagnostics diag;
    // Injects the prefix on diag.FormatError while in scope.
    ld::ScopedModuleDiagnostics module_diag{diag, "foo"};
    EXPECT_FALSE(diag.FormatError("some error", elfldltl::FileOffset{0x123u}));
    fit::result<dl::Error, int> result = diag.take_error();
    EXPECT_TRUE(result.is_error());
    EXPECT_EQ(result.error_value().take_str(), "foo: some error at file offset 0x123");
  }
  {
    dl::Diagnostics diag;
    {
      // No effect after it goes out of scope again.
      ld::ScopedModuleDiagnostics module_diag{diag, "foo"};
    }
    EXPECT_FALSE(diag.FormatError("some error"));
    fit::result<dl::Error> result = diag.take_error();
    EXPECT_TRUE(result.is_error());
    EXPECT_EQ(result.error_value().take_str(), "some error");
  }
}

}  // namespace
