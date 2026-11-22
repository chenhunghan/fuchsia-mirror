// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/fsl/vmo/vector.h"

#include <zircon/syscalls.h>

#include <gtest/gtest.h>

namespace fsl {
namespace {

TEST(VmoVector, ShortVector) {
  std::vector<char> v(123, 'f');
  fsl::SizedVmo sb;
  EXPECT_TRUE(VmoFromVector(v, &sb));
  std::vector<char> v_out;
  EXPECT_TRUE(VectorFromVmo(std::move(sb), &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia::mem::Buffer buf;
  EXPECT_TRUE(VmoFromVector(v, &buf));
  EXPECT_TRUE(VectorFromVmo(buf, &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia_mem::Buffer natural_buf;
  EXPECT_TRUE(VmoFromVector(v, &natural_buf));
  EXPECT_TRUE(VectorFromVmo(natural_buf, &v_out));
  EXPECT_EQ(v, v_out);
}

TEST(VmoVector, EmptyVector) {
  std::vector<char> v;
  fsl::SizedVmo sb;
  EXPECT_TRUE(VmoFromVector(v, &sb));
  std::vector<char> v_out;
  EXPECT_TRUE(VectorFromVmo(std::move(sb), &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia::mem::Buffer buf;
  EXPECT_TRUE(VmoFromVector(v, &buf));
  EXPECT_TRUE(VectorFromVmo(buf, &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia_mem::Buffer natural_buf;
  EXPECT_TRUE(VmoFromVector(v, &natural_buf));
  EXPECT_TRUE(VectorFromVmo(natural_buf, &v_out));
  EXPECT_EQ(v, v_out);
}

TEST(VmoVector, ShortUnsignedVector) {
  std::vector<uint8_t> v(123, 42);
  fsl::SizedVmo sb;
  EXPECT_TRUE(VmoFromVector(v, &sb));
  std::vector<uint8_t> v_out;
  EXPECT_TRUE(VectorFromVmo(std::move(sb), &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia::mem::Buffer buf;
  EXPECT_TRUE(VmoFromVector(v, &buf));
  EXPECT_TRUE(VectorFromVmo(buf, &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia_mem::Buffer natural_buf;
  EXPECT_TRUE(VmoFromVector(v, &natural_buf));
  EXPECT_TRUE(VectorFromVmo(natural_buf, &v_out));
  EXPECT_EQ(v, v_out);
}

TEST(VmoVector, EmptyUnsignedVector) {
  std::vector<uint8_t> v;
  fsl::SizedVmo sb;
  EXPECT_TRUE(VmoFromVector(v, &sb));
  std::vector<uint8_t> v_out;
  EXPECT_TRUE(VectorFromVmo(std::move(sb), &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia::mem::Buffer buf;
  EXPECT_TRUE(VmoFromVector(v, &buf));
  EXPECT_TRUE(VectorFromVmo(buf, &v_out));
  EXPECT_EQ(v, v_out);

  ::fuchsia_mem::Buffer natural_buf;
  EXPECT_TRUE(VmoFromVector(v, &natural_buf));
  EXPECT_TRUE(VectorFromVmo(natural_buf, &v_out));
  EXPECT_EQ(v, v_out);
}

TEST(VmoVector, SizeLargerThanVmo) {
  std::vector<uint8_t> v(123, 42);
  std::vector<uint8_t> v_out;

  ::fuchsia::mem::Buffer buf;
  EXPECT_TRUE(VmoFromVector(v, &buf));
  buf.size = zx_system_get_page_size() + 1;
  EXPECT_FALSE(VectorFromVmo(buf, &v_out));

  ::fuchsia_mem::Buffer natural_buf;
  EXPECT_TRUE(VmoFromVector(v, &natural_buf));
  natural_buf.size() = zx_system_get_page_size() + 1;
  EXPECT_FALSE(VectorFromVmo(natural_buf, &v_out));
}

}  // namespace
}  // namespace fsl
