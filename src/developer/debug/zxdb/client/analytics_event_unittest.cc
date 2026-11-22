// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/analytics_event.h"

#include <stdlib.h>

#include <string>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/remote_api_test.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/common/scoped_test_env.h"

namespace zxdb {

class AnalyticsEventTest : public RemoteAPITest {};

TEST_F(AnalyticsEventTest, SessionStartedRecordsRunByAgent) {
  for (bool run_by_agent : {true, false}) {
    SessionStarted event("1234");
    event.SetRunByAgent(run_by_agent);

    ASSERT_TRUE(event.parameters_opt());
    auto parameters = *event.parameters_opt();

    ASSERT_NE(parameters.find("is_agent_env"), parameters.end());
    EXPECT_EQ(std::get<bool>(parameters.find("is_agent_env")->second), run_by_agent);
  }
}

TEST_F(AnalyticsEventTest, DoNotLeakUsername) {
  const std::string kUsername = "BobAlice";
  ScopedTestEnv env;

  ASSERT_EQ(env.Set("USER", kUsername), 0);

  // A valid verb with an invalid argument that contains a username should be anonymized.
  CommandReport report;
  report.verb = "jump";
  report.arguments.emplace_back("/path/home/" + kUsername);

  CommandEvent event("1234");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  auto parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("argument0"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("argument0")->second).c_str(),
               "/path/home/$USER");

  // An invalid verb prints an error message with the text verbatim from the command line, which
  // should also be anonymized. This is also important for testing that the replacement works
  // properly. The string could be enclosed in escaped double quotes by different error paths, and
  // if the username appears immediately preceding an escaped double quote, the rest of the string
  // following that would be lost without careful handling.
  report = CommandReport();
  report.err = Err("Invalid command \"pasted/with/" + kUsername + "\" is not a valid verb.");
  event = CommandEvent("6789");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(),
               "Invalid command \"pasted/with/$USER\" is not a valid verb.");

  // Edge cases.
  report = CommandReport();
  report.err = Err(kUsername + " error.");
  event = CommandEvent("1357");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(),
               "$USER error.");

  report = CommandReport();
  report.err = Err("Error: " + kUsername);
  event = CommandEvent("0246");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(),
               "Error: $USER");

  report = CommandReport();
  report.err = Err(kUsername);
  event = CommandEvent("9876");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(), "$USER");

  // The username should be escaped every time that it appears in the string.
  report = CommandReport();
  report.err = Err(kUsername + " " + kUsername);
  event = CommandEvent("4321");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(),
               "$USER $USER");

  report = CommandReport();
  report.err = Err(kUsername + kUsername);
  event = CommandEvent("1000");
  event.FromCommandReport(report);

  ASSERT_TRUE(event.parameters_opt());
  parameters = *event.parameters_opt();

  EXPECT_NE(parameters.find("error_message"), parameters.end());
  EXPECT_STREQ(std::get<std::string>(parameters.find("error_message")->second).c_str(),
               "$USER$USER");
}

namespace {

TEST(ObfuscateUserTest, ReplacesAllOccurrencesAndPreservesSuffix) {
  ScopedTestEnv env;
  ASSERT_EQ(env.Set("USER", "BobAlice"), 0);

  struct TestCase {
    std::string_view input;
    std::string_view expected;
  };
  const std::vector<TestCase> kCases = {
      // Middle-of-string replacement must preserve suffixes longer than 40 characters.
      {
          .input = "/home/BobAlice/projects/fuchsia/src/developer/debug/zxdb/client/"
                   "analytics_event.cc:123",
          .expected =
              "/home/$USER/projects/fuchsia/src/developer/debug/zxdb/client/analytics_event.cc:123",
      },
      // An escaped double quote immediately after the username must not truncate the tail.
      {
          .input = "Error in \\\"/tmp/BobAlice\\\" while loading symbols from build directory",
          .expected = "Error in \\\"/tmp/$USER\\\" while loading symbols from build directory",
      },
      {.input = "BobAlice", .expected = "$USER"},
      {.input = "BobAlice failed early", .expected = "$USER failed early"},
      {.input = "Failed for BobAlice", .expected = "Failed for $USER"},
      {
          .input = "BobAlice met BobAlice and BobAliceBobAlice",
          .expected = "$USER met $USER and $USER$USER",
      },
  };

  for (const auto& test_case : kCases) {
    EXPECT_EQ(ObfuscateUser(test_case.input), test_case.expected) << "input=" << test_case.input;
  }
}

TEST(ObfuscateUserTest, HandlesEmptyUnsetAndSelfReferentialEnv) {
  ScopedTestEnv env;

  ASSERT_EQ(env.Set("USER", ""), 0);
  EXPECT_EQ(ObfuscateUser("/home/alice/project/main.cc"), "/home/alice/project/main.cc");
  EXPECT_EQ(ObfuscateUser(""), "");

  ASSERT_EQ(env.Unset("USER"), 0);
  EXPECT_EQ(ObfuscateUser("/home/alice/project/main.cc"), "/home/alice/project/main.cc");

  ASSERT_EQ(env.Set("USER", "USER"), 0);
  EXPECT_EQ(ObfuscateUser("USER /home/USER/USER"), "$USER /home/$USER/$USER");
}

}  // namespace
}  // namespace zxdb
