// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/syslog/cpp/macros.h>
#include <lib/trace-engine/buffer_internal.h>
#include <lib/trace-engine/fields.h>
#include <lib/zx/socket.h>
#include <lib/zx/vmo.h>
#include <zircon/syscalls.h>

#include <array>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>
#include <trace-reader/reader.h>
#include <trace-reader/records.h>

#include "src/performance/trace_manager/buffer_forwarder.h"
#include "src/performance/trace_manager/shared_buffer.h"

namespace tracing {
namespace test {
namespace {

using testing::ElementsAre;
using testing::IsEmpty;

constexpr size_t kBufferSize = 64 * 1024;
constexpr size_t kHeaderSize = sizeof(trace::internal::trace_buffer_header);
// Sized so that the durable buffer ends, and rolling buffer 0 starts, on a page boundary. A
// zero-length transfer at a page-aligned VMO offset is what makes zx_vmar_map fail.
constexpr size_t kPageSize = 4096;
constexpr size_t kDurableBufferSize = kPageSize - kHeaderSize;
constexpr size_t kRollingBufferSize = (kBufferSize - kHeaderSize - kDurableBufferSize) / 2;
constexpr uint64_t kDurableBufferOffset = kHeaderSize;
constexpr uint64_t kRollingBuffer0Offset = kHeaderSize + kDurableBufferSize;
constexpr uint64_t kRollingBuffer1Offset = kRollingBuffer0Offset + kRollingBufferSize;
constexpr size_t kTestRecordSizeBytes = 16;
constexpr uint32_t kProviderId = 1;
constexpr char kProviderName[] = "test-provider";

// Distinct values stored in each test record so tests can tell which records were forwarded.
constexpr trace_ticks_t kDurableTicks = 333;
constexpr trace_ticks_t kValidTicks = 222;
constexpr trace_ticks_t kStaleTicks = 111;
constexpr trace_ticks_t kNewTicks = 444;

void SetupStreamingBufferHeader(zx::unowned_vmo vmo, uint32_t wrapped_count,
                                uint64_t durable_data_end, uint64_t rolling_data_end_0,
                                uint64_t rolling_data_end_1) {
  trace::internal::trace_buffer_header header{};
  header.magic = TRACE_BUFFER_HEADER_MAGIC;
  header.version = TRACE_BUFFER_HEADER_V0;
  header.buffering_mode = TRACE_BUFFERING_MODE_STREAMING;
  header.total_size = kBufferSize;
  header.durable_buffer_size = kDurableBufferSize;
  header.rolling_buffer_size = kRollingBufferSize;
  header.wrapped_count = wrapped_count;
  header.durable_data_end = durable_data_end;
  header.rolling_data_end[0] = rolling_data_end_0;
  header.rolling_data_end[1] = rolling_data_end_1;
  FX_CHECK(vmo->write(&header, 0, sizeof(header)) == ZX_OK);
}

// Writes a two-word initialization record whose ticks_per_second field is |ticks| so that tests
// can identify which records were forwarded.
void WriteTestRecord(zx::unowned_vmo vmo, uint64_t offset, trace_ticks_t ticks) {
  constexpr size_t kNumWords = 2u;
  std::array<uint64_t, kNumWords> record{};
  record[0] =
      trace::RecordFields::Type::Make(trace::ToUnderlyingType(trace::RecordType::kInitialization)) |
      trace::RecordFields::RecordSize::Make(kNumWords);
  record[1] = static_cast<uint64_t>(ticks);
  FX_CHECK(vmo->write(record.data(), offset, trace::WordsToBytes(kNumWords)) == ZX_OK);
}

// Returns the ticks_per_second of every initialization record, in order, skipping the provider
// metadata record that TransferAll() and StreamingTransfer() always write first.
std::vector<trace_ticks_t> ForwardedTicks(const std::vector<trace::Record>& records) {
  std::vector<trace_ticks_t> ticks;
  EXPECT_FALSE(records.empty());
  // EXPECT_* doesn't stop the test, so return here rather than read records[0] out of bounds.
  if (records.empty()) {
    return ticks;
  }
  EXPECT_EQ(records[0].type(), trace::RecordType::kMetadata);
  for (size_t i = 1; i < records.size(); ++i) {
    EXPECT_EQ(records[i].type(), trace::RecordType::kInitialization) << "at index " << i;
    // EXPECT_* doesn't stop the test, and GetInitialization() aborts on any other record type.
    if (records[i].type() == trace::RecordType::kInitialization) {
      ticks.push_back(records[i].GetInitialization().ticks_per_second);
    }
  }
  return ticks;
}

class SharedBufferTest : public testing::Test {
 protected:
  SharedBufferTest()
      : reader_([this](trace::Record record) { records_.push_back(std::move(record)); },
                [](std::string_view error) { ADD_FAILURE() << "Reader error: " << error; }) {}

  void SetUp() override {
    ASSERT_EQ(zx_system_get_page_size(), kPageSize);
    zx::result<SharedBuffer> result = SharedBuffer::Create(
        kBufferSize, fuchsia_tracing::BufferingMode::kStreaming, kProviderName, kProviderId);
    ASSERT_TRUE(result.is_ok());
    buffer_.emplace(std::move(result.value()));
    zx::socket ep1;
    ASSERT_EQ(zx::socket::create(0, &ep0_, &ep1), ZX_OK);
    forwarder_ = std::make_shared<BufferForwarder>(std::move(ep1));
  }

  // Reads all currently buffered bytes from |ep0_| into the fixture's persistent |reader_| (so
  // provider registration from an earlier StreamingTransfer is preserved) and returns the newly
  // decoded records.
  std::vector<trace::Record> ReadRecordsFromSocket() {
    records_.clear();
    std::vector<uint64_t> buffer(trace::BytesToWords(kBufferSize));
    size_t actual = 0;
    FX_CHECK(ep0_.read(0, buffer.data(), kBufferSize, &actual) == ZX_OK);
    EXPECT_EQ(actual % sizeof(uint64_t), 0u);
    zx_signals_t pending = 0;
    EXPECT_EQ(ep0_.wait_one(ZX_SOCKET_READABLE, zx::time::infinite_past(), &pending),
              ZX_ERR_TIMED_OUT);

    trace::Chunk chunk(buffer.data(), trace::BytesToWords(actual));
    EXPECT_TRUE(reader_.ReadRecords(chunk));
    EXPECT_EQ(chunk.remaining_words(), 0u);
    return std::move(records_);
  }

  // Advances |buffer_|'s streaming state during test setup and drains the resulting bytes from the
  // socket so the subsequent Act/Assert phase only observes records from the operation under test.
  void AdvanceStreamingState(uint32_t wrapped_count, uint64_t durable_data_end) {
    FX_CHECK(buffer_->StreamingTransfer(forwarder_, wrapped_count, durable_data_end));
    (void)ReadRecordsFromSocket();
  }

  zx::unowned_vmo vmo() const { return buffer_->Vmo(); }

  std::optional<SharedBuffer> buffer_;
  zx::socket ep0_;
  std::shared_ptr<BufferForwarder> forwarder_;
  std::vector<trace::Record> records_;
  trace::TraceReader reader_;
};

TEST_F(SharedBufferTest, UnchangedDurableEndAtPageBoundaryStillForwardsRollingBuffer) {
  // The durable buffer was completely filled and transferred by StreamingTransfer, so the next
  // durable byte to transfer is at the page-aligned VMO offset where rolling buffer 0 starts.
  for (uint64_t offset = 0; offset < kDurableBufferSize; offset += kTestRecordSizeBytes) {
    WriteTestRecord(vmo(), kDurableBufferOffset + offset, kDurableTicks);
  }
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kDurableBufferSize,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kDurableBufferSize);
  // Tracing stops after the provider has written a record to rolling buffer 1 without adding
  // anything to the durable buffer.
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kDurableBufferSize,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/kTestRecordSizeBytes);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kValidTicks));
}

TEST_F(SharedBufferTest, DurableEndBeforeTransferredOffsetReportsProviderError) {
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kDurableBufferOffset + kTestRecordSizeBytes, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0,
                             /*durable_data_end=*/2 * kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, 2 * kTestRecordSizeBytes);
  // The header claims less durable data than was already transferred, which a well-behaved
  // provider never reports within a single run.
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  EXPECT_EQ(status, TransferStatus::kProviderError);
}

TEST_F(SharedBufferTest, StreamingFlushThenStopDoesNotResendUnchangedBuffer) {
  // Buffers 0 and 1 are each filled and saved. A Flush then switches back to buffer 0 (wrap 2) and
  // tracing stops before anything new is written, so buffer 0 still holds its wrap 0 record and
  // the finalized header reports it as empty.
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/kTestRecordSizeBytes);
  AdvanceStreamingState(/*wrapped_count=*/1, kTestRecordSizeBytes);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/2, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), IsEmpty());
}

TEST_F(SharedBufferTest, StreamingBufferWrittenAfterSaveIsScanned) {
  // Buffer 0 is saved with two records, then written again (wrap 2) by a provider that exits
  // without updating the header. The first record at offset 0 is identical to wrap 0 and only the
  // second record at offset 16 differs, exercising the fingerprint window past the first record.
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kStaleTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset + kTestRecordSizeBytes, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/2 * kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/kTestRecordSizeBytes);
  AdvanceStreamingState(/*wrapped_count=*/1, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer0Offset + kTestRecordSizeBytes, kNewTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/2, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kStaleTicks, kNewTicks));
}

TEST_F(SharedBufferTest, StreamingEmptySaveDoesNotMarkUnsavedBufferAsSaved) {
  // If StreamingTransfer() receives rolling_data_end == 0 (for example from a racing Flush or a
  // provider that has not updated rolling_data_end), it forwards nothing and must not record the
  // unforwarded buffer's fingerprint as saved.
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kValidTicks));
}

TEST_F(SharedBufferTest, StreamingNeverSavedBufferIsScannedEvenIfItMatchesTheOtherBuffer) {
  // Only buffer 0 has been saved. Buffer 1 starts with the same bytes buffer 0 did, but it was
  // never forwarded, so its zero end must still be treated as an unfinalized header and scanned.
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);

  auto [status, stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kStaleTicks));
}

TEST_F(SharedBufferTest, StreamingUnchangedBufferFromPreviousRunIsNotResent) {
  // Run 1 saves buffer 0 via StreamingTransfer() and forwards buffer 1 via TransferAll() at Stop.
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kStaleTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/kTestRecordSizeBytes);
  auto [run1_status, run1_stats] = buffer_->TransferAll(forwarder_);
  ASSERT_EQ(run1_status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kStaleTicks));

  // Run 2 (CLEAR_ENTIRE) stops completely idle. Buffer 0 still holds run 1's record, which was
  // already forwarded by StreamingTransfer(), so only the durable buffer is transferred.
  buffer_->OnStart(fuchsia_tracing::BufferDisposition::kClearEntire);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);
  auto [run2_status, run2_stats] = buffer_->TransferAll(forwarder_);
  ASSERT_EQ(run2_status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), ElementsAre(kDurableTicks));

  // Run 3 (CLEAR_ENTIRE) saves buffer 0, then a Flush switches to buffer 1 and tracing stops
  // before anything is written to it. Buffer 1 still holds run 1's record, which was only ever
  // forwarded by TransferAll() at the end of run 1.
  buffer_->OnStart(fuchsia_tracing::BufferDisposition::kClearEntire);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/0, /*rolling_data_end_1=*/0);

  auto [run3_status, run3_stats] = buffer_->TransferAll(forwarder_);

  ASSERT_EQ(run3_status, TransferStatus::kComplete);
  EXPECT_THAT(ForwardedTicks(ReadRecordsFromSocket()), IsEmpty());
}

TEST_F(SharedBufferTest, OnStartClearEntireResetsStreamingOffsets) {
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/kTestRecordSizeBytes);
  AdvanceStreamingState(/*wrapped_count=*/1, kTestRecordSizeBytes);

  buffer_->OnStart(fuchsia_tracing::BufferDisposition::kClearEntire);

  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  bool success = buffer_->StreamingTransfer(forwarder_, /*wrapped_count=*/0, kTestRecordSizeBytes);

  EXPECT_TRUE(success);
}

TEST_F(SharedBufferTest, OnStartClearNondurableResetsWrappedCountOnly) {
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kDurableBufferOffset + kTestRecordSizeBytes, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0,
                             /*durable_data_end=*/2 * kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, 2 * kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1,
                             /*durable_data_end=*/2 * kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/kTestRecordSizeBytes);
  AdvanceStreamingState(/*wrapped_count=*/1, 2 * kTestRecordSizeBytes);

  buffer_->OnStart(fuchsia_tracing::BufferDisposition::kClearNondurable);

  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0,
                             /*durable_data_end=*/2 * kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  bool success =
      buffer_->StreamingTransfer(forwarder_, /*wrapped_count=*/0, 2 * kTestRecordSizeBytes);

  EXPECT_TRUE(success);
}

TEST_F(SharedBufferTest, OnStartRetainPreservesStreamingOffsets) {
  WriteTestRecord(vmo(), kDurableBufferOffset, kDurableTicks);
  WriteTestRecord(vmo(), kRollingBuffer0Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/0, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes, /*rolling_data_end_1=*/0);
  AdvanceStreamingState(/*wrapped_count=*/0, kTestRecordSizeBytes);
  WriteTestRecord(vmo(), kRollingBuffer1Offset, kValidTicks);
  SetupStreamingBufferHeader(vmo(), /*wrapped_count=*/1, /*durable_data_end=*/kTestRecordSizeBytes,
                             /*rolling_data_end_0=*/kTestRecordSizeBytes,
                             /*rolling_data_end_1=*/kTestRecordSizeBytes);
  AdvanceStreamingState(/*wrapped_count=*/1, kTestRecordSizeBytes);

  buffer_->OnStart(fuchsia_tracing::BufferDisposition::kRetain);

  bool wrapped_zero_accepted =
      buffer_->StreamingTransfer(forwarder_, /*wrapped_count=*/0, kTestRecordSizeBytes);

  EXPECT_FALSE(wrapped_zero_accepted);
}

}  // namespace
}  // namespace test
}  // namespace tracing
