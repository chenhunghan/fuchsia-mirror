// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/namespace_init.h"

#include <lib/syslog/cpp/macros.h>

#include <vector>

#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/production_encoding.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/version.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/reader.h"
#include "src/developer/forensics/utils/cobalt/metrics.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/lib/fxl/strings/string_printf.h"
#include "src/lib/fxl/strings/trim.h"

namespace forensics::feedback {
namespace {

constexpr size_t kMaxBootIdTimelineSize = 10;

}

void MoveFile(const std::string& from, const std::string& to) {
  // Bail if the file doesn't exist.
  if (!files::IsFile(from)) {
    return;
  }

  // Bail if the file can't be read.
  std::string content;
  if (!files::ReadFileToString(from, &content)) {
    FX_LOGS(ERROR) << "Failed to read file " << from;
    return;
  }

  // Copy the file content – we cannot move as the two files are under different namespaces.
  if (!files::WriteFile(to, content)) {
    FX_LOGS(ERROR) << "Failed to write file " << to;
    return;
  }

  // Delete the original file.
  if (!files::DeletePath(from, /*recursive=*/true)) {
    FX_LOGS(ERROR) << "Failed to delete " << from;
  }
}

bool TestAndSetNotAFdr(const std::string& not_a_fdr_file) {
  if (files::IsFile(not_a_fdr_file)) {
    return true;
  }

  if (!files::WriteFile(not_a_fdr_file, "", 0u)) {
    FX_LOGS(ERROR) << "Failed to create " << not_a_fdr_file;
  }

  return false;
}

void CreatePreviousLogsFile(cobalt::Logger* cobalt, const StorageSize max_decompressed_size,
                            const std::optional<zx::duration> last_boot_uptime,
                            const std::string& dir, const std::string& write_path) {
  // We read the set of /cache files into a single /tmp file.
  //
  // |max_decompressed_size| is typically overkill for a |decoder|'s preallocations because the log
  // is spread across many files. However, it's efficient because the total log size is usually
  // large enough to be serviced by scudo's secondary allocator and the memory can be decommitted
  // immediately after it's freed.
  feedback_data::system_log_recorder::ProductionDecoder decoder(max_decompressed_size.ToBytes());
  const fit::result<feedback_data::system_log_recorder::ReaderError,
                    feedback_data::system_log_recorder::ConcatenateResult>
      result =
          feedback_data::system_log_recorder::Concatenate(dir, max_decompressed_size, &decoder);
  if (result.is_error()) {
    FX_LOGS(WARNING) << "Could not concatenate log files";
    return;
  }

  if (!files::WriteFile(write_path, result->logs)) {
    FX_LOGS(WARNING) << "Could not write the log file: " << write_path;
    return;
  }

  FX_LOGS(INFO) << fxl::StringPrintf(
      "Found logs from previous boot cycle (compression ratio %.2f), available at %s\n",
      result->compression_ratio, write_path.c_str());

  cobalt->LogCount(feedback_data::system_log_recorder::ToCobalt(decoder.GetEncodingVersion()),
                   (uint64_t)(result->compression_ratio * 100));

  if (result->last_timestamp.has_value() && last_boot_uptime.has_value()) {
    const int64_t delta_ms =
        (zx::time_boot(last_boot_uptime->get()) - *result->last_timestamp).to_msecs();

    cobalt->LogEvent(cobalt::Event(
        cobalt::EventType::kInteger, cobalt_registry::kPreviousBootLogLastTimestampDeltaMetricId,
        {cobalt_registry::FeedbackMetricDimensionProvider::Feedback}, delta_ms));
  }

  if (result->is_at_capacity) {
    cobalt->LogEvent(
        cobalt::Event(cobalt::EventType::kInteger,
                      cobalt_registry::kPreviousBootLogDeduplicatedMessageCountAtCapacityMetricId,
                      {cobalt_registry::FeedbackMetricDimensionProvider::Feedback},
                      result->deduplicated_message_count));

    cobalt->LogEvent(cobalt::Event(cobalt::EventType::kInteger,
                                   cobalt_registry::kPreviousBootLogMessageCountAtCapacityMetricId,
                                   {cobalt_registry::FeedbackMetricDimensionProvider::Feedback},
                                   result->message_count));

    if (result->first_timestamp.has_value() && result->last_timestamp.has_value() &&
        *result->last_timestamp >= *result->first_timestamp) {
      const zx::duration duration = *result->last_timestamp - *result->first_timestamp;
      cobalt->LogEvent(cobalt::Event(
          cobalt::EventType::kInteger, cobalt_registry::kPreviousBootLogDurationAtCapacityMetricId,
          {cobalt_registry::FeedbackMetricDimensionProvider::Feedback}, duration.to_mins()));
    }
  }

  // Clean up the /cache files now that they have been concatenated into a single /tmp file.
  files::DeletePath(dir, /*recursive=*/true);
}

void MoveAndRecordBootId(const std::string& new_boot_id, const std::string& previous_boot_id_path,
                         const std::string& current_boot_id_path,
                         const std::string& timeline_path) {
  MoveFile(/*from=*/current_boot_id_path, /*to=*/previous_boot_id_path);
  files::WriteFile(current_boot_id_path, new_boot_id);

  std::string timeline_content;
  if (!files::ReadFileToString(timeline_path, &timeline_content)) {
    timeline_content = "";
  }

  timeline_content = std::string(fxl::TrimString(timeline_content, "[]"));

  std::vector<std::string> timeline =
      fxl::SplitStringCopy(timeline_content, ",", fxl::kTrimWhitespace, fxl::kSplitWantNonEmpty);
  timeline.insert(timeline.begin(), new_boot_id);

  if (timeline.size() > kMaxBootIdTimelineSize) {
    timeline.resize(kMaxBootIdTimelineSize);
  }

  files::WriteFile(timeline_path, "[" + fxl::JoinStrings(timeline, ", ") + "]");
}

void MoveAndRecordBuildVersion(const std::string& current_build_version,
                               const std::string& previous_build_version_path,
                               const std::string& current_build_version_path) {
  MoveFile(/*from=*/current_build_version_path, /*to=*/previous_build_version_path);
  files::WriteFile(current_build_version_path, current_build_version);
}

}  // namespace forensics::feedback
