// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/utils/cobalt/logger.h"

#include <fidl/fuchsia.metrics/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <string>

#include "src/developer/forensics/utils/cobalt/metrics.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace forensics {
namespace cobalt {
namespace {

using async::PostDelayedTask;
using fxl::StringPrintf;

constexpr uint32_t kMaxPendingEvents = 500u;

uint64_t CurrentTimeUSecs(const timekeeper::Clock* clock) {
  return zx::nsec(clock->MonotonicNow().get()).to_usecs();
}

}  // namespace

Logger::Logger(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
               timekeeper::Clock* clock)
    : dispatcher_(dispatcher),
      services_(services),
      clock_(clock),
      logger_reconnection_backoff_(/*initial_delay=*/zx::msec(100), /*retry_factor=*/2u,
                                   /*max_delay=*/zx::hour(1)) {
  auto [client, server] = fidl::Endpoints<fuchsia_metrics::MetricEventLogger>::Create();
  logger_ = fidl::Client(std::move(client), dispatcher_, this);
  ConnectToLogger(std::move(server));
}

void Logger::on_fidl_error(const fidl::UnbindInfo error) {
  // Binding a new client to a dispatcher that is shutting down crashes the process.
  if (error.is_dispatcher_shutdown()) {
    return;
  }

  FX_PLOGS(WARNING, error.status()) << "Lost connection with fuchsia.metrics.MetricEventLogger";
  logger_ = fidl::Client<fuchsia_metrics::MetricEventLogger>();
  RetryConnectingToLogger();
}

void Logger::ConnectToLogger(fidl::ServerEnd<fuchsia_metrics::MetricEventLogger> logger_request) {
  // Connect to the LoggerFactory.
  auto [client, server] = fidl::Endpoints<fuchsia_metrics::MetricEventLoggerFactory>::Create();
  services_->Connect(fidl::DiscoverableProtocolName<fuchsia_metrics::MetricEventLoggerFactory>,
                     server.TakeChannel());
  logger_factory_ = fidl::Client(std::move(client), dispatcher_);

  // We don't need a long standing connection to the LoggerFactory so we unbind after setting up
  // the Logger.
  fuchsia_metrics::ProjectSpec project;
  project.customer_id(1);
  project.project_id(kProjectId);

  logger_factory_
      ->CreateMetricEventLogger({{
          .project_spec = std::move(project),
          .logger = std::move(logger_request),
      }})
      .Then([this](fidl::Result<fuchsia_metrics::MetricEventLoggerFactory::CreateMetricEventLogger>&
                       status) {
        logger_factory_ = fidl::Client<fuchsia_metrics::MetricEventLoggerFactory>();

        if (status.is_ok()) {
          logger_reconnection_backoff_.Reset();
          return;
        }

        if (status.error_value().is_framework_error()) {
          FX_PLOGS(WARNING, status.error_value().framework_error().status())
              << "Lost connection with fuchsia.metrics.MetricEventLoggerFactory";
          return;
        }

        if (status.error_value().domain_error() == fuchsia_metrics::Error::kShutDown) {
          FX_LOGS(INFO) << "Stopping sending Cobalt events";
          logger_ = fidl::Client<fuchsia_metrics::MetricEventLogger>();
        } else {
          FX_LOGS(WARNING) << "Failed to set up Cobalt: " << status.error_value();
          logger_ = fidl::Client<fuchsia_metrics::MetricEventLogger>();
          RetryConnectingToLogger();
        }
      });
}

void Logger::RetryConnectingToLogger() {
  if (logger_) {
    return;
  }

  // Bind |logger_| and immediately send the events that were not acknowledged by the server on the
  // previous connection.
  auto [client, server] = fidl::Endpoints<fuchsia_metrics::MetricEventLogger>::Create();
  logger_ = fidl::Client(std::move(client), dispatcher_, this);
  SendAllPendingEvents();

  reconnect_task_.Reset(
      [this, request = std::move(server)]() mutable { ConnectToLogger(std::move(request)); });

  PostDelayedTask(
      dispatcher_, [reconnect = reconnect_task_.callback()]() { reconnect(); },
      logger_reconnection_backoff_.GetNext());
}

void Logger::LogEvent(Event event) {
  if (pending_events_.size() >= kMaxPendingEvents) {
    FX_LOGS(INFO) << StringPrintf("Dropping Cobalt event %s - too many pending events (%lu)",
                                  event.ToString().c_str(), pending_events_.size());
    return;
  }

  const uint64_t event_id = next_event_id_++;
  pending_events_.insert(std::make_pair(event_id, std::move(event)));
  SendEvent(event_id);
}

uint64_t Logger::StartTimer() {
  const uint64_t timer_id = next_event_id_++;
  timer_starts_usecs_.insert(std::make_pair(timer_id, CurrentTimeUSecs(clock_)));
  return timer_id;
}

void Logger::SendEvent(uint64_t event_id) {
  if (!logger_) {
    return;
  }

  if (!pending_events_.contains(event_id)) {
    return;
  }
  Event& event = pending_events_.at(event_id);

  auto callback = [this, event_id, &event](const auto& result) {
    if (result.is_error()) {
      if (result.error_value().is_framework_error()) {
        return;
      }
      FX_LOGS(INFO) << StringPrintf("Cobalt logging error: status %s, event %s",
                                    result.error_value().FormatDescription().c_str(),
                                    event.ToString().c_str());
    }

    // We don't retry events that have been acknowledged by the server, regardless of the return
    // status.
    pending_events_.erase(event_id);
  };

  switch (event.type) {
    case EventType::kInteger:
      logger_
          ->LogInteger({{
              .metric_id = event.metric_id,
              .value = static_cast<int64_t>(event.count),
              .event_codes = event.dimensions,
          }})
          .Then(std::move(callback));
      break;
    case EventType::kOccurrence:
      logger_
          ->LogOccurrence({{
              .metric_id = event.metric_id,
              .count = event.count,
              .event_codes = event.dimensions,
          }})
          .Then(std::move(callback));
      break;
  }
}

void Logger::SendAllPendingEvents() {
  for (const auto& [event_id, _] : pending_events_) {
    SendEvent(event_id);
  }
}

uint64_t Logger::GetTimerDurationUSecs(uint64_t timer_id) const {
  FX_CHECK(timer_starts_usecs_.contains(timer_id));

  return CurrentTimeUSecs(clock_) - timer_starts_usecs_.at(timer_id);
}

}  // namespace cobalt
}  // namespace forensics
