// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback_data/system_log_recorder/writer.h"

#include <fidl/fuchsia.logger/cpp/fidl.h>
#include <lib/syslog/cpp/log_level.h>

#include <memory>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/identity_decoder.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/identity_encoder.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/production_encoding.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/encoding/version.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/log_message_store.h"
#include "src/developer/forensics/feedback_data/system_log_recorder/reader.h"
#include "src/developer/forensics/testing/log_message.h"
#include "src/developer/forensics/testing/scoped_memfs_manager.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/developer/forensics/utils/log_format.h"
#include "src/developer/forensics/utils/redact/redactor.h"
#include "src/developer/forensics/utils/vmo.h"
#include "src/lib/files/directory.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"

namespace forensics {
namespace feedback_data {
namespace system_log_recorder {
namespace {

class WriterTest : public UnitTestFixture {
 public:
  WriterTest() : redactor_(inspect::BoolProperty{}) {}

  RedactorBase* GetIdentityRedactor() { return &redactor_; }

 private:
  IdentityRedactor redactor_;
};

::fpromise::result<fuchsia_logger::LogMessage, std::string> BuildLogMessage(
    const int32_t severity, const std::string& text,
    const zx::duration timestamp_offset = zx::duration(0),
    const std::vector<std::string>& tags = {}) {
  return ::fpromise::ok(forensics::BuildLogMessage(severity, text, timestamp_offset, tags));
}

// Only change "X" for one character. i.e. X -> 12 is not allowed.
const StorageSize kMaxLogLineSize =
    StorageSize::Bytes(Format(BuildLogMessage(FUCHSIA_LOG_INFO, "line X").value()).size());

const StorageSize kMaxDecompressedSize = StorageSize::Kilobytes(256);
const StorageSize kTotalLogSize = StorageSize::Kilobytes(256);

constexpr const char* kRootDirectory = "/root";
constexpr const char* kWriteDirectory = "/root/write";
constexpr const char* kPreviousLogsDirectory = "/root/previous";
constexpr const char* kReadDirectory = "/read";

class EncoderStub : public Encoder {
 public:
  EncoderStub() {}
  virtual ~EncoderStub() {}
  virtual EncodingVersion GetEncodingVersion() const { return EncodingVersion::kForTesting; }
  virtual std::string Encode(const std::string& msg) {
    input_.back() += msg;
    return msg;
  }
  virtual void Reset() { input_.push_back(""); }
  std::vector<std::string> GetInput() { return input_; }

 private:
  std::vector<std::string> input_ = {""};
};

class Decoder2x : public Decoder {
 public:
  Decoder2x() {}
  virtual ~Decoder2x() {}
  virtual EncodingVersion GetEncodingVersion() const { return EncodingVersion::kForTesting; }
  virtual std::string Decode(const std::string& msg) { return msg + msg; }
  virtual void Reset() {}
};

std::unique_ptr<Encoder> MakeIdentityEncoder() {
  return std::unique_ptr<Encoder>(new IdentityEncoder());
}

std::string MakeLogFilePath(const size_t file_num) {
  return files::JoinPath(kWriteDirectory, std::to_string(file_num));
}

TEST_F(WriterTest, VerifyFileOrdering) {
  // Set up the writer such that each file can fit 1 log message. When consuming a message the
  // end of block signal will be sent and a new empty file will be produced from file rotation.
  // From this behavior although we use 4 files, we only expect to retrieve the last 3 messages.
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize;
  const StorageSize kBufferSize = kMaxLogLineSize;

  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(), MakeIdentityEncoder(),
                        /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 4u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  // Written to file 0
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  writer.Write(store.Consume());

  // Written to file 1
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());

  // Written to file 2
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  writer.Write(store.Consume());

  // Written to file 3
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  // Written to file 4
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 5")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  IdentityDecoder decoder;

  std::string content;
  ASSERT_TRUE(files::ReadFileToString(MakeLogFilePath(2u), &content));
  EXPECT_EQ(content, R"([15604.000][07559][07687][] INFO: line 3
)");

  ASSERT_TRUE(files::ReadFileToString(MakeLogFilePath(3u), &content));
  EXPECT_EQ(content, R"([15604.000][07559][07687][] INFO: line 4
)");

  ASSERT_TRUE(files::ReadFileToString(MakeLogFilePath(4u), &content));
  EXPECT_EQ(content, R"([15604.000][07559][07687][] INFO: line 5
)");

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->compression_ratio, 1.0);

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
[15604.000][07559][07687][] INFO: line 5
)");
}

TEST_F(WriterTest, VerifyEncoderInput) {
  // Set up the writer such that each file can fit 2 log messages. We will then write 4 messages
  // and expect that the encoder receives 2 reset signals and encodes 2 log messages in each block.
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize * 2;
  const StorageSize kBufferSize = kMaxLogLineSize * 2;

  auto encoder = std::unique_ptr<EncoderStub>(new EncoderStub());
  EncoderStub* encoder_ptr = encoder.get();
  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(), std::move(encoder),
                        /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  writer.Write(store.Consume());
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  std::vector<std::string> input = encoder_ptr->GetInput();
  EXPECT_EQ(input.size(), (size_t)3);

  EXPECT_EQ(input[0], R"([15604.000][07559][07687][] INFO: line 1
[15604.000][07559][07687][] INFO: line 2
)");

  EXPECT_EQ(input[1], R"([15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
)");
}

TEST_F(WriterTest, WritesMessages) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  // Set up the writer such that each file can fit 2 log messages and the "!!! DROPPED..."
  // string.
  LogMessageStore store(kMaxLogLineSize * 2, kMaxLogLineSize * 2, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_FALSE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  IdentityDecoder decoder;

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->compression_ratio, 1.0);

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 0
[15604.000][07559][07687][] INFO: line 1
!!! DROPPED 1 MESSAGES !!!
)");

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  const fit::result<ReaderError, ConcatenateResult> result2 =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result2.is_ok());
  EXPECT_EQ(result2->compression_ratio, 1.0);

  EXPECT_EQ(result2->logs, R"([15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
)");
}

TEST_F(WriterTest, VerifyCompressionRatio) {
  // Generate 2x data when decoding. The decoder data output is not useful, just its size.
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  LogMessageStore store(kMaxLogLineSize * 4, kMaxLogLineSize * 4, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  Decoder2x decoder;

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->compression_ratio, 2.0);
}

TEST_F(WriterTest, VerifyProductionEcoding) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  // Set up the writer such that one file contains 5 log messages.
  auto encoder = std::unique_ptr<Encoder>(new ProductionEncoder());
  LogMessageStore store(kMaxLogLineSize * 5, kMaxLogLineSize * 5, GetIdentityRedactor(),
                        std::move(encoder), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  ProductionDecoder decoder;

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_FALSE(std::isnan(result->compression_ratio));

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 0
[15604.000][07559][07687][] INFO: line 1
[15604.000][07559][07687][] INFO: line 2
[15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
)");
}

TEST_F(WriterTest, FilesAlreadyPresent) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  {
    // Set up the writer such that one file contains at most 5 log messages.
    auto encoder = std::unique_ptr<Encoder>(new ProductionEncoder());
    LogMessageStore store(kMaxLogLineSize * 5, kMaxLogLineSize * 5, GetIdentityRedactor(),
                          std::move(encoder), /*ignore_before_timestamp=*/std::nullopt);
    store.TurnOnRateLimiting();

    SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                           /*previous_logs_dir=*/std::nullopt);

    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
    writer.Write(store.Consume());
  }
  {
    // Set up the writer such that one file contains at most 5 log messages.
    auto encoder = std::unique_ptr<Encoder>(new ProductionEncoder());
    LogMessageStore store(kMaxLogLineSize * 5, kMaxLogLineSize * 5, GetIdentityRedactor(),
                          std::move(encoder), /*ignore_before_timestamp=*/std::nullopt);
    store.TurnOnRateLimiting();

    SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                           /*previous_logs_dir=*/std::nullopt);

    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
    writer.Write(store.Consume());
  }

  memfs_manager.Create(kReadDirectory);
  ProductionDecoder decoder;

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_FALSE(std::isnan(result->compression_ratio));

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 0
[15604.000][07559][07687][] INFO: line 1
[15604.000][07559][07687][] INFO: line 2
[15604.000][07559][07687][] INFO: line 3
)");
}

TEST_F(WriterTest, FailCreateDirectory) {
  // Don't set up kRootDirectory
  ScopedMemFsManager memfs_manager;

  // Set up the writer such that each file can fit 2 log messages and the "!!! DROPPED..."
  // string.
  LogMessageStore store(kMaxLogLineSize * 2, kMaxLogLineSize * 2, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  // Create the kRootDirectory so kWriteDirectory can be made by |writer| after the next set of
  // writes.
  memfs_manager.Create(kRootDirectory);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_FALSE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  IdentityDecoder decoder;

  EXPECT_TRUE(Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder).is_error());

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->compression_ratio, 1.0);

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
)");
}

TEST_F(WriterTest, DirectoryDisappears) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  // Set up the writer such that each file can fit 2 log messages and the "!!! DROPPED..."
  // string.
  LogMessageStore store(kMaxLogLineSize * 2, kMaxLogLineSize * 2, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, 2u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  // Destroy kWriteDirectory so the next set of writes fail and the directory is recreated.
  ASSERT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_FALSE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());

  memfs_manager.Create(kReadDirectory);
  IdentityDecoder decoder;

  EXPECT_TRUE(Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder).is_error());

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());

  const fit::result<ReaderError, ConcatenateResult> result =
      Concatenate(kWriteDirectory, kMaxDecompressedSize, &decoder);
  ASSERT_TRUE(result.is_ok());
  EXPECT_EQ(result->compression_ratio, 1.0);

  EXPECT_EQ(result->logs, R"([15604.000][07559][07687][] INFO: line 3
[15604.000][07559][07687][] INFO: line 4
)");
}

TEST_F(WriterTest, IgnoreNonNumericFiles) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);
  ASSERT_TRUE(files::CreateDirectory(kWriteDirectory));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(kWriteDirectory, "0"), "data"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(kWriteDirectory, "invalid_file.log"), "data"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(kWriteDirectory, "1"), "data"));

  LogMessageStore store(kMaxLogLineSize * 5, kMaxLogLineSize * 5, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, 5u, kTotalLogSize, std::make_unique<IdentityDecoder>(),
                         /*previous_logs_dir=*/std::nullopt);

  // Additional writes should continue from file 2, ignoring invalid files.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  writer.Write(store.Consume());

  EXPECT_TRUE(files::IsFile(files::JoinPath(kWriteDirectory, "2")));
}

TEST_F(WriterTest, FlushAndReadLogs) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize * 5;
  const StorageSize kBufferSize = kMaxLogLineSize * 5;

  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0", zx::msec(100))));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1", zx::msec(200))));

  SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());

  ASSERT_TRUE(result.logs.is_ok());
  EXPECT_FALSE(result.cache_purged);
  SystemLogWriter::Logs logs = std::move(result.logs.value());
  ASSERT_TRUE(logs.vmo.is_valid());

  const zx::result<std::string> contents = StringFromVmo(logs.vmo);
  ASSERT_TRUE(contents.is_ok());

  EXPECT_EQ(*contents, R"([15604.100][07559][07687][] INFO: line 0
[15604.200][07559][07687][] INFO: line 1
)");
  ASSERT_TRUE(logs.first_timestamp.has_value());
  ASSERT_TRUE(logs.last_timestamp.has_value());
  EXPECT_EQ(*logs.first_timestamp, kLogMessageBaseTimestamp + zx::msec(100));
  EXPECT_EQ(*logs.last_timestamp, kLogMessageBaseTimestamp + zx::msec(200));
}

TEST_F(WriterTest, FlushAndReadLogsEmptyLogs) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize * 5;
  const StorageSize kBufferSize = kMaxLogLineSize * 5;

  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  const SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());

  ASSERT_TRUE(result.logs.is_error());
  EXPECT_FALSE(result.cache_purged);
  EXPECT_EQ(result.logs.error_value(), SystemLogWriter::WriterError::kIoError);
}

TEST_F(WriterTest, FlushAndReadLogsDetectsCachePurgeOnLogsDirectoryDeletion) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize * 5;
  const StorageSize kBufferSize = kMaxLogLineSize * 5;

  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0", zx::msec(100))));
  writer.Write(store.Consume());

  ASSERT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));

  SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());

  ASSERT_TRUE(result.logs.is_error());
  EXPECT_TRUE(result.cache_purged);
  EXPECT_EQ(result.logs.error_value(), SystemLogWriter::WriterError::kInsufficientCoverage);
}

TEST_F(WriterTest, FlushAndReadLogsSufficientCoverageAfterMinFilesWritten) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const StorageSize kBlockSize = kMaxLogLineSize;
  const StorageSize kBufferSize = kMaxLogLineSize;

  LogMessageStore store(kBlockSize, kBufferSize, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/8u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0", zx::msec(100))));
  writer.Write(store.Consume());

  EXPECT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));

  // Write blocks 1 to 4 post-purge (end_of_block = true).
  for (size_t i = 1; i <= 4; ++i) {
    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line " + std::to_string(i))));
    LogMessageStore::ConsumeResult consume_result = store.Consume();
    EXPECT_TRUE(consume_result.end_of_block);
    writer.Write(consume_result);

    SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());
    ASSERT_TRUE(result.logs.is_error());
    EXPECT_FALSE(result.cache_purged);
    EXPECT_EQ(result.logs.error_value(), SystemLogWriter::WriterError::kInsufficientCoverage);
  }

  // Write block 5 post-purge (end_of_block = true) -> reaches kMinFilesForSufficientCoverage (5).
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 5")));
  LogMessageStore::ConsumeResult consume_result = store.Consume();
  EXPECT_TRUE(consume_result.end_of_block);
  writer.Write(consume_result);

  SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());
  ASSERT_TRUE(result.logs.is_ok());
  EXPECT_FALSE(result.cache_purged);
}

TEST_F(WriterTest, WriteReturnsCachePurgedWhenLogsDirectoryDeleted) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  LogMessageStore store(kMaxLogLineSize * 10, kMaxLogLineSize * 10, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kOk);

  ASSERT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kCachePurge);
}

TEST_F(WriterTest, WriteReopensFileAfterPurge) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  LogMessageStore store(kMaxLogLineSize * 10, kMaxLogLineSize * 10, GetIdentityRedactor(),
                        std::make_unique<IdentityEncoder>(),
                        /*ignore_before_timestamp=*/std::nullopt);
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), /*previous_logs_dir=*/std::nullopt);

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 0")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kOk);

  ASSERT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kCachePurge);

  EXPECT_TRUE(files::IsDirectory(kWriteDirectory));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));

  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kOk);
}

TEST_F(WriterTest, EvictsPreviousBootFilesOldestFirstBeforeCurrentBoot) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  // Use boot sequence numbers 2 and 10, and file numbers 5 and 10, so lexicographical order
  // ("10" < "2", "10" < "5") differs from numeric order (2 < 10, 5 < 10).
  const std::string boot_2_dir = files::JoinPath(kPreviousLogsDirectory, "2");
  const std::string boot_10_dir = files::JoinPath(kPreviousLogsDirectory, "10");
  ASSERT_TRUE(files::CreateDirectory(boot_2_dir));
  ASSERT_TRUE(files::CreateDirectory(boot_10_dir));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_2_dir, "5"), "boot 2 file 5"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_2_dir, "10"), "boot 2 file 10"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_2_dir, "boot_info.json"), "{}"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_2_dir, "annotations.json"), "{}"));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_10_dir, "5"), "boot 10 file 5"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_10_dir, "10"), "boot 10 file 10"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_10_dir, "boot_info.json"), "{}"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_10_dir, "annotations.json"), "{}"));

  LogMessageStore store(kMaxLogLineSize, kMaxLogLineSize, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();

  // Budget of 5 files: 4 existing previous-boot files + file 0 created on construction = 5.
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/5u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), kPreviousLogsDirectory);

  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_2_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_2_dir, "10")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "10")));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));

  // Starting file 1 evicts boot 2 file 5.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_2_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_2_dir, "10")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "10")));

  // Starting file 2 evicts boot 2 file 10, leaving boot_info.json, annotations.json, and
  // boot_2_dir.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_2_dir, "10")));
  EXPECT_TRUE(files::IsDirectory(boot_2_dir));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_2_dir, "boot_info.json")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_2_dir, "annotations.json")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "10")));

  // Starting file 3 evicts boot 10 file 5.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_10_dir, "5")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "10")));

  // Starting file 4 evicts boot 10 file 10, leaving boot_info.json, annotations.json, and
  // boot_10_dir. Current boot files 0..4 are all still present.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_10_dir, "10")));
  EXPECT_TRUE(files::IsDirectory(boot_10_dir));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "boot_info.json")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_10_dir, "annotations.json")));
  for (size_t i = 0; i <= 4; ++i) {
    EXPECT_TRUE(files::IsFile(MakeLogFilePath(i)));
  }

  // Starting file 5 now evicts current boot file 0 since no previous-boot log files remain.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 5")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(MakeLogFilePath(0)));
  for (size_t i = 1; i <= 5; ++i) {
    EXPECT_TRUE(files::IsFile(MakeLogFilePath(i)));
  }
}

TEST_F(WriterTest, EvictsMultipleFilesInLoopWhenBudgetShrinks) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const std::string boot_0_dir = files::JoinPath(kPreviousLogsDirectory, "0");
  const std::string boot_1_dir = files::JoinPath(kPreviousLogsDirectory, "1");
  ASSERT_TRUE(files::CreateDirectory(boot_0_dir));
  ASSERT_TRUE(files::CreateDirectory(boot_1_dir));
  ASSERT_TRUE(files::CreateDirectory(kWriteDirectory));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "0"), "b0f0"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "1"), "b0f1"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "boot_info.json"), "{}"));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_1_dir, "0"), "b1f0"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_1_dir, "1"), "b1f1"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_1_dir, "boot_info.json"), "{}"));

  ASSERT_TRUE(files::WriteFile(MakeLogFilePath(0), "curr0"));
  ASSERT_TRUE(files::WriteFile(MakeLogFilePath(1), "curr1"));

  // 6 files exist on disk across boots, but max_num_files is lowered to 2 (e.g., after an OTA).
  // Starting new file 2 in constructor must evict all 4 previous-boot files and current-boot
  // file 0 so only current-boot files 1 and 2 remain.
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/2u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), kPreviousLogsDirectory);

  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_0_dir, "0")));
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_0_dir, "1")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_0_dir, "boot_info.json")));

  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_1_dir, "0")));
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_1_dir, "1")));
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_1_dir, "boot_info.json")));

  EXPECT_FALSE(files::IsFile(MakeLogFilePath(0)));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(1)));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(2)));
}

TEST_F(WriterTest, PreviousBootDeletedUnderneathWriter) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const std::string boot_0_dir = files::JoinPath(kPreviousLogsDirectory, "0");
  const std::string boot_1_dir = files::JoinPath(kPreviousLogsDirectory, "1");
  ASSERT_TRUE(files::CreateDirectory(boot_0_dir));
  ASSERT_TRUE(files::CreateDirectory(boot_1_dir));

  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "0"), "b0f0"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "1"), "b0f1"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_1_dir, "0"), "b1f0"));

  LogMessageStore store(kMaxLogLineSize, kMaxLogLineSize, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();

  // Budget of 4 files: 3 previous-boot files + current-boot file 0 = 4.
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/4u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), kPreviousLogsDirectory);
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));

  // Simulate Feedback pruning boot 0 (deleting 2 files underneath the writer).
  ASSERT_TRUE(files::DeletePath(boot_0_dir, /*recursive=*/true));

  // Writing a block starts file 1. The writer drops boot 0's first file, which is already gone,
  // from its queue, and is then within budget, so it does NOT evict boot 1's file.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  writer.Write(store.Consume());
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_1_dir, "0")));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(1)));

  // Writing another block starts file 2. The writer drops boot 0's second file, which is also
  // already gone, so still no eviction.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 2")));
  writer.Write(store.Consume());
  EXPECT_TRUE(files::IsFile(files::JoinPath(boot_1_dir, "0")));

  // Writing one more block starts file 3, which exceeds budget of 4 and evicts boot 1 file 0.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 3")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(files::JoinPath(boot_1_dir, "0")));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(3)));
}

TEST_F(WriterTest, CurrentBootGetsWholeBudgetAfterPreviousBootsArePurged) {
  ScopedMemFsManager memfs_manager;
  memfs_manager.Create(kRootDirectory);

  const std::string boot_0_dir = files::JoinPath(kPreviousLogsDirectory, "0");
  ASSERT_TRUE(files::CreateDirectory(boot_0_dir));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "0"), "b0f0"));
  ASSERT_TRUE(files::WriteFile(files::JoinPath(boot_0_dir, "1"), "b0f1"));

  LogMessageStore store(kMaxLogLineSize, kMaxLogLineSize, GetIdentityRedactor(),
                        MakeIdentityEncoder(), /*ignore_before_timestamp=*/std::nullopt);
  store.TurnOnRateLimiting();

  // Budget of 3 files: 2 previous-boot files + current-boot file 0 = 3.
  SystemLogWriter writer(kWriteDirectory, /*max_num_files=*/3u, kTotalLogSize,
                         std::make_unique<IdentityDecoder>(), kPreviousLogsDirectory);

  // A cache purge deletes both the current boot's and the previous boots' logs.
  ASSERT_TRUE(files::DeletePath(kWriteDirectory, /*recursive=*/true));
  ASSERT_TRUE(files::DeletePath(kPreviousLogsDirectory, /*recursive=*/true));
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 1")));
  EXPECT_EQ(writer.Write(store.Consume()), SystemLogWriter::WriteResult::kCachePurge);
  EXPECT_TRUE(files::IsFile(MakeLogFilePath(0)));

  SystemLogWriter::FlushAndReadLogsResult result = writer.FlushAndReadLogs(store.Consume());
  ASSERT_TRUE(result.logs.is_error());
  EXPECT_EQ(result.logs.error_value(), SystemLogWriter::WriterError::kInsufficientCoverage);

  // The current boot's files 0 to 2 fit in the budget.
  for (size_t i = 1; i <= 2; ++i) {
    EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line " + std::to_string(i + 1))));
    writer.Write(store.Consume());
  }
  for (size_t i = 0; i <= 2; ++i) {
    EXPECT_TRUE(files::IsFile(MakeLogFilePath(i)));
  }

  result = writer.FlushAndReadLogs(store.Consume());
  EXPECT_TRUE(result.logs.is_ok());

  // Starting file 3 evicts current-boot file 0.
  EXPECT_TRUE(store.Add(BuildLogMessage(FUCHSIA_LOG_INFO, "line 4")));
  writer.Write(store.Consume());
  EXPECT_FALSE(files::IsFile(MakeLogFilePath(0)));
  for (size_t i = 1; i <= 3; ++i) {
    EXPECT_TRUE(files::IsFile(MakeLogFilePath(i)));
  }
}

}  // namespace
}  // namespace system_log_recorder
}  // namespace feedback_data
}  // namespace forensics
