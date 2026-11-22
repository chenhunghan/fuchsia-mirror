// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/fsl/vmo/sized_vmo.h"

#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>

#include <gtest/gtest.h>

namespace fsl {
namespace {

constexpr uint64_t kSize = 42u;

zx::vmo MakeVmo() {
  zx::vmo vmo;
  EXPECT_EQ(ZX_OK, zx::vmo::create(kSize, 0u, &vmo));
  return vmo;
}

TEST(SizedVmoTest, ToTransport) {
  SizedVmo sized_vmo(MakeVmo(), kSize);
  ASSERT_TRUE(sized_vmo);

  ::fuchsia::mem::Buffer buffer = std::move(sized_vmo).ToTransport();
  EXPECT_TRUE(buffer.vmo.is_valid());
  EXPECT_EQ(kSize, buffer.size);

  // The vmo has been moved out of the SizedVmo; inspecting the moved-from
  // object is the point of this test.
  EXPECT_FALSE(sized_vmo);          // NOLINT(bugprone-use-after-move)
  EXPECT_EQ(0u, sized_vmo.size());  // NOLINT(bugprone-use-after-move)
}

TEST(SizedVmoTest, ToNaturalTransport) {
  SizedVmo sized_vmo(MakeVmo(), kSize);
  ASSERT_TRUE(sized_vmo);

  ::fuchsia_mem::Buffer buffer = std::move(sized_vmo).ToNaturalTransport();
  EXPECT_TRUE(buffer.vmo().is_valid());
  EXPECT_EQ(kSize, buffer.size());

  // The vmo has been moved out of the SizedVmo; inspecting the moved-from
  // object is the point of this test.
  EXPECT_FALSE(sized_vmo);          // NOLINT(bugprone-use-after-move)
  EXPECT_EQ(0u, sized_vmo.size());  // NOLINT(bugprone-use-after-move)
}

TEST(SizedVmoTest, NullToTransport) {
  ::fuchsia::mem::Buffer buffer = SizedVmo().ToTransport();
  EXPECT_FALSE(buffer.vmo.is_valid());
  EXPECT_EQ(0u, buffer.size);

  ::fuchsia_mem::Buffer natural_buffer = SizedVmo().ToNaturalTransport();
  EXPECT_FALSE(natural_buffer.vmo().is_valid());
  EXPECT_EQ(0u, natural_buffer.size());
}

TEST(SizedVmoTest, FromTransport) {
  SizedVmo sized_vmo;
  EXPECT_TRUE(SizedVmo::FromTransport(::fuchsia::mem::Buffer{MakeVmo(), kSize}, &sized_vmo));
  EXPECT_TRUE(sized_vmo);
  EXPECT_EQ(kSize, sized_vmo.size());

  SizedVmo natural_sized_vmo;
  EXPECT_TRUE(SizedVmo::FromTransport(::fuchsia_mem::Buffer(MakeVmo(), kSize), &natural_sized_vmo));
  EXPECT_TRUE(natural_sized_vmo);
  EXPECT_EQ(kSize, natural_sized_vmo.size());
}

TEST(SizedVmoTest, FromTransportSizeLargerThanVmo) {
  const uint64_t invalid_size = zx_system_get_page_size() + 1;

  SizedVmo sized_vmo;
  EXPECT_FALSE(
      SizedVmo::FromTransport(::fuchsia::mem::Buffer{MakeVmo(), invalid_size}, &sized_vmo));
  EXPECT_FALSE(sized_vmo);

  SizedVmo natural_sized_vmo;
  EXPECT_FALSE(
      SizedVmo::FromTransport(::fuchsia_mem::Buffer(MakeVmo(), invalid_size), &natural_sized_vmo));
  EXPECT_FALSE(natural_sized_vmo);
}

TEST(SizedVmoTest, RoundTrip) {
  SizedVmo sized_vmo(MakeVmo(), kSize);
  const zx_koid_t koid = [&] {
    zx_info_handle_basic_t info;
    EXPECT_EQ(ZX_OK, sized_vmo.vmo().get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr,
                                              nullptr));
    return info.koid;
  }();

  SizedVmo from_hlcpp;
  EXPECT_TRUE(SizedVmo::FromTransport(std::move(sized_vmo).ToTransport(), &from_hlcpp));
  EXPECT_EQ(kSize, from_hlcpp.size());

  SizedVmo from_natural;
  EXPECT_TRUE(SizedVmo::FromTransport(std::move(from_hlcpp).ToNaturalTransport(), &from_natural));
  EXPECT_EQ(kSize, from_natural.size());

  // The same vmo has been passed along unchanged.
  zx_info_handle_basic_t info;
  EXPECT_EQ(ZX_OK, from_natural.vmo().get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr,
                                               nullptr));
  EXPECT_EQ(koid, info.koid);
}

}  // namespace
}  // namespace fsl
