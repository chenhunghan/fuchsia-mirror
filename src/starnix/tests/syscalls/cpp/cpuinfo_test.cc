// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/auxv.h>

#include <array>
#include <fstream>
#include <optional>
#include <sstream>
#include <string>

#include "gtest/gtest.h"

class CpuinfoTest : public ::testing::Test {};

TEST_F(CpuinfoTest, Cpuinfo) {
  std::fstream cpuinfo("/proc/cpuinfo");
  ASSERT_TRUE(cpuinfo.is_open());

  std::optional<std::string> features_line;
  std::string line;
  while (std::getline(cpuinfo, line)) {
    if (!features_line && line.starts_with("Features\t:")) {
      features_line = line;
    }
  }
  EXPECT_TRUE(cpuinfo.eof());

#if defined(__arm__) || defined(__aarch64__)
  std::stringstream ss;
#if defined(__arm__)
  static constexpr std::array<std::string_view, 28> hwcap_str = {
      "swp",      "half", "thumb",   "26bit",   "fastmult", "fpa",       "vfp",
      "edsp",     "java", "iwmmxt",  "crunch",  "thumbee",  "neon",      "vfpv3",
      "vfpv3d16", "tls",  "vfpv4",   "idiva",   "idivt",    "vfpd32",    "lpae",
      "evtstrm",  "fphp", "asimdhp", "asimddp", "asimdfhm", "asimdbf16", "i8mm",
  };

  static constexpr std::array<std::string_view, 7> hwcap2_str = {
      "aes", "pmull", "sha1", "sha2", "crc32", "sb", "ssbs",
  };
#elif defined(__aarch64__)
  static constexpr std::array<std::string_view, 32> hwcap_str = {
      "fp",      "asimd", "evtstrm", "aes",   "pmull",    "sha1",   "sha2", "crc32",
      "atomics", "fphp",  "asimdhp", "cpuid", "asimdrdm", "jscvt",  "fcma", "lrcpc",
      "dcpop",   "sha3",  "sm3",     "sm4",   "asimddp",  "sha512", "sve",  "asimdfhm",
      "dit",     "uscat", "ilrcpc",  "flagm", "ssbs",     "sb",     "paca", "pacg",
  };

  static constexpr std::array<std::string_view, 32> hwcap2_str = {
      "dcpodp",    "sve2",      "sveaes",  "svepmull",  "svebitperm", "svesha3",  "svesm4",
      "flagm2",    "frint",     "svei8mm", "svef32mm",  "svef64mm",   "svebf16",  "i8mm",
      "bf16",      "dgh",       "rng",     "bti",       "mte",        "ecv",      "afp",
      "rpres",     "mte3",      "sme",     "smei16i64", "smef64f64",  "smei8i32", "smef16f32",
      "smeb16f32", "smef32f32", "smefa64", "wfxt",
  };
#endif
  ss << "Features\t:";
  for (size_t i = 0; i < hwcap_str.size(); i++) {
    if (getauxval(AT_HWCAP) & (1UL << i)) {
      ss << " " << hwcap_str[i];
    }
  }
  for (size_t i = 0; i < hwcap2_str.size(); i++) {
    if (getauxval(AT_HWCAP2) & (1UL << i)) {
      ss << " " << hwcap2_str[i];
    }
  }
  const auto expected_features_line = ss.str();

  ASSERT_TRUE(features_line.has_value());
  EXPECT_EQ(*features_line, expected_features_line);
#endif
}
