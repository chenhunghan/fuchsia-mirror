// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cstdint>
#include <fstream>
#include <string>
#include <vector>

#include <gtest/gtest.h>

#include "visitor_registry.h"

namespace {

std::vector<uint8_t> ReadFileBytes(const std::string& path) {
  std::ifstream file(path, std::ios::binary | std::ios::ate);
  EXPECT_TRUE(file.is_open()) << "Failed to open " << path;
  if (!file.is_open()) {
    return {};
  }
  const std::streamsize size = file.tellg();
  EXPECT_GT(size, 4);
  if (size <= 0) {
    return {};
  }
  file.seekg(0, std::ios::beg);
  std::vector<uint8_t> bytes(static_cast<size_t>(size));
  EXPECT_TRUE(file.read(reinterpret_cast<char*>(bytes.data()), size));
  return bytes;
}

TEST(DevicetreeTest, StaticVisitorConstructorRan) { EXPECT_EQ(GetRegisteredVisitorCount(), 1); }

TEST(DevicetreeTest, SimpleDtbHasValidFdtMagic) {
  const std::vector<uint8_t> bytes = ReadFileBytes("test_data/simple.dtb");
  ASSERT_GT(bytes.size(), 4u);
  EXPECT_EQ(bytes[0], 0xd0);
  EXPECT_EQ(bytes[1], 0x0d);
  EXPECT_EQ(bytes[2], 0xfe);
  EXPECT_EQ(bytes[3], 0xed);
}

TEST(DevicetreeTest, PreprocessedDtbHasValidMagicAndDefine) {
  const std::vector<uint8_t> bytes = ReadFileBytes("test_data/preprocessed.dtb");
  ASSERT_GT(bytes.size(), 4u);
  EXPECT_EQ(bytes[0], 0xd0);
  EXPECT_EQ(bytes[1], 0x0d);
  EXPECT_EQ(bytes[2], 0xfe);
  EXPECT_EQ(bytes[3], 0xed);

  // Verify the TEST_DEP_DEFINE preprocessor branch and /include/ fragment were included in the DTB.
  std::string content(bytes.begin(), bytes.end());
  EXPECT_NE(content.find("okay"), std::string::npos);
  EXPECT_NE(content.find("fragment-node"), std::string::npos);
}

}  // namespace
