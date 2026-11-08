// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_READER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_READER_H_

#include <lib/fit/result.h>
#include <lib/zx/time.h>

#include <cstdint>
#include <optional>
#include <string>

#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/decoder.h"
#include "src/developer/forensics/utils/storage_size.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {

enum class ReaderError : std::uint8_t {
  kIoError,
  kDecompressionError,
};

struct ConcatenateResult {
  // The decoded logs, sorted by timestamp and with repeated messages merged.
  std::string logs;

  // The size of |logs| divided by the size of the encoded log files, rounded up to one decimal.
  float compression_ratio;

  // Whether log files were deleted to make room for newer logs, i.e. the oldest logs were lost.
  bool is_at_capacity;

  // The number of messages that were logged, including repeated messages.
  uint64_t message_count;

  // The number of messages that were logged, not counting consecutive repeats of a message.
  uint64_t deduplicated_message_count;

  // The earliest and latest timestamps in |logs|, if any message has a timestamp.
  std::optional<zx::time_boot> first_timestamp;
  std::optional<zx::time_boot> last_timestamp;
};

// Reads the encoded logs in |logs_dir|, decodes them using |decoder|, sorts log messages
// chronologically by timestamp, and aggregates consecutive repeated messages. Calculates the
// resulting compression ratio.
//
// Returns the processed log string and metadata on success, or a ReaderError on failure.
fit::result<ReaderError, ConcatenateResult> Concatenate(const std::string& logs_dir,
                                                        StorageSize max_decompressed_size,
                                                        Decoder* decoder);

}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_SYSTEM_LOG_RECORDER_READER_H_
