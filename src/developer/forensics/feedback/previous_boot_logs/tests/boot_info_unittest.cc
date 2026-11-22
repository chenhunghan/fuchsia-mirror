// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/previous_boot_logs/boot_info.h"

#include <lib/zx/time.h>

#include <optional>
#include <string>

#include <gtest/gtest.h>

namespace forensics::feedback {
namespace {

using ::testing::TestParamInfo;
using ::testing::TestWithParam;
using ::testing::ValuesIn;

struct BootInfoRoundTripParam {
  std::string test_name;
  BootInfo info;
};

using BootInfoRoundTripTest = TestWithParam<BootInfoRoundTripParam>;

TEST_P(BootInfoRoundTripTest, RoundTripsJson) {
  const BootInfoRoundTripParam& params = GetParam();
  const std::string json = params.info.ToJson();
  const std::optional<BootInfo> parsed = BootInfo::FromJson(json);
  ASSERT_TRUE(parsed.has_value());
  EXPECT_EQ(*parsed, params.info);
}

INSTANTIATE_TEST_SUITE_P(BootInfoRoundTripTests, BootInfoRoundTripTest,
                         ValuesIn<BootInfoRoundTripParam>({
                             {
                                 .test_name = "AllFields",
                                 .info =
                                     BootInfo{
                                         .boot_id = "boot-id-1",
                                         .uptime = zx::msec(36000000),
                                         .utc_boot_difference = zx::nsec(1726053894000000000),
                                     },
                             },
                             {
                                 .test_name = "MissingBootId",
                                 .info =
                                     BootInfo{
                                         .boot_id = std::nullopt,
                                         .uptime = zx::msec(12000),
                                         .utc_boot_difference = zx::nsec(987654321),
                                     },
                             },
                             {
                                 .test_name = "MissingUptime",
                                 .info =
                                     BootInfo{
                                         .boot_id = "boot-id-2",
                                         .uptime = std::nullopt,
                                         .utc_boot_difference = zx::nsec(987654321),
                                     },
                             },
                             {
                                 .test_name = "MissingUtcBootDifference",
                                 .info =
                                     BootInfo{
                                         .boot_id = "boot-id-3",
                                         .uptime = zx::msec(5000),
                                         .utc_boot_difference = std::nullopt,
                                     },
                             },
                             {
                                 .test_name = "NoFields",
                                 .info =
                                     BootInfo{
                                         .boot_id = std::nullopt,
                                         .uptime = std::nullopt,
                                         .utc_boot_difference = std::nullopt,
                                     },
                             },
                         }),
                         [](const TestParamInfo<BootInfoRoundTripParam>& info) {
                           return info.param.test_name;
                         });

struct InvalidBootInfoJsonParam {
  std::string test_name;
  std::string json;
};

using InvalidBootInfoJsonTest = TestWithParam<InvalidBootInfoJsonParam>;

TEST_P(InvalidBootInfoJsonTest, RejectsInvalidJson) {
  EXPECT_EQ(BootInfo::FromJson(GetParam().json), std::nullopt);
}

INSTANTIATE_TEST_SUITE_P(InvalidBootInfoJsonTests, InvalidBootInfoJsonTest,
                         ValuesIn<InvalidBootInfoJsonParam>({
                             {
                                 .test_name = "Empty",
                                 .json = "",
                             },
                             {
                                 .test_name = "MalformedJson",
                                 .json = R"({"boot_id": "abc")",
                             },
                             {
                                 .test_name = "NotAnObject",
                                 .json = R"(["boot_id"])",
                             },
                             {
                                 .test_name = "WrongBootIdType",
                                 .json = R"({"boot_id": 123})",
                             },
                             {
                                 .test_name = "WrongUptimeType",
                                 .json = R"({"uptime_ms": "not_an_int"})",
                             },
                             {
                                 .test_name = "WrongUtcBootDifferenceType",
                                 .json = R"({"utc_boot_difference_nanos": "not_an_int"})",
                             },
                         }),
                         [](const TestParamInfo<InvalidBootInfoJsonParam>& info) {
                           return info.param.test_name;
                         });

}  // namespace
}  // namespace forensics::feedback
