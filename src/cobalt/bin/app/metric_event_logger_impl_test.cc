// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/metric_event_logger_impl.h"

#include <fuchsia/metrics/cpp/fidl.h>

#include <memory>
#include <optional>

#include <gtest/gtest.h>

#include "src/lib/cobalt/cpp/metric_event_builder.h"
#include "third_party/cobalt/src/logger/fake_logger.h"

namespace cobalt {
namespace {

class FailingLogger : public logger::testing::FakeLogger {
 public:
  void SetStatusForMetric(uint32_t metric_id, Status status) {
    statuses_[metric_id] = std::move(status);
  }

  Status LogOccurrence(uint32_t metric_id, uint64_t count,
                       const std::vector<uint32_t>& event_codes) override {
    if (auto it = statuses_.find(metric_id); it != statuses_.end()) {
      return it->second;
    }
    return FakeLogger::LogOccurrence(metric_id, count, event_codes);
  }

 private:
  std::map<uint32_t, Status> statuses_;
};

}  // namespace

class MetricEventLoggerImplTest : public ::testing::Test {
 public:
  MetricEventLoggerImplTest()
      : fake_logger_(new FailingLogger),
        logger_impl_(std::unique_ptr<logger::LoggerInterface>(fake_logger_)),
        logger_(&logger_impl_) {}

 protected:
  FailingLogger* fake_logger_;
  MetricEventLoggerImpl logger_impl_;
  fuchsia::metrics::MetricEventLogger* logger_;
};

TEST_F(MetricEventLoggerImplTest, PauseDuringBatch) {
  std::vector<fuchsia::metrics::MetricEvent> events;
  events.push_back(MetricEventBuilder(1).as_occurrence(1));
  events.push_back(MetricEventBuilder(1).as_occurrence(2));
  events.push_back(MetricEventBuilder(1).as_occurrence(3));
  events.push_back(MetricEventBuilder(1).as_occurrence(4));
  events.push_back(MetricEventBuilder(1).as_occurrence(5));
  std::optional<fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result> result;
  logger_->LogMetricEvents(
      std::move(events),
      [&result](fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result response) {
        result = std::move(response);
      });
  ASSERT_TRUE(result.has_value());
  EXPECT_TRUE(result->is_response());
  std::map<logger::LoggerCallsMadeMigratedMetricDimensionLoggerMethod, uint32_t>
      internal_logger_calls = fake_logger_->internal_logger_calls();
  // Only the LogMetricEvents call should be recorded.
  EXPECT_EQ(internal_logger_calls.size(), 1u);
  EXPECT_EQ(internal_logger_calls
                [logger::LoggerCallsMadeMigratedMetricDimensionLoggerMethod::LogMetricEvents],
            1u);
}

TEST_F(MetricEventLoggerImplTest, PreservesBufferFullError) {
  fake_logger_->SetStatusForMetric(2, Status(StatusCode::RESOURCE_EXHAUSTED, "buffer full"));
  fake_logger_->SetStatusForMetric(3, Status(StatusCode::INTERNAL, "internal error"));

  std::vector<fuchsia::metrics::MetricEvent> events;
  events.push_back(MetricEventBuilder(1).as_occurrence(1));
  events.push_back(MetricEventBuilder(2).as_occurrence(1));
  events.push_back(MetricEventBuilder(3).as_occurrence(1));
  events.push_back(MetricEventBuilder(4).as_occurrence(1));

  std::optional<fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result> result;
  logger_->LogMetricEvents(
      std::move(events),
      [&result](fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result response) {
        result = std::move(response);
      });

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_err());
  EXPECT_EQ(result->err(), fuchsia::metrics::Error::BUFFER_FULL);
  // Valid events in the batch before and after the failure are still processed.
  EXPECT_EQ(fake_logger_->call_count(), 2u);
}

TEST_F(MetricEventLoggerImplTest, PreservesInvalidArgumentsError) {
  std::vector<fuchsia::metrics::MetricEvent> events;
  fuchsia::metrics::MetricEvent uninitialized_event;
  uninitialized_event.metric_id = 1;
  events.push_back(std::move(uninitialized_event));

  std::optional<fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result> result;
  logger_->LogMetricEvents(
      std::move(events),
      [&result](fuchsia::metrics::MetricEventLogger_LogMetricEvents_Result response) {
        result = std::move(response);
      });

  ASSERT_TRUE(result.has_value());
  ASSERT_TRUE(result->is_err());
  EXPECT_EQ(result->err(), fuchsia::metrics::Error::INVALID_ARGUMENTS);
}

}  // namespace cobalt
