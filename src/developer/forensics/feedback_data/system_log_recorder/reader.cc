// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/system_log_recorder/reader.h"

#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <lib/zx/time.h>

#include <algorithm>
#include <cctype>
#include <charconv>
#include <functional>
#include <optional>
#include <string>
#include <vector>

#include "src/developer/forensics/feedback_data/constants.h"
#include "src/developer/forensics/utils/log_format.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/fxl/strings/string_number_conversions.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {
namespace {

constexpr size_t kFirstFileNumber = 0u;

struct PostProcessResult {
  std::string logs;
  uint64_t message_count;
  uint64_t deduplicated_message_count;
  std::optional<zx::time_boot> first_timestamp;
  std::optional<zx::time_boot> last_timestamp;
};

// Returns a view starting at the next line in |str| or an empty view if none are left.
std::string_view ToNextLine(std::string_view str) {
  const size_t pos = str.find_first_of('\n');
  return (pos != std::string_view::npos && pos != str.size()) ? str.substr(pos + 1)
                                                              : std::string_view();
}

// Returns true if |str| starts like a repeated message line.
bool MatchesRepeat(std::string_view str) {
  constexpr std::string_view kPrefix(kRepeatedStrPrefix);
  if (str.size() < kPrefix.size() || str.substr(0, kPrefix.size()) != kPrefix) {
    return false;
  }

  str.remove_prefix(kPrefix.size());
  return !str.empty() && std::isdigit(static_cast<unsigned char>(str.front()));
}

// Splits message into 3 parts.
//  1) The portion of the message before the first sequence of repeated warnings.
//  2) The aggregated count of the sequence of repeated warnings.
//  3) The portion of the message after the first sequence of repeated warnings.
//
// Note, 2) and 3) will be default initialized if no repeated warnings are present.
std::tuple<std::string_view, size_t, std::string_view> SplitRepeatedCount(
    const std::string_view message) {
  constexpr std::string_view kPrefix(kRepeatedStrPrefix);

  std::string_view head;
  size_t repeated{0};
  std::string_view tail(message);

  // Find the first line matching the repeated message warning.
  while (!tail.empty()) {
    if (MatchesRepeat(tail)) {
      head = std::string_view(message.data(), tail.data() - message.data());
      break;
    }

    tail = ToNextLine(tail);
  }

  // No repeated warning found.
  if (tail.empty()) {
    return {message, 0, std::string_view()};
  }

  // Aggregate the repeated counts and find the end of the consecutive repeated message warnings.
  while (!tail.empty()) {
    if (!MatchesRepeat(tail)) {
      break;
    }

    std::string_view new_tail = tail;
    new_tail.remove_prefix(kPrefix.size());
    if (new_tail.empty() || !std::isdigit(static_cast<unsigned char>(new_tail.front()))) {
      break;
    }

    int r{0};
    if (std::from_chars(new_tail.data(), new_tail.data() + new_tail.size(), r).ec != std::errc{}) {
      FX_LOGS(ERROR) << "Failed to extract repeat count from '" << new_tail << "'";
      break;
    }

    repeated += r;
    tail = ToNextLine(new_tail);
  }

  return {head, repeated, tail};
}

// Returns the messages which make up |log|.
std::vector<std::string_view> ParseMessages(const std::string_view log) {
  if (log.empty()) {
    return {};
  }

  // Overallocate to prevent residual memory when growing |messages|.
  //
  // 20,000 was selected because log files can be large and this is a large enough allocation to
  // allow the allocator to decommit the memory backing |messages| when the object is destroyed.
  // This is technically dedends implementation details of scudo, but is considered acceptable
  // due to the prior residual impact of processing the previous boot log
  // (https://fxbug.dev/42071246).
  std::vector<std::string_view> messages;
  messages.reserve(20000);

  // Consider the first line the start of a new message, regardless of whether or not it matches a
  // message. It initially spans all of |log| and is trimmed below if another message follows.
  messages.emplace_back(log);

  std::string_view remainder = ToNextLine(log.substr(1));
  while (!remainder.empty()) {
    // Find either the start of the next message in |log| or the end of |log|.
    while (!remainder.empty() && !ParseLogHeader(remainder).has_value()) {
      remainder = ToNextLine(remainder);
    }

    // Resize the last message so it ends at the start of the next message or the end of |log|.
    const char* const end = (!remainder.empty()) ? remainder.data() : log.data() + log.size();
    messages.back() = std::string_view(messages.back().data(), end - messages.back().data());

    if (remainder.empty()) {
      break;
    }

    messages.emplace_back(remainder.substr(0, 1));
    remainder.remove_prefix(1);
  }

  return messages;
}

PostProcessResult PostProcess(const std::string_view log) {
  std::vector<std::string_view> messages = ParseMessages(log);
  if (messages.empty()) {
    return PostProcessResult{
        .logs = "",
        .message_count = 0,
        .deduplicated_message_count = 0,
        .first_timestamp = std::nullopt,
        .last_timestamp = std::nullopt,
    };
  }

  // Don't sort with the first message if it doesn't have a timestamp.
  auto begin = messages.begin();
  if (!ParseLogHeader(*begin).has_value()) {
    ++begin;
  }

  std::stable_sort(begin, messages.end(),
                   [](const std::string_view lhs, const std::string_view rhs) {
                     return ParseLogHeader(lhs) < ParseLogHeader(rhs);
                   });

  // The writer already merged consecutive repetitions of a message, so each message in |messages|
  // counts once towards the deduplicated message count. Because |messages| is sorted, the first
  // and last messages have the earliest and latest timestamps.
  const uint64_t deduplicated_message_count = std::distance(begin, messages.end());
  const std::optional<zx::time_boot> first_timestamp =
      (begin != messages.end()) ? ParseLogHeader(*begin) : std::nullopt;
  const std::optional<zx::time_boot> last_timestamp =
      (begin != messages.end()) ? ParseLogHeader(messages.back()) : std::nullopt;

  std::string final_log;
  final_log.reserve(log.size());
  uint64_t repeated_message_count = 0;

  // Appends |message| to |final_log| while recursively accumulating the repeated count for each
  // message.
  std::function<void(std::string_view)> AddMessage = [&final_log, &repeated_message_count,
                                                      &AddMessage](const std::string_view message) {
    if (message.empty()) {
      return;
    }

    const auto [head, repeat_count, tail] = SplitRepeatedCount(message);
    repeated_message_count += repeat_count;

    final_log.append(head);

    if (repeat_count == 1) {
      final_log.append(kRepeatedOnceFormatStr);
    } else if (repeat_count > 1) {
      final_log.append(kRepeatedStrPrefix);
      final_log.append(std::to_string(repeat_count));
      final_log.append(kRepeatedStrSuffix);
    }

    AddMessage(tail);
  };

  for (const std::string_view message : messages) {
    AddMessage(message);
  }

  return PostProcessResult{
      .logs = std::move(final_log),
      .message_count = deduplicated_message_count + repeated_message_count,
      .deduplicated_message_count = deduplicated_message_count,
      .first_timestamp = first_timestamp,
      .last_timestamp = last_timestamp,
  };
}

}  // namespace

fit::result<ReaderError, ConcatenateResult> Concatenate(const std::string& logs_dir,
                                                        const StorageSize max_decompressed_size,
                                                        Decoder* decoder) {
  if (!files::IsDirectory(logs_dir)) {
    FX_LOGS(WARNING) << "No logs directory found: " << logs_dir;
    return fit::error(ReaderError::kIoError);
  }

  std::vector<std::string> dir_contents;
  files::ReadDirContents(logs_dir, &dir_contents);

  // Only read the files the writer created, which are named after their number.
  std::vector<size_t> file_nums;
  for (const std::string& fname : dir_contents) {
    size_t file_num = 0;
    if (fxl::StringToNumberWithError(fname, &file_num)) {
      file_nums.push_back(file_num);
    }
  }

  // Sort the files based on the number the previous writer assigned them. The lower the number,
  // the older the file.
  std::sort(file_nums.begin(), file_nums.end());

  std::string uncompressed_log;
  uncompressed_log.reserve(max_decompressed_size.ToBytes());

  std::string buffer;
  buffer.reserve(max_decompressed_size.ToBytes());

  uint64_t total_compressed_log_size{0};
  for (const size_t file_num : file_nums) {
    const std::string path = files::JoinPath(logs_dir, std::to_string(file_num));
    if (!files::ReadFileToString(path, &buffer)) {
      continue;
    }

    total_compressed_log_size += buffer.size();
    uncompressed_log.append(decoder->Decode(buffer));
  }

  if (total_compressed_log_size == 0) {
    FX_LOGS(WARNING) << "The encoded log is empty";
    return fit::error(ReaderError::kIoError);
  }

  if (uncompressed_log.empty()) {
    FX_LOGS(WARNING) << "The decoded log is empty";
    return fit::error(ReaderError::kDecompressionError);
  }

  // Sort logs and combine messages for repeated logs.
  PostProcessResult post_processed = PostProcess(uncompressed_log);
  if (post_processed.logs.empty()) {
    FX_LOGS(WARNING) << "The post-processed log is empty";
    return fit::error(ReaderError::kDecompressionError);
  }

  // Compression ratio rounded up to the next decimal, e.g., 2.54x compression -> 2.6x.
  const uint32_t decimal_ratio =
      (static_cast<uint32_t>(post_processed.logs.size()) * 10 - 1) / total_compressed_log_size + 1;

  return fit::ok(ConcatenateResult{
      .logs = std::move(post_processed.logs),
      .compression_ratio = static_cast<float>(decimal_ratio) / 10.0f,
      .is_at_capacity = !file_nums.empty() && file_nums.front() > kFirstFileNumber,
      .message_count = post_processed.message_count,
      .deduplicated_message_count = post_processed.deduplicated_message_count,
      .first_timestamp = post_processed.first_timestamp,
      .last_timestamp = post_processed.last_timestamp,
  });
}

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics
