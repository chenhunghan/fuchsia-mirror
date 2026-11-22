// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "workload.h"

#include <lib/zircon-internal/ktrace.h>

#include <fstream>

#include <gtest/gtest.h>

namespace {
void MakeTestFile(const std::string filepath, const std::string contents) {
  std::ofstream file;
  file.open(filepath);
  file << contents;
  file.close();
}

TEST(WorkloadTest, ParseTracingTranslatesStringsToGroupMasksCorrectly) {
  const std::string test_json_filepath = "tmp/trace_config_parse_test.json";

  std::map<std::string, uint32_t> corresponding_string = {
      {"KTRACE_GRP_ALL", KTRACE_GRP_ALL},
      {"KTRACE_GRP_META", KTRACE_GRP_META},
      {"KTRACE_GRP_MEMORY", KTRACE_GRP_MEMORY},
      {"KTRACE_GRP_SCHEDULER", KTRACE_GRP_SCHEDULER},
      {"KTRACE_GRP_CONTENTION", KTRACE_GRP_CONTENTION},
      {"KTRACE_GRP_IPC", KTRACE_GRP_IPC},
      {"KTRACE_GRP_IRQ", KTRACE_GRP_IRQ},
      {"KTRACE_GRP_PROBE", KTRACE_GRP_PROBE},
      {"KTRACE_GRP_ARCH", KTRACE_GRP_ARCH},
      {"KTRACE_GRP_SYSCALL", KTRACE_GRP_SYSCALL},
      {"KTRACE_GRP_VM", KTRACE_GRP_VM},
      {"KTRACE_GRP_RESTRICTED", KTRACE_GRP_RESTRICTED},
      {"UNKNOWN", KTRACE_GRP_ALL},
  };

  for (auto& element : corresponding_string) {
    std::string contents = "{\"tracing\": {\"group mask\": \"" + element.first + "\",},}";
    uint32_t expected_group_mask = element.second;

    MakeTestFile(test_json_filepath, contents);
    Workload workload = Workload::Load("/tmp/trace_config_parse_test.json");

    ASSERT_TRUE(workload.tracing().has_value());
    ASSERT_EQ(expected_group_mask, workload.tracing().value().group_mask);
  }

  std::string contents = "{\"tracing\": {},}";
  uint32_t expected_group_mask = KTRACE_GRP_ALL;

  MakeTestFile(test_json_filepath, contents);
  Workload workload = Workload::Load("/tmp/trace_config_parse_test.json");

  ASSERT_TRUE(workload.tracing().has_value());
  ASSERT_EQ(expected_group_mask, workload.tracing().value().group_mask);
}

TEST(WorkloadTest, ParseTracingAssignsFilepath) {
  const std::string test_json_filepath = "tmp/trace_config_parse_test.json";
  const std::string human_readable_filepath = "/tmp/latest.ktrace";

  std::string contents = "{\"tracing\": {\"filepath\": \"" + human_readable_filepath + "\",},}";

  MakeTestFile(test_json_filepath, contents);
  Workload workload = Workload::Load("/tmp/trace_config_parse_test.json");

  ASSERT_TRUE(workload.tracing().has_value());
  ASSERT_TRUE(workload.tracing().value().filepath.has_value());
  ASSERT_EQ(human_readable_filepath, workload.tracing().value().filepath.value());
}

TEST(WorkloadTest, ParseTracingAssignsStringRef) {
  const std::string test_json_filepath = "tmp/trace_config_parse_test.json";
  const std::string string_ref = "test ref";

  std::string contents = "{\"tracing\": {\"string ref\": \"" + string_ref + "\",},}";

  MakeTestFile(test_json_filepath, contents);
  Workload workload = Workload::Load("/tmp/trace_config_parse_test.json");

  ASSERT_TRUE(workload.tracing().has_value());
  ASSERT_TRUE(workload.tracing().value().trace_string_ref.has_value());
  ASSERT_EQ(string_ref, workload.tracing().value().trace_string_ref.value());
}

TEST(WorkloadTest, ParseWorkerAffinityAndDeadlineOptions) {
  const std::string test_json_filepath = "tmp/affinity_parse_test.json";
  const std::string contents = R"JSON({
    "workers": [
      {
        "name": "dl-medium",
        "group": "deadline_medium",
        "priority": {
          "capacity": "2500us",
          "period": "3333us",
          "critical": true,
          "affinity": [2, 3, 4, 5, 6]
        },
        "actions": [
          { "action": "yield" }
        ]
      },
      {
        "name": "fair-pinned",
        "group": "fair_pinned",
        "priority": 16,
        "affinity": [0, 1],
        "instances": 2,
        "actions": [
          { "action": "yield" }
        ]
      }
    ]
  })JSON";

  MakeTestFile(test_json_filepath, contents);
  Workload workload = Workload::Load("/tmp/affinity_parse_test.json");

  ASSERT_EQ(3u, workload.workers().size());

  const auto& dl = workload.workers()[0];
  ASSERT_TRUE(dl.affinity.has_value());
  EXPECT_EQ(0b01111100u, dl.affinity->mask[0]);
  ASSERT_TRUE(std::holds_alternative<WorkerConfig::DeadlineParams>(dl.priority));
  const auto& params = std::get<WorkerConfig::DeadlineParams>(dl.priority);
  EXPECT_EQ(zx::usec(2500), params.capacity);
  EXPECT_EQ(zx::usec(3333), params.deadline);
  EXPECT_EQ(zx::usec(3333), params.period);
  EXPECT_TRUE(params.critical);

  for (size_t i = 1; i <= 2; i++) {
    const auto& fair = workload.workers()[i];
    ASSERT_TRUE(fair.affinity.has_value());
    EXPECT_EQ(0b00000011u, fair.affinity->mask[0]);
    ASSERT_TRUE(std::holds_alternative<int>(fair.priority));
    EXPECT_EQ(16, std::get<int>(fair.priority));
  }
}

TEST(WorkloadTest, ParseBundledAsymmetricFairStarvationWorkload) {
  Workload workload = Workload::Load("/pkg/data/asymmetric_fair_starvation.json");

  ASSERT_TRUE(workload.interval().has_value());
  EXPECT_EQ(std::chrono::seconds(120), workload.interval().value());
  ASSERT_EQ(33u, workload.workers().size());

  size_t deadline_medium_count = 0;
  size_t deadline_control_count = 0;
  size_t fair_spinner_count = 0;
  size_t fair_victim_timer_count = 0;
  size_t fair_victim_port_count = 0;

  for (const auto& w : workload.workers()) {
    if (w.group == "deadline_medium") {
      deadline_medium_count++;
      ASSERT_TRUE(w.affinity.has_value());
      EXPECT_EQ(0b01111100u, w.affinity->mask[0]);
      EXPECT_TRUE(std::holds_alternative<WorkerConfig::DeadlineParams>(w.priority));
    } else if (w.group == "deadline_control_port") {
      deadline_control_count++;
      ASSERT_TRUE(w.affinity.has_value());
      EXPECT_EQ(0b01111100u, w.affinity->mask[0]);
      EXPECT_TRUE(std::holds_alternative<WorkerConfig::DeadlineParams>(w.priority));
    } else if (w.group == "fair_spinner") {
      fair_spinner_count++;
      EXPECT_FALSE(w.affinity.has_value());
      EXPECT_TRUE(std::holds_alternative<int>(w.priority));
    } else if (w.group == "fair_victim_timer") {
      fair_victim_timer_count++;
      EXPECT_FALSE(w.affinity.has_value());
      EXPECT_TRUE(std::holds_alternative<int>(w.priority));
    } else if (w.group == "fair_victim_port") {
      fair_victim_port_count++;
      EXPECT_FALSE(w.affinity.has_value());
      EXPECT_TRUE(std::holds_alternative<int>(w.priority));
    }
  }

  EXPECT_EQ(12u, deadline_medium_count);
  EXPECT_EQ(1u, deadline_control_count);
  EXPECT_EQ(14u, fair_spinner_count);
  EXPECT_EQ(3u, fair_victim_timer_count);
  EXPECT_EQ(3u, fair_victim_port_count);
}
}  // anonymous namespace
