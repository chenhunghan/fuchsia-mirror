// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/analytics/cpp/core_dev_tools/environment_status.h"

#include <cstdlib>

#include <gtest/gtest.h>

namespace analytics::core_dev_tools {

namespace {

// The same generated lists that environment_status.cc uses.
constexpr const char* kAgentEnvironments[] = {
#include "src/lib/analytics/cpp/core_dev_tools/agent_environments.inc"
#include "src/lib/analytics/cpp/core_dev_tools/agent_session_id_environments.inc"
};

// The test itself can be run by an agent, so each test starts from an environment without any
// agent variables.
class AgentEnvironmentTest : public ::testing::Test {
 protected:
  void SetUp() override {
    for (const char* name : kAgentEnvironments) {
      ASSERT_EQ(unsetenv(name), 0);
    }
  }
};

TEST_F(AgentEnvironmentTest, NotRunByAgentWithoutVariables) { EXPECT_FALSE(IsRunByAgent()); }

TEST_F(AgentEnvironmentTest, EachVariableIndicatesAgent) {
  for (const char* name : kAgentEnvironments) {
    ASSERT_EQ(setenv(name, "1", 1), 0);
    EXPECT_TRUE(IsRunByAgent()) << name;
    ASSERT_EQ(unsetenv(name), 0);
    EXPECT_FALSE(IsRunByAgent()) << name;
  }
}

// Checks one well-known name from each input file directly, which proves that neither generated
// list is empty.
TEST_F(AgentEnvironmentTest, KnownVariableIndicatesAgent) {
  constexpr const char* kKnownVariables[] = {"GEMINI_CLI", "FFX_AGENT_SESSION_ID"};
  for (const char* name : kKnownVariables) {
    ASSERT_EQ(setenv(name, "1", 1), 0);
    EXPECT_TRUE(IsRunByAgent()) << name;
    ASSERT_EQ(unsetenv(name), 0);
    EXPECT_FALSE(IsRunByAgent()) << name;
  }
}

// An empty value doesn't indicate an agent, which is consistent with //src/lib/agents and
// //tools/devshell/lib/is_invoked_by_agent.sh.
TEST_F(AgentEnvironmentTest, EmptyVariableIsIgnored) {
  ASSERT_EQ(setenv("GEMINI_CLI", "", 1), 0);
  EXPECT_FALSE(IsRunByAgent());
  ASSERT_EQ(unsetenv("GEMINI_CLI"), 0);
}

TEST(EnvironmentStatusTest, Bot) {
  ASSERT_EQ(setenv("TEST_ONLY_ENV", "1", 1), 0);
  EXPECT_STREQ(GetBotInfo().name, "test-only");
  EXPECT_TRUE(IsRunByBot());
}

TEST(EnvironmentStatusTest, Disabled) {
  ASSERT_EQ(unsetenv("FUCHSIA_ANALYTICS_DISABLED"), 0);
  EXPECT_FALSE(IsDisabledByEnvironment());

  ASSERT_EQ(setenv("FUCHSIA_ANALYTICS_DISABLED", "1", 1), 0);
  EXPECT_TRUE(IsDisabledByEnvironment());
  // IsDisabledByEnvironment() currently returns true whenever FUCHSIA_ANALYTICS_DISABLED is set,
  // no matter what value it is. This behavior should be consistent with
  // //src/lib/analytics/rust/src/env_info.rs
  ASSERT_EQ(setenv("FUCHSIA_ANALYTICS_DISABLED", "0", 1), 0);
  EXPECT_TRUE(IsDisabledByEnvironment());
  ASSERT_EQ(setenv("FUCHSIA_ANALYTICS_DISABLED", "", 1), 0);
  EXPECT_TRUE(IsDisabledByEnvironment());

  ASSERT_EQ(unsetenv("FUCHSIA_ANALYTICS_DISABLED"), 0);
  EXPECT_FALSE(IsDisabledByEnvironment());
}

}  // namespace

}  // namespace analytics::core_dev_tools
