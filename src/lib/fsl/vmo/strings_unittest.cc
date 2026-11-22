// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/fsl/vmo/strings.h"

#include <zircon/syscalls.h>

#include <gtest/gtest.h>

namespace fsl {
namespace {

TEST(VmoStringsTest, ShortString) {
  const std::string hello_in_string = "Hello, world.";
  std::string hello_out_string;

  SizedVmo hello_sized_vmo;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_sized_vmo));
  EXPECT_TRUE(StringFromVmo(std::move(hello_sized_vmo), &hello_out_string));
  EXPECT_EQ(hello_in_string, hello_out_string);

  ::fuchsia::mem::Buffer hello_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_buffer));
  EXPECT_TRUE(StringFromVmo(hello_buffer, &hello_out_string));
  EXPECT_EQ(hello_in_string, hello_out_string);

  ::fuchsia_mem::Buffer hello_natural_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_natural_buffer));
  EXPECT_TRUE(StringFromVmo(hello_natural_buffer, &hello_out_string));
  EXPECT_EQ(hello_in_string, hello_out_string);
}

TEST(VmoStringsTest, EmptyString) {
  const std::string empty_in_string = "";
  std::string empty_out_string;

  SizedVmo empty_sized_vmo;
  EXPECT_TRUE(VmoFromString(empty_in_string, &empty_sized_vmo));
  EXPECT_TRUE(StringFromVmo(std::move(empty_sized_vmo), &empty_out_string));
  EXPECT_EQ(empty_in_string, empty_out_string);

  ::fuchsia::mem::Buffer empty_buffer;
  EXPECT_TRUE(VmoFromString(empty_in_string, &empty_buffer));
  EXPECT_TRUE(StringFromVmo(empty_buffer, &empty_out_string));
  EXPECT_EQ(empty_in_string, empty_out_string);

  ::fuchsia_mem::Buffer empty_natural_buffer;
  EXPECT_TRUE(VmoFromString(empty_in_string, &empty_natural_buffer));
  EXPECT_TRUE(StringFromVmo(empty_natural_buffer, &empty_out_string));
  EXPECT_EQ(empty_in_string, empty_out_string);
}

TEST(VmoStringsTest, BinaryString) {
  std::string binary_in_string(10, '\0');
  for (size_t i = 0; i < binary_in_string.size(); i++) {
    binary_in_string[i] = (char)i;
  }
  std::string binary_out_string;

  SizedVmo binary_sized_vmo;
  EXPECT_TRUE(VmoFromString(binary_in_string, &binary_sized_vmo));
  EXPECT_TRUE(StringFromVmo(std::move(binary_sized_vmo), &binary_out_string));
  EXPECT_EQ(binary_in_string, binary_out_string);

  ::fuchsia::mem::Buffer binary_buffer;
  EXPECT_TRUE(VmoFromString(binary_in_string, &binary_buffer));
  EXPECT_TRUE(StringFromVmo(binary_buffer, &binary_out_string));
  EXPECT_EQ(binary_in_string, binary_out_string);

  ::fuchsia_mem::Buffer binary_natural_buffer;
  EXPECT_TRUE(VmoFromString(binary_in_string, &binary_natural_buffer));
  EXPECT_TRUE(StringFromVmo(binary_natural_buffer, &binary_out_string));
  EXPECT_EQ(binary_in_string, binary_out_string);
}

TEST(VmoStringsTest, Prefix) {
  const std::string hello_in_string = "Hello, world.";
  const std::string hello_prefix = "Hello";
  std::string hello_out_string;

  ::fuchsia::mem::Buffer hello_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_buffer));
  EXPECT_TRUE(StringFromVmo(hello_buffer, hello_prefix.size(), &hello_out_string));
  EXPECT_EQ(hello_prefix, hello_out_string);

  ::fuchsia_mem::Buffer hello_natural_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_natural_buffer));
  EXPECT_TRUE(StringFromVmo(hello_natural_buffer, hello_prefix.size(), &hello_out_string));
  EXPECT_EQ(hello_prefix, hello_out_string);
}

TEST(VmoStringsTest, SizeLargerThanVmo) {
  const std::string hello_in_string = "Hello, world.";
  std::string hello_out_string;

  ::fuchsia::mem::Buffer hello_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_buffer));
  hello_buffer.size = zx_system_get_page_size() + 1;
  EXPECT_FALSE(StringFromVmo(hello_buffer, &hello_out_string));
  EXPECT_FALSE(StringFromVmo(hello_buffer, hello_buffer.size, &hello_out_string));

  ::fuchsia_mem::Buffer hello_natural_buffer;
  EXPECT_TRUE(VmoFromString(hello_in_string, &hello_natural_buffer));
  hello_natural_buffer.size() = zx_system_get_page_size() + 1;
  EXPECT_FALSE(StringFromVmo(hello_natural_buffer, &hello_out_string));
  EXPECT_FALSE(StringFromVmo(hello_natural_buffer, hello_natural_buffer.size(), &hello_out_string));
}

}  // namespace
}  // namespace fsl
