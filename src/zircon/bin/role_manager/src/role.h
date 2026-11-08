// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_ZIRCON_BIN_ROLE_MANAGER_SRC_ROLE_H_
#define SRC_ZIRCON_BIN_ROLE_MANAGER_SRC_ROLE_H_

#include <fidl/fuchsia.scheduler/cpp/fidl.h>
#include <fidl/fuchsia.sys2/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/function.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/resource.h>

#include "config.h"

class RoleManager : public fidl::WireServer<fuchsia_scheduler::RoleManager>,
                    public fidl::WireAsyncEventHandler<fuchsia_sys2::BootNotifier> {
 public:
  using UnclaimedProfileCallback = fit::function<void(std::string_view role, std::string_view type,
                                                      zircon_profile::ProfileScope scope)>;

  static zx::result<std::unique_ptr<RoleManager>> Create(async_dispatcher_t* dispatcher,
                                                         inspect::Node& parent);
  static std::unique_ptr<RoleManager> CreateForTesting(
      async_dispatcher_t* dispatcher, zircon_profile::ConfiguredProfiles profiles,
      fidl::ClientEnd<fuchsia_sys2::BootNotifier> boot_notifier,
      UnclaimedProfileCallback on_unclaimed_profile = nullptr);
  void SetRole(SetRoleRequestView request, SetRoleCompleter::Sync& completer) override;
  void GetProfileForRole(GetProfileForRoleRequestView request,
                         GetProfileForRoleCompleter::Sync& completer) override;
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_scheduler::RoleManager> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

  // fidl::WireAsyncEventHandler<fuchsia_sys2::BootNotifier>
  void OnBootComplete() override;
  void on_fidl_error(fidl::UnbindInfo error) override;
  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_sys2::BootNotifier> metadata) override;

 private:
  // Makes a best effort attempt to log the request to the syslog if a sufficient log level has
  // been specified.
  void LogRequest(SetRoleRequestView request);
  void LogUnclaimedProfiles();

  RoleManager(async_dispatcher_t* dispatcher, zx::resource profile_resource,
              zircon_profile::ConfiguredProfiles profiles,
              UnclaimedProfileCallback on_unclaimed_profile = nullptr)
      : dispatcher_(dispatcher),
        profile_resource_(std::move(profile_resource)),
        profiles_(std::move(profiles)),
        on_unclaimed_profile_(std::move(on_unclaimed_profile)) {}
  async_dispatcher_t* dispatcher_;
  zx::resource profile_resource_;
  zircon_profile::ConfiguredProfiles profiles_;
  UnclaimedProfileCallback on_unclaimed_profile_;
  fidl::WireClient<fuchsia_sys2::BootNotifier> boot_notifier_client_;
  async::TaskClosureMethod<RoleManager, &RoleManager::LogUnclaimedProfiles>
      unclaimed_profile_warning_task_{this};

  void PublishInspect(inspect::Node& parent);
  fpromise::promise<inspect::Inspector> Inspect();
};

#endif  // SRC_ZIRCON_BIN_ROLE_MANAGER_SRC_ROLE_H_
