// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_LOG_BUFFER_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_LOG_BUFFER_H_

#include <fuchsia/logger/cpp/fidl.h>
#include <lib/fpromise/result.h>
#include <lib/zx/time.h>

#include <deque>
#include <optional>
#include <string>
#include <vector>

#include "src/developer/forensics/utils/redact/redactor.h"
#include "src/developer/forensics/utils/storage_size.h"

namespace forensics::feedback {

// Stores up to |capacity| bytes of system log messages, dropping the earliest messages when the
// stored messages occupy too much space.
class LogBuffer {
 public:
  LogBuffer(StorageSize capacity, RedactorBase* redactor);

  // Adds |message| to the buffer and drops messages as required to keep the total size under
  // |capacity|.
  //
  // Messages are assumed to be received mostly in order.
  void Add(::fpromise::result<fuchsia::logger::LogMessage, std::string> message);

  // Returns std::nullopt if the buffer is empty.
  std::optional<zx::time_boot> FirstTimestamp() const;

  // Returns std::nullopt if the buffer is empty.
  std::optional<zx::time_boot> LastTimestamp() const;

  std::string ToString();

 private:
  struct Message {
    Message(const ::fpromise::result<fuchsia::logger::LogMessage, std::string>& message,
            zx::time_boot default_timestamp);

    zx::time_boot timestamp;
    std::string msg;
  };

  void Sort();
  void EnforceCapacity();

  // Resets variables keeping track of the last message
  void ResetLastMessage();

  RedactorBase* redactor_;
  std::deque<Message> messages_;

  std::string last_msg_{};
  int32_t last_severity_{};
  std::vector<std::string> last_tags{};
  size_t last_msg_repeated_{0u};

  bool is_sorted_{true};

  size_t size_{0u};
  const size_t capacity_;
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_DATA_LOG_BUFFER_H_
