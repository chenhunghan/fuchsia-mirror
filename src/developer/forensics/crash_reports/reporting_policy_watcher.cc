// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/reporting_policy_watcher.h"

#include <lib/syslog/cpp/macros.h>

namespace forensics {
namespace crash_reports {

std::string ToString(const ReportingPolicy policy) {
  switch (policy) {
    case ReportingPolicy::kUndecided:
      return "UNDECIDED";
    case ReportingPolicy::kArchive:
      return "ARCHIVE";
    case ReportingPolicy::kDoNotFileAndDelete:
      return "DO_NOT_FILE_AND_DELETE";
    case ReportingPolicy::kUpload:
      return "UPLOAD";
  }
}

ReportingPolicyWatcher::ReportingPolicyWatcher(const ReportingPolicy policy) : policy_(policy) {}

void ReportingPolicyWatcher::OnPolicyChange(::fit::function<void(ReportingPolicy)> on_change) {
  callbacks_.push_back(std::move(on_change));
}

void ReportingPolicyWatcher::SetPolicy(const ReportingPolicy policy) {
  if (policy_ == policy) {
    return;
  }

  FX_LOGS(INFO) << "Reporting policy changed from " << ToString(policy_) << " to "
                << ToString(policy);
  policy_ = policy;

  for (const ::fit::function<void(ReportingPolicy)>& on_change : callbacks_) {
    on_change(policy_);
  }
}

UserReportingPolicyWatcher::UserReportingPolicyWatcher(
    async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services)
    : ReportingPolicyWatcher(ReportingPolicy::kUndecided),
      dispatcher_(dispatcher),
      services_(std::move(services)),
      watch_backoff_(/*initial_delay=*/zx::min(1), /*retry_factor=*/2u, /*max_delay=*/zx::hour(1)) {
  Watch();
}

void UserReportingPolicyWatcher::on_fidl_error(const fidl::UnbindInfo error) {
  privacy_settings_ = fidl::Client<fuchsia_settings::Privacy>();
  FX_PLOGS(WARNING, error.status()) << "Lost connection to fuchsia.settings.Privacy";
  SetPolicy(ReportingPolicy::kUndecided);
  watch_task_.PostDelayed(dispatcher_, watch_backoff_.GetNext());
}

void UserReportingPolicyWatcher::Watch() {
  if (!privacy_settings_.is_valid()) {
    auto [client, server] = fidl::Endpoints<fuchsia_settings::Privacy>::Create();
    services_->Connect(fidl::DiscoverableProtocolName<fuchsia_settings::Privacy>,
                       server.TakeChannel());
    privacy_settings_ =
        fidl::Client<fuchsia_settings::Privacy>(std::move(client), dispatcher_, this);
  }

  privacy_settings_->Watch().Then([this](fidl::Result<fuchsia_settings::Privacy::Watch>& result) {
    // Errors unbind |privacy_settings_|, and on_fidl_error() handles reconnecting.
    if (result.is_error()) {
      return;
    }

    watch_backoff_.Reset();

    if (!result->settings().user_data_sharing_consent().has_value()) {
      SetPolicy(ReportingPolicy::kUndecided);
    } else if (*result->settings().user_data_sharing_consent()) {
      SetPolicy(ReportingPolicy::kUpload);
    } else {
      SetPolicy(ReportingPolicy::kDoNotFileAndDelete);
    }

    // Watch for the next update, following the hanging-get pattern.
    Watch();
  });
}

}  // namespace crash_reports
}  // namespace forensics
