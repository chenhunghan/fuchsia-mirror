// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/debug_agent/launcher.h"

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.debugger/cpp/fidl.h>

#include <gtest/gtest.h>

namespace {

TEST(DebugAgentLauncherTest, CreateChildDecl) {
  fuchsia_debugger::LaunchOptions options_false;
  options_false.monitor_root_job(false);

  auto decl_false = DebugAgentLauncher::CreateChildDecl("test_agent_false", options_false);
  EXPECT_EQ(decl_false.name(), "test_agent_false");
  EXPECT_FALSE(decl_false.config_overrides().has_value());

  auto offers_false = DebugAgentLauncher::CreateDynamicOffers(options_false);
  ASSERT_EQ(offers_false.size(), 1u);
  ASSERT_TRUE(offers_false[0].config().has_value());
  EXPECT_EQ(offers_false[0].config()->source_name(), "fuchsia.debugger.MonitorRootJobFalse");
  EXPECT_EQ(offers_false[0].config()->target_name(), "fuchsia.debugger.MonitorRootJob");
  EXPECT_EQ(offers_false[0].config()->availability(),
            fuchsia_component_decl::Availability::kRequired);

  fuchsia_debugger::LaunchOptions options_true;
  options_true.monitor_root_job(true);

  auto decl_true = DebugAgentLauncher::CreateChildDecl("test_agent_true", options_true);
  EXPECT_EQ(decl_true.name(), "test_agent_true");
  EXPECT_FALSE(decl_true.config_overrides().has_value());

  auto offers_true = DebugAgentLauncher::CreateDynamicOffers(options_true);
  ASSERT_EQ(offers_true.size(), 1u);
  ASSERT_TRUE(offers_true[0].config().has_value());
  EXPECT_EQ(offers_true[0].config()->source_name(), "fuchsia.debugger.MonitorRootJobTrue");
  EXPECT_EQ(offers_true[0].config()->target_name(), "fuchsia.debugger.MonitorRootJob");
  EXPECT_EQ(offers_true[0].config()->availability(),
            fuchsia_component_decl::Availability::kRequired);

  fuchsia_debugger::LaunchOptions options_empty;
  auto decl_empty = DebugAgentLauncher::CreateChildDecl("test_agent_empty", options_empty);
  EXPECT_EQ(decl_empty.name(), "test_agent_empty");
  EXPECT_FALSE(decl_empty.config_overrides().has_value());

  auto offers_empty = DebugAgentLauncher::CreateDynamicOffers(options_empty);
  EXPECT_TRUE(offers_empty.empty());
}

}  // namespace
