// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/bin/ufsutil/rpmb.h"

#include <endian.h>
#include <fidl/fuchsia.hardware.rpmb/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <cstring>
#include <initializer_list>
#include <optional>
#include <span>
#include <sstream>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <openssl/digest.h>
#include <openssl/hmac.h>
#include <zxtest/zxtest.h>

#include "src/devices/block/bin/ufsutil/rpmb-frame.h"
#include "src/devices/block/bin/ufsutil/ufsutil.h"
#include "src/lib/files/file.h"
#include "src/lib/files/scoped_temp_dir.h"

namespace ufsutil {
namespace {

constexpr uint64_t kFrameSize = fuchsia_hardware_rpmb::wire::kFrameSize;
// Both UFS RPMB servers reject tx or rx buffers larger than 16 frames.
constexpr size_t kMaxTransferFrames = 16;
constexpr uint64_t kMaxTransferSize = kMaxTransferFrames * kFrameSize;
// Both UFS RPMB servers send at most two frames per SECURITY PROTOCOL OUT command.
constexpr size_t kMaxFramesPerOut = 2;
// The block other clients write to in tests of concurrent writes.
constexpr uint16_t kForeignAddress = 60;
constexpr size_t kBlockCount = 64;
constexpr size_t kBlockSize = sizeof(RpmbFrame::data);

// The number of frames the device returns for a failed authenticated data read.
enum class ErrorFrames {
  kOne,
  kRequested,
};

// How the driver splits `tx_frames` into SECURITY PROTOCOL OUT commands.
enum class Split {
  // C++ `rpmb.cc`: two frames per command.
  kTwoFrames,
  // Rust `rpmb.rs`: min(2, reliable_write_sector_count) frames per command.
  kReliableWriteSectorCount,
};

// A write by another client, such as the TEE, between the write request and the result read
// request of the tool.
struct ForeignWrite {
  uint16_t address = kForeignAddress;
  RpmbResult result = RpmbResult::kOk;
};

// What the driver does when SECURITY PROTOCOL IN returns fewer frames than `rx_frames` holds.
enum class ShortResponse {
  // C++ `rpmb.cc`: copies its shared DMA buffer as is, so the rest of `rx_frames` holds frames of
  // earlier commands.
  kStaleFrames,
  // Rust `rpmb.rs`: fails the request with ZX_ERR_IO on the residual.
  kIoError,
};

// A SECURITY PROTOCOL command that reached the fake device.
struct SecurityCommand {
  bool out;
  size_t frames;

  bool operator==(const SecurityCommand&) const = default;
};

constexpr SecurityCommand Out(size_t frames) { return {.out = true, .frames = frames}; }
constexpr SecurityCommand In(size_t frames) { return {.out = false, .frames = frames}; }

// Checks the fields that JESD220H 12.4.7.3.3 requires in every frame of an authenticated data
// write request: the request type, the block count of the whole access, the start address, and the
// current write counter. Only the last frame carries a MAC.
bool WriteFramesAreWellFormed(std::span<const RpmbFrame> frames) {
  const RpmbFrame& last = frames.back();
  for (size_t i = 0; i < frames.size(); ++i) {
    const RpmbFrame& frame = frames[i];
    if (be16toh(frame.req_resp) != kRpmbAuthenticatedDataWriteRequest ||
        frame.block_count != last.block_count || frame.address != last.address ||
        frame.write_counter != last.write_counter) {
      return false;
    }
    if (i + 1 < frames.size() &&
        !std::ranges::all_of(frame.key_mac, [](uint8_t byte) { return byte == 0; })) {
      return false;
    }
  }
  return true;
}

uint8_t BlockByte(size_t block, size_t offset) { return static_cast<uint8_t>(block * 7 + offset); }

using Key = std::array<uint8_t, kRpmbKeySize>;

constexpr Key MakeKey(uint8_t seed) {
  Key key{};
  for (size_t i = 0; i < key.size(); ++i) {
    key[i] = static_cast<uint8_t>(seed + i * 3);
  }
  return key;
}

constexpr Key kKey = MakeKey(1);

// Computes the MAC field by field, as ufs-utils does, so that the test does not share the frame
// offset arithmetic of `rpmb.cc`.
Key Mac(const Key& key, std::span<const RpmbFrame> frames) {
  std::vector<uint8_t> input;
  const auto append = [&input](const auto& field) {
    const auto* bytes = reinterpret_cast<const uint8_t*>(&field);
    input.insert(input.end(), bytes, bytes + sizeof(field));
  };
  for (const RpmbFrame& frame : frames) {
    append(frame.data);
    append(frame.nonce);
    append(frame.write_counter);
    append(frame.address);
    append(frame.block_count);
    append(frame.result);
    append(frame.req_resp);
  }
  Key mac{};
  unsigned int mac_size = 0;
  ZX_ASSERT(HMAC(EVP_sha256(), key.data(), key.size(), input.data(), input.size(), mac.data(),
                 &mac_size) != nullptr);
  ZX_ASSERT(mac_size == mac.size());
  return mac;
}

void WriteKeyFile(const std::string& path, const Key& key) {
  ZX_ASSERT(files::WriteFile(path, reinterpret_cast<const char*>(key.data()), key.size()));
}

// Writes `blocks` blocks whose bytes start at `seed` to `path`.
std::vector<uint8_t> WriteDataFile(const char* path, size_t blocks, uint8_t seed) {
  std::vector<uint8_t> data(blocks * kBlockSize);
  for (size_t i = 0; i < data.size(); ++i) {
    data[i] = static_cast<uint8_t>(seed + i);
  }
  ZX_ASSERT(files::WriteFile(path, reinterpret_cast<const char*>(data.data()), data.size()));
  return data;
}

std::string FileContents(const char* path) {
  std::string contents;
  files::ReadFileToString(path, &contents);
  return contents;
}

// Serves `fuchsia.hardware.rpmb/Rpmb` like a UFS driver in front of an RPMB well-known LU. The
// server splits requests into SECURITY PROTOCOL commands the way the drivers do and moves frames
// through one buffer shared by all commands, and the device model answers each command, so tests
// observe what a real device would receive.
class FakeRpmb : public fidl::WireServer<fuchsia_hardware_rpmb::Rpmb> {
 public:
  FakeRpmb() {
    for (size_t block = 0; block < kBlockCount; ++block) {
      for (size_t offset = 0; offset < kBlockSize; ++offset) {
        blocks_[block][offset] = BlockByte(block, offset);
      }
    }
  }

  void GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) override {
    fuchsia_hardware_rpmb::wire::EmmcDeviceInfo info{
        .cid = cid,
        .rpmb_size = rpmb_size,
        .reliable_write_sector_count = reliable_write_sector_count,
    };
    completer.Reply(fuchsia_hardware_rpmb::wire::DeviceInfo::WithEmmcInfo(
        fidl::ObjectView<fuchsia_hardware_rpmb::wire::EmmcDeviceInfo>::FromExternal(&info)));
  }

  void Request(RequestRequestView request, RequestCompleter::Sync& completer) override {
    if (request_error != ZX_OK) {
      completer.ReplyError(request_error);
      return;
    }
    const fuchsia_mem::wire::Range& tx = request->request.tx_frames;
    if (tx.size < kFrameSize || tx.size % kFrameSize != 0) {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
    if (tx.size > kMaxTransferSize) {
      completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
      return;
    }
    const fuchsia_mem::wire::Range* rx =
        request->request.rx_frames && request->request.rx_frames->size > 0
            ? request->request.rx_frames.get()
            : nullptr;
    if (rx != nullptr && rx->size % kFrameSize != 0) {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
    if (rx != nullptr && rx->size > kMaxTransferSize) {
      completer.ReplyError(ZX_ERR_OUT_OF_RANGE);
      return;
    }

    const size_t tx_count = tx.size / kFrameSize;
    const size_t frames_per_out =
        split == Split::kTwoFrames
            ? kMaxFramesPerOut
            : std::clamp<size_t>(reliable_write_sector_count, 1, kMaxFramesPerOut);
    for (size_t sent = 0; sent < tx_count; sent += frames_per_out) {
      const size_t count = std::min(frames_per_out, tx_count - sent);
      ASSERT_OK(tx.vmo.read(buffer_.data(), tx.offset + sent * kFrameSize, count * kFrameSize));
      SecurityProtocolOut(std::span(buffer_).first(count));
    }

    if (rx != nullptr) {
      if (zx::result<> status = SecurityProtocolIn(rx->size / kFrameSize); status.is_error()) {
        completer.ReplyError(status.error_value());
        return;
      }
      ASSERT_OK(rx->vmo.write(buffer_.data(), rx->offset, rx->size));
    }
    completer.ReplySuccess();
  }

  fidl::Array<uint8_t, 16> cid{};
  uint8_t rpmb_size = 4;
  uint8_t reliable_write_sector_count = 2;
  bool key_programmed = true;
  Key key = kKey;
  uint32_t write_counter = 0;
  uint16_t extra_result_bits = 0;
  bool corrupt_nonce = false;
  bool corrupt_response_type = false;
  // The response type whose MAC the device corrupts.
  std::optional<uint16_t> corrupt_mac_type;
  bool corrupt_write_result_type = false;
  // The frames of an authenticated data read response carry the address after the requested one.
  bool corrupt_read_address = false;
  Split split = Split::kTwoFrames;
  // Another client writes block 60 between the write counter read and the write request.
  bool foreign_write_before_write = false;
  std::optional<ForeignWrite> foreign_write_before_result_read;
  // Another client reads the write counter between the write request and the result read request.
  bool foreign_counter_read_before_result_read = false;
  // The raw result of the next authenticated data write, which then writes nothing.
  std::optional<uint16_t> write_result_raw;
  ErrorFrames error_frames = ErrorFrames::kOne;
  ShortResponse short_response = ShortResponse::kStaleFrames;
  // A response frame the device leaves unwritten in the shared buffer.
  std::optional<size_t> unwritten_frame;
  zx_status_t request_error = ZX_OK;

  const std::array<uint8_t, kBlockSize>& block(size_t index) const { return blocks_[index]; }

  std::vector<SecurityCommand> commands;
  // Set when SECURITY PROTOCOL IN asked for a different number of frames than the device prepared.
  bool response_size_mismatch = false;

 private:
  void SecurityProtocolOut(std::span<const RpmbFrame> frames) {
    commands.push_back(Out(frames.size()));
    const RpmbFrame& request = frames[0];
    const uint16_t type = be16toh(request.req_resp);
    pending_.clear();
    switch (type) {
      case kRpmbWriteCounterReadRequest:
        if (frames.size() == 1) {
          pending_.push_back(CounterResponse(request));
        }
        break;
      case kRpmbAuthenticatedDataReadRequest: {
        if (frames.size() != 1) {
          break;
        }
        const uint16_t address = be16toh(request.address);
        const uint16_t count = be16toh(request.block_count);
        RpmbResult result = KeyResult();
        if (result == RpmbResult::kOk && count == 0) {
          result = RpmbResult::kGeneralFailure;
        } else if (result == RpmbResult::kOk && size_t{address} + count > kBlockCount) {
          result = RpmbResult::kAddressFailure;
        }
        const size_t frame_count =
            result == RpmbResult::kOk || error_frames == ErrorFrames::kRequested
                ? std::max<size_t>(count, 1)
                : 1;
        for (size_t i = 0; i < frame_count; ++i) {
          RpmbFrame& response =
              pending_.emplace_back(Response(request, kRpmbAuthenticatedDataReadResponse, result));
          if (result == RpmbResult::kOk) {
            std::ranges::copy(blocks_[address + i], response.data);
          }
          if (corrupt_read_address) {
            response.address = htobe16(static_cast<uint16_t>(address + 1));
          }
        }
        SignResponse(pending_);
        break;
      }
      case kRpmbAuthenticatedDataWriteRequest:
        if (foreign_write_before_write) {
          foreign_write_before_write = false;
          ++write_counter;
        }
        result_register_ = Write(frames);
        break;
      case kRpmbResultReadRequest:
        if (frames.size() == 1 && result_register_.has_value()) {
          if (std::optional<ForeignWrite> foreign =
                  std::exchange(foreign_write_before_result_read, std::nullopt)) {
            if (foreign->result == RpmbResult::kOk) {
              ++write_counter;
            }
            result_register_ =
                WriteResult(foreign->address, static_cast<uint16_t>(foreign->result));
          }
          if (std::exchange(foreign_counter_read_before_result_read, false)) {
            result_register_ = CounterResponse(RpmbFrame{});
          }
          pending_.push_back(*result_register_);
        }
        break;
      default:
        break;
    }
    // Every request other than a result read overwrites the result register.
    if (type != kRpmbResultReadRequest && type != kRpmbAuthenticatedDataWriteRequest &&
        !pending_.empty()) {
      result_register_ = pending_.back();
    }
  }

  RpmbFrame CounterResponse(const RpmbFrame& request) const {
    std::vector<RpmbFrame> response{Response(request, kRpmbWriteCounterReadResponse, KeyResult())};
    response[0].write_counter = htobe32(write_counter);
    SignResponse(response);
    return response[0];
  }

  zx::result<> SecurityProtocolIn(size_t count) {
    commands.push_back(In(count));
    response_size_mismatch |= count != pending_.size();
    if (pending_.size() < count && short_response == ShortResponse::kIoError) {
      return zx::error(ZX_ERR_IO);
    }
    for (size_t i = 0; i < std::min(count, pending_.size()); ++i) {
      if (i != unwritten_frame) {
        buffer_[i] = pending_[i];
      }
    }
    return zx::ok();
  }

  // Executes an authenticated data write and returns the frame for the next result read.
  RpmbFrame Write(std::span<const RpmbFrame> frames) {
    const RpmbFrame& last = frames.back();
    const uint16_t address = be16toh(last.address);
    const uint16_t count = be16toh(last.block_count);
    if (std::optional<uint16_t> raw = std::exchange(write_result_raw, std::nullopt)) {
      return WriteResult(address, *raw);
    }
    RpmbResult result = KeyResult();
    // A write of more than bRPMB_ReadWriteSize blocks fails with a general failure.
    if (result == RpmbResult::kOk &&
        (count != frames.size() || count > reliable_write_sector_count)) {
      result = RpmbResult::kGeneralFailure;
    } else if (result == RpmbResult::kOk && !WriteFramesAreWellFormed(frames)) {
      result = RpmbResult::kGeneralFailure;
    } else if (result == RpmbResult::kOk && !std::ranges::equal(Mac(key, frames), last.key_mac)) {
      result = RpmbResult::kAuthenticationFailure;
    } else if (result == RpmbResult::kOk && be32toh(last.write_counter) != write_counter) {
      result = RpmbResult::kCounterFailure;
    } else if (result == RpmbResult::kOk && size_t{address} + count > kBlockCount) {
      result = RpmbResult::kAddressFailure;
    }
    if (result == RpmbResult::kOk) {
      for (size_t i = 0; i < count; ++i) {
        std::ranges::copy(frames[i].data, blocks_[address + i].begin());
      }
      ++write_counter;
    }
    return WriteResult(address,
                       static_cast<uint16_t>(static_cast<uint16_t>(result) | extra_result_bits));
  }

  RpmbFrame WriteResult(uint16_t address, uint16_t raw_result) const {
    std::vector<RpmbFrame> response(1);
    response[0].address = htobe16(address);
    response[0].write_counter = htobe32(write_counter);
    response[0].result = htobe16(raw_result);
    response[0].req_resp = htobe16(corrupt_write_result_type ? kRpmbAuthenticatedDataReadResponse
                                                             : kRpmbAuthenticatedDataWriteResponse);
    SignResponse(response);
    return response[0];
  }

  // Puts the MAC of `frames` in the last frame.
  void SignResponse(std::span<RpmbFrame> frames) const {
    if (!key_programmed || frames.empty()) {
      return;
    }
    RpmbFrame& last = frames.back();
    std::ranges::copy(Mac(key, frames), last.key_mac);
    if (corrupt_mac_type == be16toh(last.req_resp)) {
      last.key_mac[0] ^= 0xff;
    }
  }

  RpmbResult KeyResult() const {
    return key_programmed ? RpmbResult::kOk : RpmbResult::kKeyNotProgrammed;
  }

  RpmbFrame Response(const RpmbFrame& request, uint16_t type, RpmbResult result) const {
    RpmbFrame response{};
    std::ranges::copy(request.nonce, response.nonce);
    if (corrupt_nonce) {
      response.nonce[0] ^= 0xff;
    }
    response.address = request.address;
    response.block_count = request.block_count;
    response.result =
        htobe16(static_cast<uint16_t>(static_cast<uint16_t>(result) | extra_result_bits));
    response.req_resp = htobe16(corrupt_response_type ? static_cast<uint16_t>(type + 1) : type);
    return response;
  }

  std::array<std::array<uint8_t, kBlockSize>, kBlockCount> blocks_;
  std::vector<RpmbFrame> pending_;
  std::optional<RpmbFrame> result_register_;
  // The DMA buffer that the C++ driver shares between all SECURITY PROTOCOL commands.
  std::array<RpmbFrame, kMaxTransferFrames> buffer_{};
};

class RpmbTest : public zxtest::Test {
 public:
  RpmbTest() : loop_(&kAsyncLoopConfigNeverAttachToThread) {}

  void SetUp() override {
    ASSERT_OK(loop_.StartThread("rpmb-test-loop"));
    key_file_ = OutPath("rpmb-key.bin");
    wrong_key_file_ = OutPath("rpmb-wrong-key.bin");
    WriteKeyFile(key_file_, kKey);
    WriteKeyFile(wrong_key_file_, MakeKey(2));
  }

  void TearDown() override { loop_.Shutdown(); }

  // Runs `RunRpmb` with `args` against `fake_` and returns the standard output on success.
  zx::result<std::string> RunCommand(std::initializer_list<const char*> args) {
    auto endpoints = fidl::Endpoints<fuchsia_hardware_rpmb::Rpmb>::Create();
    fidl::BindServer(loop_.dispatcher(), std::move(endpoints.server), &fake_);
    return RunWithClient(std::move(endpoints.client), args);
  }

  zx::result<std::string> RunWithClient(fidl::ClientEnd<fuchsia_hardware_rpmb::Rpmb> client,
                                        std::initializer_list<const char*> args) {
    std::ostringstream out;
    err_.str("");
    std::vector<char*> argv;
    for (const char* arg : args) {
      argv.push_back(const_cast<char*>(arg));
    }
    argv.push_back(nullptr);
    zx::result<> status = RunRpmb(fidl::WireSyncClient(std::move(client)),
                                  static_cast<int>(args.size()), argv.data(), out, err_);
    if (status.is_error()) {
      return status.take_error();
    }
    return zx::ok(out.str());
  }

  bool ErrorContains(std::string_view text) const {
    return err_.str().find(text) != std::string::npos;
  }

  // Returns true if blocks `address`.. of the fake device hold `data`.
  bool BlocksHold(size_t address, std::span<const uint8_t> data) const {
    for (size_t i = 0; i < data.size(); ++i) {
      if (fake_.block(address + i / kBlockSize)[i % kBlockSize] != data[i]) {
        return false;
      }
    }
    return true;
  }

  // Returns a path under this test's temporary directory.
  std::string OutPath(const std::string& name) { return temp_dir_.path() + "/" + name; }

  files::ScopedTempDir temp_dir_;
  std::string key_file_;
  std::string wrong_key_file_;
  FakeRpmb fake_;
  std::ostringstream err_;
  async::Loop loop_;
};

constexpr char kRpmbPath[] = "/svc/fuchsia.hardware.rpmb.Service/0/device";

TEST(RpmbCommandTest, IsRpmbCommand) {
  EXPECT_TRUE(IsRpmbCommand("rpmb"));
  EXPECT_FALSE(IsRpmbCommand("rpmb-info"));
  EXPECT_FALSE(IsRpmbCommand("info"));
  EXPECT_FALSE(IsRpmbCommand("read-desc"));
}

TEST(RpmbCommandTest, TopLevelUsageListsRpmbCommands) {
  std::ostringstream out;
  Initialize();
  PrintUsage(out);
  EXPECT_NE(std::string::npos, out.str().find("rpmb counter"));
  EXPECT_NE(std::string::npos, out.str().find("fuchsia.hardware.rpmb.Service"));
}

TEST_F(RpmbTest, Help) {
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "--help"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Usage: ufsutil <rpmb-device> rpmb read"));
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, UnknownOption) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter", "--bogus"}),
                ZX_ERR_INVALID_ARGS);
}

TEST_F(RpmbTest, MissingOrUnknownCommand) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb"}), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "bogus"}), ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("missing or unknown RPMB command"));
  EXPECT_TRUE(fake_.commands.empty());
}

// getopt_long permutes non-option arguments behind the options it parses. A command that takes no
// positional arguments must reject them instead of silently using the defaults.
TEST_F(RpmbTest, RejectsPositionalArguments) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "4", "8", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("unexpected argument '4'"));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "info", "extra"}), ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("unexpected argument 'extra'"));
  EXPECT_TRUE(fake_.commands.empty());
}

// zircon musl resets its short-option cluster cursor only when optind is 0. A call that returns
// from the middle of a cluster must not shift the options of the next call.
TEST_F(RpmbTest, ParsesOptionsAfterHelpInCluster) {
  ASSERT_OK(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-hf"}));
  ASSERT_OK(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "3", "-n", "1", "-f",
                        OutPath("rpmb-read.bin").c_str()}));
  std::vector<uint8_t> data;
  ASSERT_TRUE(files::ReadFileToVector(OutPath("rpmb-read.bin"), &data));
  ASSERT_EQ(kBlockSize, data.size());
  EXPECT_EQ(BlockByte(3, 0), data[0]);
}

TEST_F(RpmbTest, NonRpmbCommand) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "read-desc"}), ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, Info) {
  fake_.rpmb_size = 4;
  fake_.reliable_write_sector_count = 4;
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "info"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("RPMB size                  : 512 KiB"));
  EXPECT_NE(std::string::npos, output->find("Reliable write sector count: 4"));
  EXPECT_NE(std::string::npos, output->find("Frames per read request    : 4"));
  EXPECT_NE(std::string::npos, output->find("Frames per write request   : 2"));
  EXPECT_NE(std::string::npos, output->find("CID                        : not reported"));
  EXPECT_TRUE(fake_.commands.empty());
}

// The C++ driver reports bRPMB_ReadWriteSize unbounded, but both drivers accept at most 16 frames
// per request.
TEST_F(RpmbTest, InfoBoundsFramesPerRead) {
  fake_.reliable_write_sector_count = 64;
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "info"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Reliable write sector count: 64"));
  EXPECT_NE(std::string::npos, output->find("Frames per read request    : 16"));
  EXPECT_NE(std::string::npos, output->find("Frames per write request   : 2"));
}

TEST_F(RpmbTest, Counter) {
  fake_.write_counter = 0x1234;
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Write counter: 4660"));
  EXPECT_NE(std::string::npos, output->find("MAC not verified"));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1)}));
  EXPECT_FALSE(fake_.response_size_mismatch);
}

TEST_F(RpmbTest, CounterExpired) {
  fake_.write_counter = 0xffffffff;
  fake_.extra_result_bits = kRpmbResultWriteCounterExpired;
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Write counter: 4294967295"));
  EXPECT_NE(std::string::npos, output->find("expired"));
}

TEST_F(RpmbTest, CounterKeyNotProgrammed) {
  fake_.key_programmed = false;
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"}), ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("authentication key not yet programmed"));
}

TEST_F(RpmbTest, CounterRejectsWrongNonce) {
  fake_.corrupt_nonce = true;
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"}), ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("nonce"));
}

TEST_F(RpmbTest, CounterRejectsWrongResponseType) {
  fake_.corrupt_response_type = true;
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"}), ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("response type"));
}

TEST_F(RpmbTest, DriverError) {
  fake_.request_error = ZX_ERR_IO;
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter"}), ZX_ERR_IO);
}

TEST_F(RpmbTest, ClosedConnectionSuggestsRpmbPath) {
  auto [client, server] = fidl::Endpoints<fuchsia_hardware_rpmb::Rpmb>::Create();
  server.TakeChannel().reset();
  EXPECT_STATUS(
      RunWithClient(std::move(client),
                    {"ufsutil", "/svc/fuchsia.hardware.ufs.Service/0/device", "rpmb", "info"}),
      ZX_ERR_PEER_CLOSED);
  EXPECT_TRUE(ErrorContains("/svc/fuchsia.hardware.rpmb.Service/<instance_id>/device"));
}

TEST_F(RpmbTest, Read) {
  fake_.reliable_write_sector_count = 2;
  const std::string file = OutPath("rpmb-read.bin");
  zx::result<std::string> output = RunCommand(
      {"ufsutil", kRpmbPath, "rpmb", "read", "--start", "3", "--count", "5", "-f", file.c_str()});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("MAC not verified"));

  std::vector<uint8_t> data;
  ASSERT_TRUE(files::ReadFileToVector(file, &data));
  ASSERT_EQ(5 * kBlockSize, data.size());
  for (size_t i = 0; i < data.size(); ++i) {
    ASSERT_EQ(BlockByte(3 + i / kBlockSize, i % kBlockSize), data[i], "byte %zu", i);
  }
  // Each read request asks for at most reliable_write_sector_count frames.
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(2), Out(1), In(2), Out(1), In(1)}));
  EXPECT_FALSE(fake_.response_size_mismatch);
}

TEST_F(RpmbTest, ReadSplitsAtSixteenFrames) {
  fake_.reliable_write_sector_count = 64;
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "0",
                                               "-n", "20", "-f", OutPath("rpmb-read.bin").c_str()});
  ASSERT_OK(output);

  std::vector<uint8_t> data;
  ASSERT_TRUE(files::ReadFileToVector(OutPath("rpmb-read.bin").c_str(), &data));
  ASSERT_EQ(20 * kBlockSize, data.size());
  for (size_t i = 0; i < data.size(); ++i) {
    ASSERT_EQ(BlockByte(i / kBlockSize, i % kBlockSize), data[i], "byte %zu", i);
  }
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(16), Out(1), In(4)}));
  EXPECT_FALSE(fake_.response_size_mismatch);
}

TEST_F(RpmbTest, ReadDefaultsToOneBlockAtAddressZero) {
  ASSERT_OK(
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-f", OutPath("rpmb-read.bin").c_str()}));
  std::vector<uint8_t> data;
  ASSERT_TRUE(files::ReadFileToVector(OutPath("rpmb-read.bin").c_str(), &data));
  ASSERT_EQ(kBlockSize, data.size());
  EXPECT_EQ(BlockByte(0, 1), data[1]);
}

TEST_F(RpmbTest, ReadRequiresFile) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-n", "1"}), ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, ReadChecksOutputDirectoryFirst) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-f",
                            OutPath("missing-dir/out.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, ReadRejectsRangePastLastAddress) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "0xffff", "-n", "2", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "0x10000", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-n", "0", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

// The device returns one error frame, and the rest of `rx_frames` holds a frame of the previous
// read (type 0x0400, result OK, another nonce).
TEST_F(RpmbTest, ReadReportsDeviceResultInOneFrame) {
  ASSERT_OK(RunCommand(
      {"ufsutil", kRpmbPath, "rpmb", "read", "-n", "2", "-f", OutPath("rpmb-read.bin").c_str()}));
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "63", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("RPMB authenticated data read failed: address failure"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, ReadReportsDeviceResultInRequestedFrames) {
  fake_.error_frames = ErrorFrames::kRequested;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "63", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("RPMB authenticated data read failed: address failure"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
  EXPECT_FALSE(fake_.response_size_mismatch);
}

// The Rust driver fails the request when the device returns fewer frames than requested, so the
// device result does not reach the client.
TEST_F(RpmbTest, ReadFailsWhenDriverRejectsShortResponse) {
  fake_.short_response = ShortResponse::kIoError;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "63", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO);
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

// Only the last frame is valid, but a frame before it holds data of the previous read.
TEST_F(RpmbTest, ReadRejectsStaleFrameBeforeLastFrame) {
  fake_.reliable_write_sector_count = 4;
  ASSERT_OK(RunCommand(
      {"ufsutil", kRpmbPath, "rpmb", "read", "-n", "4", "-f", OutPath("rpmb-read.bin").c_str()}));
  fake_.unwritten_frame = 1;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "8", "-n", "3", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("nonce"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, ReadKeyNotProgrammed) {
  fake_.key_programmed = false;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("authentication key not yet programmed"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, ReadFailureInSecondRequestLeavesFileUntouched) {
  fake_.reliable_write_sector_count = 2;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "62", "-n", "4", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("address failure"));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(2), Out(1), In(2)}));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, ReadRejectsWrongNonce) {
  fake_.corrupt_nonce = true;
  EXPECT_STATUS(
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-f", OutPath("rpmb-read.bin").c_str()}),
      ZX_ERR_IO_DATA_INTEGRITY);
}

TEST_F(RpmbTest, ReadRejectsResponseForOtherBlocks) {
  fake_.corrupt_read_address = true;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "5", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("address 0x0006"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, CounterVerifiesMac) {
  fake_.write_counter = 7;
  zx::result<std::string> output =
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter", "--key", key_file_.c_str()});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Write counter: 7"));
  EXPECT_NE(std::string::npos, output->find("MAC verified"));
  EXPECT_EQ(std::string::npos, output->find("MAC not verified"));
}

TEST_F(RpmbTest, CounterRejectsMacOfOtherKey) {
  EXPECT_STATUS(
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter", "-k", wrong_key_file_.c_str()}),
      ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("MAC"));
}

TEST_F(RpmbTest, CounterRejectsKeyOfWrongSize) {
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-short-key.bin").c_str(), "short", 5));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "counter", "-k",
                            OutPath("rpmb-short-key.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("32 bytes"));
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, ReadVerifiesMac) {
  fake_.reliable_write_sector_count = 2;
  zx::result<std::string> output =
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "3", "-n", "3", "-f",
                  OutPath("rpmb-read.bin").c_str(), "-k", key_file_.c_str()});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("MAC verified"));
  EXPECT_EQ(std::string::npos, output->find("MAC not verified"));

  std::vector<uint8_t> data;
  ASSERT_TRUE(files::ReadFileToVector(OutPath("rpmb-read.bin").c_str(), &data));
  ASSERT_EQ(3 * kBlockSize, data.size());
  EXPECT_EQ(BlockByte(5, 9), data[2 * kBlockSize + 9]);
}

TEST_F(RpmbTest, ReadRejectsWrongMac) {
  fake_.corrupt_mac_type = kRpmbAuthenticatedDataReadResponse;
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-untouched.bin").c_str(), "old", 3));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-n", "2", "-f",
                            OutPath("rpmb-untouched.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("MAC"));
  EXPECT_EQ("old", FileContents(OutPath("rpmb-untouched.bin").c_str()));
}

TEST_F(RpmbTest, WriteOneBlock) {
  fake_.write_counter = 5;
  const std::vector<uint8_t> data = WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  zx::result<std::string> output =
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "--start", "9", "--file",
                  OutPath("rpmb-write.bin").c_str(), "--key", key_file_.c_str()});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("Wrote 1 blocks (256 bytes)"));
  EXPECT_TRUE(BlocksHold(9, data));
  EXPECT_EQ(6u, fake_.write_counter);
  // Write counter read, then the write and the result read in separate OUT commands.
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(1), Out(1), In(1)}));
  EXPECT_FALSE(fake_.response_size_mismatch);
}

// A write must reach the device in one SECURITY PROTOCOL OUT command, and the drivers send at most
// two frames per command.
TEST_F(RpmbTest, WriteSendsAtMostTwoFramesPerRequest) {
  fake_.reliable_write_sector_count = 4;
  const std::vector<uint8_t> data = WriteDataFile(OutPath("rpmb-write.bin").c_str(), 3, 0x10);
  ASSERT_OK(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "20", "-f",
                        OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}));
  EXPECT_TRUE(BlocksHold(20, data));
  EXPECT_EQ(2u, fake_.write_counter);
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(2), Out(1), In(1),  //
                                         Out(1), In(1), Out(1), Out(1), In(1)}));
}

// The Rust driver sends at most reliable_write_sector_count frames per OUT command, and the device
// rejects writes of more blocks.
TEST_F(RpmbTest, WriteSendsOneFramePerRequestWhenDeviceReportsOne) {
  fake_.split = Split::kReliableWriteSectorCount;
  fake_.reliable_write_sector_count = 1;
  const std::vector<uint8_t> data = WriteDataFile(OutPath("rpmb-write.bin").c_str(), 2, 0x20);
  ASSERT_OK(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                        OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}));
  EXPECT_TRUE(BlocksHold(0, data));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(1), Out(1), In(1),  //
                                         Out(1), In(1), Out(1), Out(1), In(1)}));
}

TEST_F(RpmbTest, WriteRequiresFileAndKey) {
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(
      RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-f", OutPath("rpmb-write.bin").c_str()}),
      ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("-k/--key"));
  EXPECT_TRUE(fake_.commands.empty());
}

// A write has no default address: a forgotten `--start` must not overwrite block 0.
TEST_F(RpmbTest, WriteRequiresStart) {
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("-s/--start"));
  EXPECT_TRUE(fake_.commands.empty());
  EXPECT_EQ(0u, fake_.write_counter);
  EXPECT_EQ(BlockByte(0, 0), fake_.block(0)[0]);
}

TEST_F(RpmbTest, WriteRejectsFileSize) {
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-write.bin").c_str(), "", 0));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  const std::string partial(300, 'x');
  ASSERT_TRUE(files::WriteFile(OutPath("rpmb-write.bin").c_str(), partial.data(), partial.size()));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("multiple of 256"));
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("missing.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, WriteRejectsRangePastLastAddress) {
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 2, 0);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0xffff", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

// With another key, the MAC of the write counter response does not match, so nothing is written.
TEST_F(RpmbTest, WriteWithOtherKeyStopsBeforeWriting) {
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", wrong_key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1)}));
  EXPECT_EQ(0u, fake_.write_counter);
  EXPECT_EQ(BlockByte(0, 0), fake_.block(0)[0]);
}

TEST_F(RpmbTest, WriteStopsAtFirstDeviceFailure) {
  fake_.reliable_write_sector_count = 1;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 3, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "63", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("RPMB authenticated data write failed: address failure"));
  EXPECT_TRUE(ErrorContains("1 of 3 blocks were written"));
  // The device reported the failure of this write, so the outcome of the chunk is known.
  EXPECT_FALSE(ErrorContains("may or may not"));
  // The third block is not sent.
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(1), Out(1), In(1),  //
                                         Out(1), In(1), Out(1), Out(1), In(1)}));
  EXPECT_EQ(1u, fake_.write_counter);
}

// Another client writes between the write request and the result read request, so the result
// belongs to that write.
TEST_F(RpmbTest, WriteDetectsResultOfOtherWrite) {
  fake_.foreign_write_before_result_read = ForeignWrite{};
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "4", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("could not confirm"));
  EXPECT_TRUE(ErrorContains("0 of 1 blocks were written"));
  EXPECT_TRUE(ErrorContains("may or may not"));
}

// The other write goes to the same address, so only the write counter tells the results apart.
TEST_F(RpmbTest, WriteDetectsResultOfOtherWriteToSameAddress) {
  fake_.foreign_write_before_result_read = ForeignWrite{.address = 4};
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "4", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("could not confirm"));
}

// The write of the tool succeeds, and then a write of another client to the same address fails. Its
// result carries the address and the write counter that the write of the tool left.
TEST_F(RpmbTest, WriteDoesNotTakeFailureOfOtherWriteToSameAddress) {
  fake_.foreign_write_before_result_read =
      ForeignWrite{.address = 4, .result = RpmbResult::kCounterFailure};
  const std::vector<uint8_t> data = WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "4", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("could not confirm"));
  EXPECT_TRUE(ErrorContains("may or may not"));
  EXPECT_TRUE(BlocksHold(4, data));
}

// Another client writes between the write counter read and the write request, so the write of the
// tool fails with a counter failure. Its result is the same as in the test above.
TEST_F(RpmbTest, WriteCannotConfirmCounterFailure) {
  fake_.foreign_write_before_write = true;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("could not confirm"));
  EXPECT_TRUE(ErrorContains("counter failure"));
  EXPECT_TRUE(ErrorContains("may or may not"));
  EXPECT_EQ(BlockByte(0, 0), fake_.block(0)[0]);
}

// Another client reads the write counter between the write request and the result read request, so
// the result read returns the response to that request.
TEST_F(RpmbTest, WriteDetectsOtherRequestBeforeResultRead) {
  fake_.foreign_counter_read_before_result_read = true;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("response type 0x0200"));
  EXPECT_TRUE(ErrorContains("may or may not"));
}

// The device reports 0x0085 when the write counter has expired.
TEST_F(RpmbTest, WriteReportsExpiredCounterInResult) {
  fake_.write_result_raw = 0x0085;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO);
  EXPECT_TRUE(ErrorContains("write failure, write counter expired (result 0x0085)"));
}

TEST_F(RpmbTest, WriteStopsBeforeWritingWhenCounterExpired) {
  fake_.write_counter = 0xffffffff;
  fake_.extra_result_bits = kRpmbResultWriteCounterExpired;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_BAD_STATE);
  EXPECT_TRUE(ErrorContains("expired"));
  // The counter read showed that nothing was written.
  EXPECT_FALSE(ErrorContains("may or may not"));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1)}));
}

TEST_F(RpmbTest, WriteRejectsWrongResultMac) {
  fake_.corrupt_mac_type = kRpmbAuthenticatedDataWriteResponse;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("MAC"));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(1), Out(1), In(1)}));
}

TEST_F(RpmbTest, WriteRejectsWrongResultType) {
  fake_.corrupt_write_result_type = true;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_IO_DATA_INTEGRITY);
  EXPECT_TRUE(ErrorContains("response type 0x0400"));
  EXPECT_EQ(fake_.commands, (std::vector{Out(1), In(1), Out(1), Out(1), In(1)}));
}

// The fake device reports 512 KiB (2048 blocks) by default; these tests make it report the
// smallest RPMB unit, 128 KiB (512 blocks).
TEST_F(RpmbTest, WriteRejectsRangePastDeviceSize) {
  fake_.rpmb_size = 1;
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 2, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "511", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("outside the 512 blocks"));
  EXPECT_TRUE(fake_.commands.empty());
  EXPECT_EQ(0u, fake_.write_counter);
}

TEST_F(RpmbTest, WriteAcceptsRangeWithinDeviceSize) {
  fake_.rpmb_size = 1;
  const std::vector<uint8_t> data = WriteDataFile(OutPath("rpmb-write.bin").c_str(), 2, 0x40);
  ASSERT_OK(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "-s", "0", "-f",
                        OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}));
  EXPECT_TRUE(BlocksHold(0, data));
}

TEST_F(RpmbTest, ReadRejectsRangePastDeviceSize) {
  fake_.rpmb_size = 1;
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "-s", "512", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("outside the 512 blocks"));
  EXPECT_TRUE(fake_.commands.empty());
}

// A positional argument, such as a `--start` whose dashes were left out, must not be ignored.
TEST_F(RpmbTest, WriteRejectsExtraArgument) {
  WriteDataFile(OutPath("rpmb-write.bin").c_str(), 1, 0x40);
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "9", "-f",
                            OutPath("rpmb-write.bin").c_str(), "-k", key_file_.c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("unexpected argument '9'"));
  EXPECT_TRUE(fake_.commands.empty());
  EXPECT_EQ(0u, fake_.write_counter);
}

TEST_F(RpmbTest, ReadRejectsExtraArgument) {
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "3", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(ErrorContains("unexpected argument '3'"));
  EXPECT_TRUE(fake_.commands.empty());
  // Arguments after `--` are positional as well.
  EXPECT_STATUS(RunCommand({"ufsutil", kRpmbPath, "rpmb", "read", "--", "-f",
                            OutPath("rpmb-read.bin").c_str()}),
                ZX_ERR_INVALID_ARGS);
  EXPECT_TRUE(fake_.commands.empty());
}

TEST_F(RpmbTest, WriteHelpWarnsAboutPermanentWrite) {
  zx::result<std::string> output = RunCommand({"ufsutil", kRpmbPath, "rpmb", "write", "--help"});
  ASSERT_OK(output);
  EXPECT_NE(std::string::npos, output->find("WARNING"));
  EXPECT_NE(std::string::npos, output->find("TEE"));
  EXPECT_TRUE(fake_.commands.empty());
}

// The fake device rejects a write whose frames do not repeat the fields of the whole access.
TEST(RpmbFrameTest, WriteFramesAreWellFormed) {
  std::vector<RpmbFrame> frames(2);
  for (RpmbFrame& frame : frames) {
    frame.req_resp = htobe16(kRpmbAuthenticatedDataWriteRequest);
    frame.block_count = htobe16(static_cast<uint16_t>(frames.size()));
    frame.address = htobe16(4);
    frame.write_counter = htobe32(7);
  }
  EXPECT_TRUE(WriteFramesAreWellFormed(frames));
  // A frame that carries its own address instead of the start of the access.
  frames[0].address = htobe16(5);
  EXPECT_FALSE(WriteFramesAreWellFormed(frames));
  frames[0].address = htobe16(4);
  // A frame that carries its own block number instead of the total block count.
  frames[0].block_count = htobe16(1);
  EXPECT_FALSE(WriteFramesAreWellFormed(frames));
  frames[0].block_count = htobe16(static_cast<uint16_t>(frames.size()));
  // A non-last frame with a MAC.
  frames[0].key_mac[0] = 1;
  EXPECT_FALSE(WriteFramesAreWellFormed(frames));
  frames[0].key_mac[0] = 0;
  EXPECT_TRUE(WriteFramesAreWellFormed(frames));
}

}  // namespace
}  // namespace ufsutil
