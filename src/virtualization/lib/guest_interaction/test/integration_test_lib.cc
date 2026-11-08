// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "integration_test_lib.h"

#include <fidl/fuchsia.kernel/cpp/fidl.h>
#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <fidl/fuchsia.net.stack/cpp/fidl.h>
#include <fidl/fuchsia.net.virtualization/cpp/fidl.h>
#include <fidl/fuchsia.scheduler/cpp/fidl.h>
#include <fidl/fuchsia.sysinfo/cpp/fidl.h>
#include <fidl/fuchsia.sysmem/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <fidl/fuchsia.tracing.provider/cpp/fidl.h>
#include <fidl/fuchsia.virtualization.guest.interaction/cpp/fidl.h>
#include <fidl/fuchsia.virtualization/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/syslog/cpp/macros.h>

#include "src/lib/testing/predicates/status.h"
#include "src/virtualization/tests/lib/guest_console.h"

static fit::closure MakeRecurringTask(async_dispatcher_t* dispatcher, fit::closure cb,
                                      zx::duration frequency) {
  return [dispatcher, cb = std::move(cb), frequency]() mutable {
    cb();
    async::PostDelayedTask(dispatcher, MakeRecurringTask(dispatcher, std::move(cb), frequency),
                           frequency);
  };
}

void GuestInteractionTest::GetHostVsockEndpoint(
    fidl::ServerEnd<fuchsia_virtualization::HostVsockEndpoint> endpoint) {
  fidl::Result<fuchsia_virtualization::Guest::GetHostVsockEndpoint> vsock_result =
      guest_->GetHostVsockEndpoint({{.endpoint = std::move(endpoint)}});
  FX_CHECK(vsock_result.is_ok()) << vsock_result.error_value().FormatDescription();
}

void GuestInteractionTest::SetUp() {
  using component_testing::ChildRef;
  using component_testing::Directory;
  using component_testing::ParentRef;
  using component_testing::Protocol;
  using component_testing::RealmBuilder;
  using component_testing::RealmRoot;
  using component_testing::Route;

  // Launch the Debian guest
  constexpr auto kFakeNetstackComponentName = "fake_netstack";
  constexpr auto kDebianGuestManagerUrl = "debian_guest_manager#meta/debian_guest_manager.cm";

  constexpr auto kGuestManagerName = "guest_manager";

  fuchsia_virtualization::GuestConfig cfg;
  cfg.virtio_gpu(false);

  auto realm_builder = RealmBuilder::Create();
  realm_builder.AddChild(kGuestManagerName, kDebianGuestManagerUrl);
  realm_builder.AddLocalChild(kFakeNetstackComponentName,
                              [&]() { return fake_netstack_.NewComponent(); });

  realm_builder
      .AddRoute(Route{
          .capabilities =
              {
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_logger::LogSink>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_kernel::HypervisorResource>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_kernel::VmexResource>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_sysinfo::SysInfo>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_sysmem::Allocator>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_sysmem2::Allocator>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_tracing_provider::Registry>},
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_scheduler::RoleManager>},
              },
          .source = {ParentRef()},
          .targets = {ChildRef{kGuestManagerName}}})
      .AddRoute(Route{
          .capabilities =
              {
                  Protocol{fidl::DiscoverableProtocolName<fuchsia_net_virtualization::Control>},
              },
          .source = {ChildRef{kFakeNetstackComponentName}},
          .targets = {ChildRef{kGuestManagerName}}})
      .AddRoute(Route{
          .capabilities =
              {
                  Protocol{
                      fidl::DiscoverableProtocolName<fuchsia_virtualization::DebianGuestManager>},
              },
          .source = ChildRef{kGuestManagerName},
          .targets = {ParentRef()}});

  realm_root_ = realm_builder.Build(dispatcher());

  zx::result client_end =
      realm_root_->component().Connect<fuchsia_virtualization::DebianGuestManager>();
  ASSERT_OK(client_end.status_value());
  guest_manager_.Bind(std::move(*client_end));

  zx::result endpoints = fidl::CreateEndpoints<fuchsia_virtualization::Guest>();
  ASSERT_OK(endpoints.status_value());
  auto [guest_client_end, guest_server_end] = std::move(*endpoints);

  FX_LOGS(INFO) << "Starting Debian Guest";
  fidl::Result<fuchsia_virtualization::DebianGuestManager::Launch> res = guest_manager_->Launch(
      {{.guest_config = std::move(cfg), .controller = std::move(guest_server_end)}});
  ASSERT_TRUE(res.is_ok()) << res.error_value().FormatDescription();

  guest_.Bind(std::move(guest_client_end));

  // Start a GuestConsole.  When the console starts, it waits until it
  // receives some sensible output from the guest to ensure that the guest is
  // usable.
  FX_LOGS(INFO) << "Getting Serial Console";
  fidl::Result<fuchsia_virtualization::Guest::GetConsole> get_console_result = guest_->GetConsole();
  ASSERT_TRUE(get_console_result.is_ok()) << get_console_result.error_value().FormatDescription();
  FX_LOGS(INFO) << "Serial Console Received";

  GuestConsole serial(std::make_unique<ZxSocket>(std::move(get_console_result->socket())));
  ASSERT_OK(serial.Start(zx::time::infinite()));

  // Make sure the pty is running and that the guest will receive our commands.
  ASSERT_OK(serial.RepeatCommandTillSuccess("echo guest ready", "$", "guest ready",
                                            zx::time::infinite(), zx::sec(1)));

  // Wait until guest_interaction_daemon is running.
  ASSERT_OK(serial.ExecuteBlocking(
      "journalctl -f --no-tail -u guest_interaction_daemon | grep -m1 Listening", "$",
      zx::time::infinite(), nullptr));

  // Periodically log the guest state.
  MakeRecurringTask(
      dispatcher(),
      [serial = std::move(serial), log_count = 0]() mutable {
        ASSERT_OK(serial.ExecuteBlocking("echo " + std::to_string(++log_count) +
                                             "; journalctl -u guest_interaction_daemon --no-pager",
                                         "$", zx::time::infinite(), nullptr));
      },
      zx::sec(10))();
}

void GuestInteractionTest::TearDown() {
  guest_ = {};
  guest_manager_ = {};
  if (realm_root_) {
    bool complete = false;
    realm_root_->Teardown([&](fit::result<fuchsia::component::Error> result) { complete = true; });
    RunLoopUntil([&]() { return complete; });
    realm_root_.reset();
  }
}
