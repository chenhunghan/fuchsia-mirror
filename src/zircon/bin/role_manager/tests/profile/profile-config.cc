// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-testing/test_loop.h>
#include <lib/zx/profile.h>
#include <lib/zx/result.h>

#include <set>
#include <string>
#include <tuple>
#include <unordered_set>

#include <zxtest/zxtest.h>

#include "src/zircon/bin/role_manager/src/config.h"
#include "src/zircon/bin/role_manager/src/role.h"

namespace {

using zircon_profile::LoadConfigs;
using zircon_profile::ProfileScope;
using zircon_profile::Role;

TEST(ProfileConfig, Parse) {
  fit::result result = LoadConfigs("/pkg/data");
  ASSERT_TRUE(result.is_ok());

  {
    const auto iter = result->thread.find(*Role::Create("fuchsia.default"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Builtin);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 16);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.bringup.a:affinity"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Bringup);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 0);
    EXPECT_EQ(iter->second.info.cpu_affinity_mask.mask[0], 0b001);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.bringup.b:affinity"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 1);
    EXPECT_EQ(iter->second.info.cpu_affinity_mask.mask[0], 0b011);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.core.a"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 5'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 10'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.core.critical"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_DEADLINE | ZX_PROFILE_INFO_FLAG_CRITICAL);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 5'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 10'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.core.all_cpus"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 16);
    for (const uint64_t mask_word : iter->second.info.cpu_affinity_mask.mask) {
      EXPECT_EQ(mask_word, std::numeric_limits<uint64_t>::max());
    }
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.bringup.a"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 10);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.product.a"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Product);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 25);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.core.a:affinity"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Product);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 6'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 15'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 20'000'000);
    EXPECT_EQ(iter->second.info.cpu_affinity_mask.mask[0], 0b110);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.bringup.b"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Product);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  // The next two test cases validate that the same role name with different selectors correctly
  // fetches two different roles.
  {
    const auto iter = result->thread.find(*Role::Create("test.core.parameterized.role:input=foo"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 5'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 10'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
    EXPECT_EQ(iter->second.output_parameters.size(), 2);
    EXPECT_EQ(iter->second.output_parameters[0].key(), "output1");
    EXPECT_EQ(iter->second.output_parameters[0].value().int_value().value(), 1);
    EXPECT_EQ(iter->second.output_parameters[1].key(), "output2");
    EXPECT_EQ(iter->second.output_parameters[1].value().float_value().value(), 2.5);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.core.parameterized.role:input=bar"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 6'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 9'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
    EXPECT_EQ(iter->second.output_parameters.size(), 2);
    EXPECT_EQ(iter->second.output_parameters[0].key(), "output1");
    EXPECT_EQ(iter->second.output_parameters[0].value().int_value().value(), 5);
    EXPECT_EQ(iter->second.output_parameters[1].key(), "output2");
    EXPECT_EQ(iter->second.output_parameters[1].value().float_value().value(), 42.6);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.board.a"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Board);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.board.a:affinity"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 11);
    EXPECT_EQ(iter->second.info.cpu_affinity_mask.mask[0], 0b001);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.board.b"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Product);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  {
    const auto iter = result->thread.find(*Role::Create("test.board.b:affinity"));
    ASSERT_TRUE(iter != result->thread.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Board);
    EXPECT_EQ(iter->second.info.flags,
              ZX_PROFILE_INFO_FLAG_CPU_MASK | ZX_PROFILE_INFO_FLAG_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 6);
    EXPECT_EQ(iter->second.info.cpu_affinity_mask.mask[0], 0b0010);
  }

  // The next two test cases validate that the order of selectors does not change the role that is
  // fetched.
  {
    const auto iter =
        result->thread.find(*Role::Create("test.core.parameterized.role:param1=foo,param2=bar"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 7'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 8'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
    EXPECT_EQ(iter->second.output_parameters.size(), 3);
    EXPECT_EQ(iter->second.output_parameters[0].key(), "output1");
    EXPECT_EQ(iter->second.output_parameters[0].value().int_value().value(), 489);
    EXPECT_EQ(iter->second.output_parameters[1].key(), "output2");
    EXPECT_EQ(iter->second.output_parameters[1].value().float_value().value(), 297.5);
    EXPECT_EQ(iter->second.output_parameters[2].key(), "output3");
    EXPECT_EQ(iter->second.output_parameters[2].value().string_value().value(), "Hello, World!");
  }

  {
    const auto iter =
        result->thread.find(*Role::Create("test.core.parameterized.role:param2=bar,param1=foo"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_DEADLINE);
    EXPECT_EQ(iter->second.info.deadline_params.capacity, 7'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.relative_deadline, 8'000'000);
    EXPECT_EQ(iter->second.info.deadline_params.period, 10'000'000);
    EXPECT_EQ(iter->second.output_parameters.size(), 3);
    EXPECT_EQ(iter->second.output_parameters[0].key(), "output1");
    EXPECT_EQ(iter->second.output_parameters[0].value().int_value().value(), 489);
    EXPECT_EQ(iter->second.output_parameters[1].key(), "output2");
    EXPECT_EQ(iter->second.output_parameters[1].value().float_value().value(), 297.5);
    EXPECT_EQ(iter->second.output_parameters[2].key(), "output3");
    EXPECT_EQ(iter->second.output_parameters[2].value().string_value().value(), "Hello, World!");
  }

  {
    const auto iter = result->memory.find(*Role::Create("fuchsia.default"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Builtin);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 16);
  }
  {
    const auto iter = result->memory.find(*Role::Create("test.bringup.a"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  {
    const auto iter = result->memory.find(*Role::Create("test.bringup.b"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Bringup);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 24);
  }

  {
    const auto iter = result->memory.find(*Role::Create("test.core.a"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 24);
  }

  {
    const auto iter = result->memory.find(*Role::Create("test.core.mem"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  {
    const auto iter = result->memory.find(*Role::Create("test.board.a"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Board);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 20);
  }

  {
    const auto iter = result->memory.find(*Role::Create("test.board.b"));
    ASSERT_TRUE(iter != result->memory.end());
    EXPECT_EQ(iter->second.scope, ProfileScope::Core);
    EXPECT_EQ(iter->second.info.flags, ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY);
    EXPECT_EQ(iter->second.info.priority, 24);
  }

  const std::unordered_set<std::string> expected_thread_profiles{
      "test.product.a",
      "test.core.a:affinity",
      "test.bringup.a:affinity",
      "test.bringup.b",
      "test.bringup.b:affinity",
      "test.core.a",
      "test.core.critical",
      "test.core.all_cpus",
      "test.core.parameterized.role:input=foo",
      "test.core.parameterized.role:input=bar",
      "test.core.parameterized.role:param1=foo,param2=bar",
      "test.core.deadline.only",
      "test.bringup.a",
      "fuchsia.default",
      "test.board.a",
      "test.board.a:affinity",
      "test.board.b",
      "test.board.b:affinity",
  };
  EXPECT_EQ(result->thread.size(), expected_thread_profiles.size());
  for (auto expected_thread_profile : expected_thread_profiles) {
    fit::result role = zircon_profile::Role::Create(expected_thread_profile);
    ASSERT_TRUE(role.is_ok());
    EXPECT_NE(result->thread.cend(), result->thread.find(*role));
  }

  const std::unordered_set<std::string> expected_memory_profiles{
      "test.bringup.a",  "test.bringup.b", "test.core.a",  "test.core.mem",
      "fuchsia.default", "test.board.a",   "test.board.b",
  };
  EXPECT_EQ(result->memory.size(), expected_memory_profiles.size());
  for (auto expected_memory_profile : expected_memory_profiles) {
    fit::result role = zircon_profile::Role::Create(expected_memory_profile);
    ASSERT_TRUE(role.is_ok());
    EXPECT_NE(result->memory.cend(), result->memory.find(*role));
  }
}

TEST(ProfileConfig, CreateRole) {
  EXPECT_EQ(fit::success{}, Role::Create("abcd"));
  EXPECT_EQ(fit::success{}, Role::Create("a.b.c.d"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123"));
  EXPECT_EQ(fit::success{}, Role::Create("_abcd123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123.01234"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd-123.012-34"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd_123.012_34"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123.abc123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123._abc123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123._abc123:xyz123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123._abc123:xyz-123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123._abc123:xyz_123"));
  EXPECT_EQ(fit::success{}, Role::Create("abcd123._abc123:xyz123,abc987=01234"));

  EXPECT_EQ(fit::failed{}, Role::Create(""));
  EXPECT_EQ(fit::failed{}, Role::Create("+abcd"));
  EXPECT_EQ(fit::failed{}, Role::Create("-abcd"));
}

TEST(ProfileConfig, MaybeMediaRole) {
  {
    fit::result role = Role::Create("foo.bar:realm=media,capacity=1000000,deadline=1000000");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::success{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:realm=bar,capacity=1000000,deadline=1000000");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:realm=media,capacity=baz,deadline=1000000");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:realm=media,capacity=1000000,deadline=baz");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:capacity=1000000,deadline=baz");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:realm=media,deadline=1000000");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
  {
    fit::result role = Role::Create("foo.bar:realm=media,capacity=1000000");
    ASSERT_TRUE(role.is_ok());
    EXPECT_EQ(fit::failed{}, role->ToMediaRole());
  }
}

TEST(ProfileConfig, LogUnclaimedProfilesAfterBoot) {
  async::TestLoop loop;
  fit::result result = LoadConfigs("/pkg/data");
  ASSERT_TRUE(result.is_ok());

  // Mark one Product profile ("test.product.a") as claimed so it should not be reported.
  auto claimed = result->thread.find(*Role::Create("test.product.a"));
  ASSERT_TRUE(claimed != result->thread.end());
  claimed->second.request_count = 1;

  auto [client_end, server_end] = fidl::Endpoints<fuchsia_sys2::BootNotifier>::Create();

  using UnclaimedEntry = std::tuple<std::string, std::string, ProfileScope>;
  std::set<UnclaimedEntry> unclaimed;
  auto manager = RoleManager::CreateForTesting(
      loop.dispatcher(), std::move(*result), std::move(client_end),
      [&](std::string_view role, std::string_view type, ProfileScope scope) {
        unclaimed.emplace(role, type, scope);
      });

  // Before OnBootComplete, advancing time by 3 minutes should not run the check.
  loop.RunFor(zx::min(3));
  EXPECT_TRUE(unclaimed.empty());

  // Send OnBootComplete and verify RoleManager unbinds the BootNotifier channel.
  ASSERT_OK(fidl::WireSendEvent(server_end)->OnBootComplete().status());
  loop.RunUntilIdle();

  zx_signals_t observed = 0;
  EXPECT_OK(
      server_end.channel().wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), &observed));
  EXPECT_TRUE(observed & ZX_CHANNEL_PEER_CLOSED);

  // Just before 3 minutes, the delayed task has not fired yet.
  loop.RunFor(zx::min(3) - zx::sec(1));
  EXPECT_TRUE(unclaimed.empty());

  // At 3 minutes after OnBootComplete, only unclaimed Board and Product profiles are reported.
  loop.RunFor(zx::sec(1));
  const std::set<UnclaimedEntry> expected{
      {"test.core.a", "thread", ProfileScope::Product},
      {"test.bringup.b", "thread", ProfileScope::Product},
      {"test.board.a", "thread", ProfileScope::Board},
      {"test.board.b", "thread", ProfileScope::Product},
      {"test.board.b", "thread", ProfileScope::Board},
      {"test.board.a", "memory", ProfileScope::Board},
  };
  EXPECT_EQ(unclaimed, expected);
}

}  // anonymous namespace
